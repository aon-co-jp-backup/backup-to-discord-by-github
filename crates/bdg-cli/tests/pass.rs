//! `bdg` の1回分の実行を、モックDiscord/GitHubと本物のgitで通す。

use bdg_cli::config::Config;
use bdg_cli::runner::{backup_pass, doctor, prune_pass, resolve_targets, Ctx, Status};
use bdg_testkit::{Shared, State};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn run_git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn make_repo(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", "one"]);
}

struct Env {
    ctx: Ctx,
    state: Shared,
    _dir: tempfile::TempDir,
}

async fn env(extra_github: &str, repos_toml: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let root = bdg_testkit::start(state.clone()).await;
    std::fs::write(dir.path().join("discord.txt"), "test-token\n").unwrap();
    std::fs::write(dir.path().join("pass.txt"), "correct horse\n").unwrap();
    std::fs::write(dir.path().join("gh.txt"), "ghp_test\n").unwrap();
    let toml = format!(
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
token_file = '{d}/gh.txt'
api_base = "{root}/github"
{extra_github}
repos = [{repos_toml}]
[schedule]
keep = 1
pause_secs = 0
state_file = "state.json"
"#,
        d = dir.path().display().to_string().replace('\\', "/"),
    );
    let cfg_path = dir.path().join("bdg.toml");
    std::fs::write(&cfg_path, toml).unwrap();
    let cfg = Config::load(&cfg_path).unwrap();
    Env { ctx: Ctx::from_config(cfg).unwrap(), state, _dir: dir }
}

fn quoted(p: &Path) -> String {
    format!("'{}'", p.display())
}

#[tokio::test]
async fn scheduled_pass_skips_unchanged_backs_up_changes_and_prunes() {
    let repo = tempfile::tempdir().unwrap();
    make_repo(repo.path());
    let e = env("", &quoted(repo.path())).await;
    let name = format!("{}/{}", repo.path().parent().unwrap().file_name().unwrap().to_string_lossy(), repo.path().file_name().unwrap().to_string_lossy());

    let first = backup_pass(&e.ctx, false, None).await.unwrap();
    assert_eq!(first.len(), 1);
    assert!(matches!(first[0].status, Status::BackedUp { .. }), "{:?}", first[0].status);
    assert_eq!(first[0].name, name);

    // 変更なし → 送らない(Discordの容量を食わない)。
    let msgs_after_first = e.state.lock().unwrap().msgs.len();
    let second = backup_pass(&e.ctx, false, None).await.unwrap();
    assert_eq!(second[0].status, Status::Unchanged);
    assert_eq!(e.state.lock().unwrap().msgs.len(), msgs_after_first);

    // 状態ファイルが残り、次回プロセスでも変更なしと判断できる。
    assert!(e.ctx.cfg.schedule.state_file.exists());

    // 変更あり → 送る。
    std::fs::write(repo.path().join("a.txt"), "two\n").unwrap();
    run_git(repo.path(), &["commit", "-q", "-am", "two"]);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // created_at は秒単位
    let third = backup_pass(&e.ctx, false, None).await.unwrap();
    assert!(matches!(third[0].status, Status::BackedUp { .. }));

    // --force は変更なしでも送る。
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let forced = backup_pass(&e.ctx, true, None).await.unwrap();
    assert!(matches!(forced[0].status, Status::BackedUp { .. }));

    // keep=1: 3世代のうち古い2つが消え、最新だけが残る。
    let rep = prune_pass(&e.ctx, false, None).await.unwrap();
    assert_eq!(rep.deleted.len(), 2);
    assert_eq!(rep.kept.len(), 1);
}

#[tokio::test]
async fn one_failing_repository_does_not_stop_the_others() {
    let good = tempfile::tempdir().unwrap();
    make_repo(good.path());
    let empty = tempfile::tempdir().unwrap();
    run_git(empty.path(), &["init", "-q", "-b", "main"]);
    let missing = std::env::temp_dir().join("bdg-no-such-repo-xyz");
    let specs = format!("{}, {}, {}", quoted(&missing), quoted(empty.path()), quoted(good.path()));
    // 存在しないパスは「絶対パスで実在」の条件を満たさないので設定の段階で弾かれる。
    let e = env("", &specs).await;
    let err = backup_pass(&e.ctx, false, None).await.unwrap_err();
    assert!(err.contains("unsupported repository spec"), "{err}");

    let specs = format!("{}, {}", quoted(empty.path()), quoted(good.path()));
    let e = env("", &specs).await;
    let out = backup_pass(&e.ctx, false, None).await.unwrap();
    assert_eq!(out[0].status, Status::Empty);
    assert!(matches!(out[1].status, Status::BackedUp { .. }));
}

#[tokio::test]
async fn organization_listing_filters_forks_and_dedups() {
    let a = tempfile::tempdir().unwrap();
    make_repo(a.path());
    let url = a.path().display().to_string();
    let e = env("orgs = [\"o\"]", "").await;
    e.state.lock().unwrap().repos = vec![
        serde_json::json!({"full_name":"o/a","clone_url":url,"private":true}),
        serde_json::json!({"full_name":"o/fork","clone_url":url,"fork":true}),
        serde_json::json!({"full_name":"o/a","clone_url":url}),
    ];
    let targets = resolve_targets(&e.ctx).await.unwrap();
    assert_eq!(targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["o/a"]);
}

#[tokio::test]
async fn doctor_round_trips_and_leaves_nothing_behind() {
    let repo = tempfile::tempdir().unwrap();
    make_repo(repo.path());
    let e = env("orgs = [\"o\"]", &quoted(repo.path())).await;
    e.state.lock().unwrap().repos = vec![serde_json::json!({"full_name":"o/x","clone_url":"https://github.com/o/x.git"})];
    let steps = doctor(&e.ctx).await;
    for (name, ok, detail) in &steps {
        assert!(ok, "{name}: {detail}");
    }
    assert!(steps.len() >= 5);
    assert!(e.state.lock().unwrap().msgs.is_empty(), "doctor must delete its test data");
    assert!(e.state.lock().unwrap().github_auth.iter().all(|a| a == "Bearer ghp_test"));
}

#[tokio::test]
async fn doctor_reports_a_helpful_hint_for_bad_token() {
    let repo = tempfile::tempdir().unwrap();
    make_repo(repo.path());
    let e = env("", &quoted(repo.path())).await;
    std::fs::write(e.ctx.cfg.discord.token_file.clone(), "wrong\n").unwrap();
    let ctx = Ctx::from_config(Config::load(&e._dir.path().join("bdg.toml")).unwrap()).unwrap();
    let steps = doctor(&ctx).await;
    let send = steps.iter().find(|s| s.0 == "Discordへ送信").unwrap();
    assert!(!send.1);
    assert!(send.2.contains("Botトークン"), "{}", send.2);
}

#[tokio::test]
async fn only_private_skips_public_repositories() {
    let a = tempfile::tempdir().unwrap();
    make_repo(a.path());
    let url = a.path().display().to_string();
    let e = env("include_user = true\nonly_private = true", "").await;
    e.state.lock().unwrap().repos = vec![
        serde_json::json!({"full_name":"o/priv","clone_url":url,"private":true}),
        serde_json::json!({"full_name":"o/pub","clone_url":url,"private":false}),
    ];
    let targets = resolve_targets(&e.ctx).await.unwrap();
    assert_eq!(targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["o/priv"]);
}
