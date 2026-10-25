//! prune・GitHub一覧・変更検知の試験(モックDiscord/GitHub)。

use bdg_core::backup::{backup, list_backups, prune, restore, BackupOptions};
use bdg_core::crypto::{KdfParams, KeySource};
use bdg_core::discord::{DiscordClient, RetryPolicy};
use bdg_core::gitbundle::{remote_fingerprint, GitSource};
use bdg_testkit::{Shared, State};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn client(root: &str) -> DiscordClient {
    DiscordClient::new("test-token", "123456789")
        .unwrap()
        .with_base_url(format!("{root}/api"))
        .with_max_upload_bytes(8 * 1024)
        .with_retry(RetryPolicy { max_attempts: 3, max_wait: Duration::from_secs(5), backoff_base: Duration::from_millis(10) })
}

fn opts(repo: &str) -> BackupOptions {
    BackupOptions { repo: repo.into(), kdf: KdfParams { m_cost: 8, t_cost: 1, p_cost: 1 } }
}

fn pass(p: &str) -> KeySource {
    KeySource::Passphrase(p.into())
}

fn data() -> Cursor<Vec<u8>> {
    Cursor::new((0..5000u32).map(|i| (i * 7 % 251) as u8).collect())
}

#[tokio::test]
async fn prune_keeps_newest_per_repo_and_never_touches_unreadable() {
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let root = bdg_testkit::start(state.clone()).await;
    let dc = client(&root);
    for _ in 0..3 {
        backup(data(), &pass("pw"), &dc, &opts("o/a")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await; // created_at は秒単位
    }
    backup(data(), &pass("pw"), &dc, &opts("o/b")).await.unwrap();
    // 別の鍵のバックアップ(読めない)。
    backup(data(), &pass("other-key"), &dc, &opts("o/c")).await.unwrap();
    let before = state.lock().unwrap().msgs.len();

    let dry = prune(&dc, &pass("pw"), 1, None, true).await.unwrap();
    assert_eq!(dry.deleted.len(), 2);
    assert_eq!(dry.unreadable, 1);
    assert_eq!(state.lock().unwrap().msgs.len(), before, "dry run must not delete");

    let rep = prune(&dc, &pass("pw"), 1, None, false).await.unwrap();
    assert_eq!(rep.deleted.len(), 2);
    assert_eq!(rep.kept.len(), 2); // o/a の最新1件 + o/b

    // 別の鍵のバックアップは無傷で、まだ復元できる。
    let refs = list_backups(&dc).await.unwrap();
    assert_eq!(refs.len(), 3);
    let c = refs.iter().find(|r| !rep.kept.iter().any(|k| k.1 == r.id8)).unwrap();
    let mut out = Vec::new();
    restore(&dc, &pass("other-key"), c, &mut out).await.unwrap();
    assert_eq!(out.len(), 5000);
    assert!(prune(&dc, &pass("pw"), 0, None, true).await.is_err());
}

#[tokio::test]
async fn github_listing_paginates_authenticates_and_validates() {
    use bdg_core::github::{list_repos, Owner};
    let state: Shared = Arc::new(Mutex::new(State::default()));
    state.lock().unwrap().repos = vec![
        serde_json::json!({"full_name":"o/pub","clone_url":"https://github.com/o/pub.git","private":false}),
        serde_json::json!({"full_name":"o/priv","clone_url":"https://github.com/o/priv.git","private":true,"archived":true}),
    ];
    let root = bdg_testkit::start(state.clone()).await;
    let gh = format!("{root}/github");
    let repos = list_repos(&gh, "ghp_x", &Owner::Org("o".into())).await.unwrap();
    assert_eq!(repos.len(), 2);
    assert!(repos[1].private && repos[1].archived);
    assert_eq!(list_repos(&gh, "ghp_x", &Owner::AuthenticatedUser).await.unwrap().len(), 2);
    assert!(state.lock().unwrap().github_auth.iter().all(|a| a == "Bearer ghp_x"));
    assert!(list_repos(&gh, "t", &Owner::Org("../x".into())).await.is_err());
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

#[test]
fn remote_fingerprint_changes_only_when_refs_change() {
    let src = tempfile::tempdir().unwrap();
    run_git(src.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("a"), "1").unwrap();
    run_git(src.path(), &["add", "."]);
    run_git(src.path(), &["commit", "-q", "-m", "one"]);
    let s = GitSource { url: src.path().to_string_lossy().into_owned(), token: None };
    let f1 = remote_fingerprint(&s).unwrap().unwrap();
    assert_eq!(f1, remote_fingerprint(&s).unwrap().unwrap());
    std::fs::write(src.path().join("a"), "2").unwrap();
    run_git(src.path(), &["commit", "-q", "-am", "two"]);
    assert_ne!(f1, remote_fingerprint(&s).unwrap().unwrap());
}
