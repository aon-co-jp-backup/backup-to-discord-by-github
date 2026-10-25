//! モックDiscord(RPoemで実装。ストリーミングmultipartの実利用も兼ねる)を相手にした通し試験。

use bdg_core::backup::{backup, list_backups, restore, BackupOptions};
use bdg_core::crypto::{KdfParams, KeySource};
use bdg_core::discord::{DiscordClient, RetryPolicy};
use bdg_core::gitbundle::{open_bundle, GitSource};
use bytes::Bytes;
use open_runo_router::hyper_compat::{fixed_body, json_response, query_params, serve, Handler, Response, Router};
use open_runo_router::multipart_stream::{MultipartLimits, MultipartStream};
use hyper::{Method, StatusCode};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    next_id: u64,
    msgs: Vec<(u64, String, Vec<u8>)>,
    uploads: usize,
    /// 1回目のアップロードだけ429を返す。
    rate_limit_first: bool,
    /// n回目(1始まり)のアップロードで403を返す。
    reject_upload_n: Option<usize>,
}

type Shared = Arc<Mutex<State>>;

fn authorized(req: &open_runo_router::hyper_compat::Request) -> bool {
    req.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some("Bot test-token")
}

fn host(req: &open_runo_router::hyper_compat::Request) -> String {
    req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1").to_string()
}

fn msg_json(host: &str, id: u64, name: &str, size: usize) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "attachments": [{ "filename": name, "url": format!("http://{host}/cdn/{id}/{name}"), "size": size }]
    })
}

async fn start_mock(state: Shared) -> String {
    let st = state.clone();
    let post: Handler = Arc::new(move |req, _p| {
        let st = st.clone();
        Box::pin(async move {
            if !authorized(&req) {
                return json_response(StatusCode::UNAUTHORIZED, &serde_json::json!({"message": "401: Unauthorized"}));
            }
            let h = host(&req);
            {
                let mut s = st.lock().unwrap();
                s.uploads += 1;
                if s.rate_limit_first && s.uploads == 1 {
                    return json_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        &serde_json::json!({"message": "You are being rate limited.", "retry_after": 0.05, "global": false}),
                    );
                }
                if s.reject_upload_n == Some(s.uploads) {
                    return json_response(StatusCode::FORBIDDEN, &serde_json::json!({"message": "Missing Permissions"}));
                }
            }
            let mut mp = match MultipartStream::from_request(req, MultipartLimits::default()) {
                Ok(m) => m,
                Err(r) => return r,
            };
            let Ok(Some(field)) = mp.next_field().await else {
                return json_response(StatusCode::BAD_REQUEST, &serde_json::json!({"message": "no file"}));
            };
            let name = field.filename.clone().unwrap_or_default();
            let data = mp.field_bytes(64 * 1024 * 1024).await.unwrap_or_default();
            let mut s = st.lock().unwrap();
            s.next_id += 1;
            let id = 1_000_000_000_000_000 + s.next_id;
            let body = msg_json(&h, id, &name, data.len());
            s.msgs.push((id, name, data));
            json_response(StatusCode::OK, &body)
        })
    });

    let st = state.clone();
    let list: Handler = Arc::new(move |req, _p| {
        let st = st.clone();
        Box::pin(async move {
            let h = host(&req);
            let q = query_params(&req);
            let before: u64 = q.get("before").and_then(|b| b.parse().ok()).unwrap_or(u64::MAX);
            let limit: usize = q.get("limit").and_then(|b| b.parse().ok()).unwrap_or(50);
            let s = st.lock().unwrap();
            let mut v: Vec<_> = s.msgs.iter().filter(|m| m.0 < before).collect();
            v.sort_by_key(|m| std::cmp::Reverse(m.0));
            let out: Vec<_> = v.into_iter().take(limit).map(|m| msg_json(&h, m.0, &m.1, m.2.len())).collect();
            json_response(StatusCode::OK, &out)
        })
    });

    let st = state.clone();
    let get_one: Handler = Arc::new(move |req, p| {
        let st = st.clone();
        Box::pin(async move {
            let h = host(&req);
            let id: u64 = p.get("mid").and_then(|m| m.parse().ok()).unwrap_or(0);
            let s = st.lock().unwrap();
            match s.msgs.iter().find(|m| m.0 == id) {
                Some(m) => json_response(StatusCode::OK, &msg_json(&h, m.0, &m.1, m.2.len())),
                None => json_response(StatusCode::NOT_FOUND, &serde_json::json!({"message": "Unknown Message"})),
            }
        })
    });

    let st = state.clone();
    let delete: Handler = Arc::new(move |_req, p| {
        let st = st.clone();
        Box::pin(async move {
            let id: u64 = p.get("mid").and_then(|m| m.parse().ok()).unwrap_or(0);
            st.lock().unwrap().msgs.retain(|m| m.0 != id);
            open_runo_router::hyper_compat::empty_status(StatusCode::NO_CONTENT)
        })
    });

    let st = state.clone();
    let cdn: Handler = Arc::new(move |_req, p| {
        let st = st.clone();
        Box::pin(async move {
            let id: u64 = p.get("mid").and_then(|m| m.parse().ok()).unwrap_or(0);
            let s = st.lock().unwrap();
            match s.msgs.iter().find(|m| m.0 == id) {
                Some(m) => {
                    let resp: Response = hyper::Response::builder()
                        .status(200)
                        .body(fixed_body(Bytes::from(m.2.clone())))
                        .unwrap();
                    resp
                }
                None => json_response(StatusCode::NOT_FOUND, &serde_json::json!({})),
            }
        })
    });

    let router = Router::new()
        .route(Method::POST, "/api/channels/:cid/messages", post)
        .route(Method::GET, "/api/channels/:cid/messages", list)
        .route(Method::GET, "/api/channels/:cid/messages/:mid", get_one)
        .route(Method::DELETE, "/api/channels/:cid/messages/:mid", delete)
        .route(Method::GET, "/cdn/:mid/:fname", cdn);
    let (addr, _h) = serve(router, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    format!("http://{addr}/api")
}

fn client(base: &str, max_upload: usize) -> DiscordClient {
    DiscordClient::new("test-token", "123456789")
        .unwrap()
        .with_base_url(base)
        .with_max_upload_bytes(max_upload)
        .with_retry(RetryPolicy {
            max_attempts: 4,
            max_wait: Duration::from_secs(5),
            backoff_base: Duration::from_millis(10),
        })
}

fn fast_opts(repo: &str) -> BackupOptions {
    BackupOptions { repo: repo.into(), kdf: KdfParams { m_cost: 8, t_cost: 1, p_cost: 1 } }
}

fn pass(p: &str) -> KeySource {
    KeySource::Passphrase(p.into())
}

fn sample(len: usize) -> Vec<u8> {
    // 圧縮しにくく、再現できるデータ。先頭に探しやすい目印を入れる。
    let mut v = b"PLAINTEXT-MARKER-SECRET-REPO".to_vec();
    let mut x: u32 = 12345;
    while v.len() < len {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        v.push((x >> 24) as u8);
    }
    v
}

#[tokio::test]
async fn round_trip_many_parts_with_rate_limit_and_no_plaintext_leak() {
    let state: Shared = Arc::new(Mutex::new(State { rate_limit_first: true, ..Default::default() }));
    let base = start_mock(state.clone()).await;
    let dc = client(&base, 8 * 1024);
    let data = sample(100_000);

    let manifest = backup(Cursor::new(data.clone()), &pass("correct horse"), &dc, &fast_opts("owner/private-repo"))
        .await
        .unwrap();
    assert!(manifest.part_count >= 13, "expected many parts, got {}", manifest.part_count);
    assert_eq!(manifest.plain_bytes, data.len() as u64);

    // Discordに置かれたものに、平文もリポジトリ名も含まれない。
    {
        let s = state.lock().unwrap();
        assert_eq!(s.msgs.len() as u32, manifest.part_count + 1);
        for (_, name, bytes) in &s.msgs {
            assert!(name.starts_with("bdg-") && name.ends_with(".bin"), "{name}");
            assert!(!name.contains("private-repo"));
            assert!(bytes.len() <= 8 * 1024);
            assert!(!bytes.windows(9).any(|w| w == b"PLAINTEXT"), "plaintext leaked in {name}");
            assert!(!bytes.windows(12).any(|w| w == b"private-repo"), "repo name leaked in {name}");
        }
    }

    let refs = list_backups(&dc).await.unwrap();
    assert_eq!(refs.len(), 1);
    let mut restored = Vec::new();
    let m = restore(&dc, &pass("correct horse"), &refs[0], &mut restored).await.unwrap();
    assert_eq!(m.repo, "owner/private-repo");
    assert_eq!(restored, data);

    // 鍵が違えば復元できない。
    let err = restore(&dc, &pass("wrong"), &refs[0], &mut Vec::new()).await.unwrap_err();
    assert!(matches!(err, bdg_core::Error::Decrypt(_)), "{err}");
}

#[tokio::test]
async fn tampered_part_is_detected() {
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let base = start_mock(state.clone()).await;
    let dc = client(&base, 8 * 1024);
    let data = sample(40_000);
    backup(Cursor::new(data), &pass("pw"), &dc, &fast_opts("o/r")).await.unwrap();
    let r = list_backups(&dc).await.unwrap().remove(0);

    // 1バイト改ざん → SHA-256不一致。
    {
        let mut s = state.lock().unwrap();
        let i = s.msgs.iter().position(|m| m.1.ends_with("-00001.bin")).unwrap();
        s.msgs[i].2[100] ^= 1;
    }
    let err = restore(&dc, &pass("pw"), &r, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(err, bdg_core::Error::Format(_)), "{err}");
}

#[tokio::test]
async fn failed_backup_cleans_up_uploaded_parts() {
    let state: Shared = Arc::new(Mutex::new(State { reject_upload_n: Some(4), ..Default::default() }));
    let base = start_mock(state.clone()).await;
    let dc = client(&base, 8 * 1024);
    let err = backup(Cursor::new(sample(60_000)), &pass("pw"), &dc, &fast_opts("o/r")).await.unwrap_err();
    assert!(matches!(err, bdg_core::Error::Discord { status: 403, .. }), "{err}");
    assert!(state.lock().unwrap().msgs.is_empty(), "uploaded parts must be deleted after a failure");
}

#[tokio::test]
async fn bad_token_is_reported_not_retried_forever() {
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let base = start_mock(state).await;
    let dc = DiscordClient::new("wrong", "123456789").unwrap().with_base_url(&base);
    let err = backup(Cursor::new(sample(100)), &pass("pw"), &dc, &fast_opts("o/r")).await.unwrap_err();
    assert!(matches!(err, bdg_core::Error::Discord { status: 401, .. }), "{err}");
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test]
async fn real_git_repository_round_trip() {
    let src = tempfile::tempdir().unwrap();
    run_git(src.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("a.txt"), "first\n").unwrap();
    run_git(src.path(), &["add", "."]);
    run_git(src.path(), &["commit", "-q", "-m", "one"]);
    std::fs::write(src.path().join("blob.bin"), sample(200_000)).unwrap();
    run_git(src.path(), &["add", "."]);
    run_git(src.path(), &["commit", "-q", "-m", "two"]);
    run_git(src.path(), &["tag", "v1"]);

    let work = tempfile::tempdir().unwrap();
    let reader = open_bundle(
        &GitSource { url: src.path().to_string_lossy().into_owned(), token: None },
        work.path(),
    )
    .unwrap();

    let state: Shared = Arc::new(Mutex::new(State::default()));
    let base = start_mock(state).await;
    let dc = client(&base, 64 * 1024);
    backup(reader, &pass("pw"), &dc, &fast_opts("o/git")).await.unwrap();

    let r = list_backups(&dc).await.unwrap().remove(0);
    let out = tempfile::tempdir().unwrap();
    let bundle = out.path().join("restored.bundle");
    let mut f = std::fs::File::create(&bundle).unwrap();
    restore(&dc, &pass("pw"), &r, &mut f).await.unwrap();
    drop(f);

    // 復元したbundleが本物のgitで検証・クローンできる。
    run_git(out.path(), &["clone", "-q", bundle.to_str().unwrap(), "clone"]);
    run_git(&out.path().join("clone"), &["bundle", "verify", bundle.to_str().unwrap()]);
    let log = std::process::Command::new("git")
        .current_dir(out.path().join("clone"))
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "two\none");
    assert!(out.path().join("clone").join("blob.bin").exists());
}
