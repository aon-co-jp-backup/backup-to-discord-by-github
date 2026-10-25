//! 管理APIとWebhook受信を、実TCP・モックDiscord/GitHub・本物のgitで通す。

use bdg_cli::config::Config;
use bdg_cli::runner::Ctx;
use bdg_server::{build, spawn_background, Settings};
use bdg_testkit::{Shared, State};
use hmac::{Hmac, Mac};
use open_runo_router::hyper_compat::serve;
use serde_json::{json, Value};
use sha2::Sha256;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ADMIN: &str = "admin-token-admin-token-123456";
const WH_SECRET: &str = "webhook-secret-xyz";

fn run_git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct Env {
    base: String,
    mock: Shared,
    repo_name: String,
    http: reqwest::Client,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn start(webhook: bool, rate: u32) -> Env {
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("a.txt"), "one\n").unwrap();
    run_git(repo.path(), &["add", "."]);
    run_git(repo.path(), &["commit", "-q", "-m", "one"]);
    let repo_name = format!(
        "{}/{}",
        repo.path().parent().unwrap().file_name().unwrap().to_string_lossy(),
        repo.path().file_name().unwrap().to_string_lossy()
    );

    let dir = tempfile::tempdir().unwrap();
    let mock: Shared = Arc::new(Mutex::new(State::default()));
    let root = bdg_testkit::start(mock.clone()).await;
    let d = dir.path().display().to_string().replace('\\', "/");
    std::fs::write(dir.path().join("discord.txt"), "test-token\n").unwrap();
    std::fs::write(dir.path().join("pass.txt"), "correct horse\n").unwrap();
    std::fs::write(
        dir.path().join("bdg.toml"),
        format!(
            r#"
[discord]
channel_id = "123456789"
token_file = '{d}/discord.txt'
max_upload_mib = 1
api_base = "{root}/api"
[crypto]
passphrase_file = '{d}/pass.txt'
kdf_memory_mib = 19
[github]
repos = ['{repo}']
[schedule]
keep = 1
pause_secs = 0
state_file = "state.json"
"#,
            repo = repo.path().display()
        ),
    )
    .unwrap();
    let ctx = Arc::new(Ctx::from_config(Config::load(&dir.path().join("bdg.toml")).unwrap()).unwrap());
    let settings = Settings {
        admin_token: ADMIN.into(),
        webhook_secret: webhook.then(|| WH_SECRET.to_string()),
        webhook_delay: Duration::from_millis(200),
        api_rate_per_min: rate,
        run_schedule: false,
    };
    let (router, app) = build(ctx, settings);
    std::mem::forget(spawn_background(&app));
    let (addr, _h) = serve(router, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    Env { base: format!("http://{addr}"), mock, repo_name, http: reqwest::Client::new(), _dirs: (repo, dir) }
}

impl Env {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.base)).bearer_auth(ADMIN)
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(format!("{}{path}", self.base)).bearer_auth(ADMIN)
    }
    async fn wait_job(&self, id: u64) -> Value {
        for _ in 0..200 {
            let j: Value = self.get(&format!("/api/jobs/{id}")).send().await.unwrap().json().await.unwrap();
            if matches!(j["status"].as_str(), Some("succeeded") | Some("failed")) {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("job {id} did not finish");
    }
}

fn sign(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(WH_SECRET.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn send_hook(env: &Env, event: &str, delivery: &str, body: &Value) -> reqwest::Response {
    let bytes = serde_json::to_vec(body).unwrap();
    env.http
        .post(format!("{}/webhook/github", env.base))
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", sign(&bytes))
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn admin_api_requires_the_token() {
    let e = start(false, 1000).await;
    // 認証不要なのはヘルスチェックだけ。
    assert_eq!(e.http.get(format!("{}/healthz", e.base)).send().await.unwrap().status(), 200);
    for req in [
        e.http.get(format!("{}/api/jobs", e.base)),
        e.http.get(format!("{}/api/jobs", e.base)).bearer_auth("wrong-token-wrong-token-123456"),
        e.http.get(format!("{}/api/jobs", e.base)).header("authorization", "Basic abc"),
        e.http.post(format!("{}/api/backup", e.base)),
    ] {
        assert_eq!(req.send().await.unwrap().status(), 401);
    }
    assert_eq!(e.get("/api/jobs").send().await.unwrap().status(), 200);
    // Webhookを無効にした場合は、ルート自体が無い。
    let r = e.http.post(format!("{}/webhook/github", e.base)).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn manual_backup_runs_and_shows_up_in_the_listing() {
    let e = start(false, 1000).await;
    let t: Value = e.get("/api/targets").send().await.unwrap().json().await.unwrap();
    assert_eq!(t["targets"][0], e.repo_name.as_str());

    // 対象外・不正な指定は拒否。
    let r = e.post("/api/backup").json(&json!({"repo": "x/not-a-target"})).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let r = e.post("/api/backup").json(&json!({"bogus": 1})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = e.post("/api/backup").body("{not json").send().await.unwrap();
    assert_eq!(r.status(), 400);

    // 大文字小文字違いのリポジトリ名も受け付ける(GitHubの名前は大小を区別しない)。
    let r = e.post("/api/backup").json(&json!({"repo": e.repo_name.to_uppercase()})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let id = r.json::<Value>().await.unwrap()["job_id"].as_u64().unwrap();
    assert_eq!(e.wait_job(id).await["status"], "succeeded");

    // 変更が無くても送らせる(force)。
    let r = e.post("/api/backup").json(&json!({"repo": e.repo_name, "force": true})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let id = r.json::<Value>().await.unwrap()["job_id"].as_u64().unwrap();
    let job = e.wait_job(id).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert!(job["messages"][0].as_str().unwrap().contains("backed up"));

    let list: Value = e.get("/api/backups").send().await.unwrap().json().await.unwrap();
    assert_eq!(list["backups"][0]["repo"], e.repo_name.as_str());
    assert_eq!(list["unreadable"], 0);
    assert!(!e.mock.lock().unwrap().msgs.is_empty());

    // 世代整理(dry run)はジョブとして実行され、何も消さない。
    let before = e.mock.lock().unwrap().msgs.len();
    let r = e.post("/api/prune").json(&json!({"dry_run": true})).send().await.unwrap();
    let id = r.json::<Value>().await.unwrap()["job_id"].as_u64().unwrap();
    assert_eq!(e.wait_job(id).await["status"], "succeeded");
    assert_eq!(e.mock.lock().unwrap().msgs.len(), before);
    let r = e.post("/api/prune").json(&json!({"keep": 0})).send().await.unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn github_webhook_verifies_filters_dedups_and_coalesces() {
    let e = start(true, 1000).await;
    let push = json!({"ref": "refs/heads/main", "repository": {"full_name": e.repo_name}});

    // 署名なし・不正は401。
    let r = e.http.post(format!("{}/webhook/github", e.base)).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = e
        .http
        .post(format!("{}/webhook/github", e.base))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", format!("sha256={}", "0".repeat(64)))
        .body(serde_json::to_vec(&push).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // ping。
    assert_eq!(send_hook(&e, "ping", "d-ping", &json!({"zen": "x"})).await.status(), 200);
    // 未対応イベントは無視。
    let r = send_hook(&e, "issues", "d-issue", &json!({})).await;
    assert_eq!(r.status(), 202);
    assert!(r.json::<Value>().await.unwrap()["ignored"].is_string());
    // 対象外のリポジトリは無視(ジョブを作らない)。
    let r = send_hook(&e, "push", "d-other", &json!({"repository": {"full_name": "someone/else"}})).await;
    assert_eq!(r.status(), 202);
    assert_eq!(r.json::<Value>().await.unwrap()["ignored"], "not a backup target");
    let jobs: Value = e.get("/api/jobs").send().await.unwrap().json().await.unwrap();
    assert!(jobs["jobs"].as_array().unwrap().is_empty());

    // 連続push → 1つのジョブにまとまる。
    let a = send_hook(&e, "push", "d-1", &push).await;
    assert_eq!(a.status(), 202);
    let a: Value = a.json().await.unwrap();
    let b: Value = send_hook(&e, "push", "d-2", &push).await.json().await.unwrap();
    assert_eq!(a["job_id"], b["job_id"]);
    assert_eq!(b["coalesced"], true);

    // 同じ配送IDの再送は409。
    assert_eq!(send_hook(&e, "push", "d-1", &push).await.status(), 409);

    // デバウンス後に実行される。
    let job = e.wait_job(a["job_id"].as_u64().unwrap()).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["source"], "webhook");
    assert!(!e.mock.lock().unwrap().msgs.is_empty());

    // 変更が無いまま再度pushが来ても、Discordには送らない。
    let before = e.mock.lock().unwrap().msgs.len();
    let c: Value = send_hook(&e, "push", "d-3", &push).await.json().await.unwrap();
    let job = e.wait_job(c["job_id"].as_u64().unwrap()).await;
    assert!(job["messages"][0].as_str().unwrap().contains("unchanged"), "{job}");
    assert_eq!(e.mock.lock().unwrap().msgs.len(), before);
}

#[tokio::test]
async fn api_is_rate_limited_and_body_size_is_capped() {
    let e = start(false, 4).await;
    let mut statuses = Vec::new();
    for _ in 0..6 {
        statuses.push(e.get("/api/jobs").send().await.unwrap().status().as_u16());
    }
    assert!(statuses[..4].iter().all(|s| *s == 200), "{statuses:?}");
    assert!(statuses[4..].iter().all(|s| *s == 429), "{statuses:?}");

    let e = start(false, 1000).await;
    let r = e.post("/api/backup").body(vec![b' '; 20 * 1024]).send().await.unwrap();
    assert_eq!(r.status(), 413);
}
