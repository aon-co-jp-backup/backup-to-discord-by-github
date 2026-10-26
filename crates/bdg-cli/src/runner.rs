//! 1回分の実行: 対象の解決 → 変更検知 → バックアップ → 世代整理。
//! 1つのリポジトリの失敗で全体を止めない(失敗は結果に記録して続ける)。

use crate::config::{read_secret, Config};
use crate::log;
use crate::state::{RepoState, State};
use bdg_core::backup::{backup, delete_backup, list_backups, prune, read_manifest, restore, BackupOptions, PruneReport};
use bdg_core::crypto::KeySource;
use bdg_core::discord::DiscordClient;
use bdg_core::github::{list_repos, Owner, DEFAULT_API_BASE};
use bdg_core::gitbundle::{open_bundle, remote_fingerprint, GitSource};
use std::io::Cursor;
use std::time::Duration;

pub struct Ctx {
    pub cfg: Config,
    pub discord: DiscordClient,
    pub key: KeySource,
    pub gh_token: Option<String>,
}

impl Ctx {
    pub fn from_config(cfg: Config) -> Result<Ctx, String> {
        let token = read_secret(&cfg.discord.token_file, "discord bot token")?;
        let mut discord = DiscordClient::new(token, cfg.discord.channel_id.clone())
            .map_err(|e| e.to_string())?
            .with_max_upload_bytes(cfg.discord.max_upload_mib as usize * 1024 * 1024);
        if let Some(base) = &cfg.discord.api_base {
            discord = discord.with_base_url(base.clone());
        }
        let key = cfg.key_source()?;
        let gh_token = match &cfg.github.token_file {
            Some(p) => Some(read_secret(p, "github token")?),
            None => None,
        };
        Ok(Ctx { cfg, discord, key, gh_token })
    }

    fn opts(&self, repo: &str) -> BackupOptions {
        BackupOptions { repo: repo.to_string(), kdf: self.cfg.kdf() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `owner/name`(マニフェストに書く名前、状態ファイルのキー)。
    pub name: String,
    pub url: String,
}

fn valid_part(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// 設定の `repos` 1件を対象にする。`owner/name` か、cloneのURL/ローカルパス。
pub fn parse_repo_spec(spec: &str) -> Result<Target, String> {
    let spec = spec.trim();
    if let Some((owner, name)) = spec.split_once('/') {
        if valid_part(owner) && valid_part(name) && !name.contains('/') && !spec.contains("://") {
            let name = name.strip_suffix(".git").unwrap_or(name);
            return Ok(Target {
                name: format!("{owner}/{name}"),
                url: format!("https://github.com/{owner}/{name}.git"),
            });
        }
    }
    let is_url = spec.starts_with("https://") || spec.starts_with("ssh://") || spec.starts_with("file://");
    let is_local = std::path::Path::new(spec).is_absolute() && std::path::Path::new(spec).exists();
    if !(is_url || is_local) {
        return Err(format!("unsupported repository spec {spec:?} (use owner/name, an https/ssh URL, or an absolute local path)"));
    }
    let trimmed = spec.trim_end_matches('/').trim_end_matches(".git");
    let mut it = trimmed.rsplit(['/', '\\']);
    let name = it.next().unwrap_or("repo");
    let owner = it.next().unwrap_or("local");
    Ok(Target { name: format!("{owner}/{name}"), url: spec.to_string() })
}

pub async fn resolve_targets(ctx: &Ctx) -> Result<Vec<Target>, String> {
    let g = &ctx.cfg.github;
    let api = g.api_base.clone().unwrap_or_else(|| DEFAULT_API_BASE.to_string());
    let mut out: Vec<Target> = Vec::new();
    let mut push = |t: Target| {
        if !out.iter().any(|x| x.name == t.name) {
            out.push(t);
        }
    };
    let mut owners: Vec<Owner> = g.orgs.iter().map(|o| Owner::Org(o.clone())).collect();
    if g.include_user {
        owners.push(Owner::AuthenticatedUser);
    }
    for owner in owners {
        let token = ctx.gh_token.as_deref().ok_or("github token is required to list repositories")?;
        let repos = list_repos(&api, token, &owner).await.map_err(|e| e.to_string())?;
        for r in repos {
            if (r.fork && !g.include_forks) || (r.archived && !g.include_archived) || (g.only_private && !r.private) {
                continue;
            }
            push(Target { name: r.full_name, url: r.clone_url });
        }
    }
    for spec in &g.repos {
        push(parse_repo_spec(spec)?);
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    BackedUp { id8: String, parts: u32, plain_bytes: u64 },
    Unchanged,
    Empty,
    Failed(String),
}

#[derive(Debug)]
pub struct Outcome {
    pub name: String,
    pub status: Status,
}

/// GitHubのトークンはGitHub宛てにだけ付ける(他のホストへ漏らさない)。
fn source_for(t: &Target, token: &Option<String>) -> GitSource {
    let token = token.clone().filter(|_| t.url.starts_with("https://github.com/"));
    GitSource { url: t.url.clone(), token }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("task failed: {e}"))
}

async fn backup_one(ctx: &Ctx, t: &Target, state: &mut State, force: bool) -> Status {
    let source = source_for(t, &ctx.gh_token);
    // 指紋は取得の前に採る: 取得中にpushされても、次回は変更ありとして再実行される。
    let fp_source = source.clone();
    let fp = match blocking(move || remote_fingerprint(&fp_source)).await {
        Ok(Ok(Some(fp))) => fp,
        Ok(Ok(None)) => return Status::Empty,
        Ok(Err(e)) => return Status::Failed(e.to_string()),
        Err(e) => return Status::Failed(e),
    };
    if ctx.cfg.schedule.only_if_changed && !force && state.repos.get(&t.name).is_some_and(|s| s.fingerprint == fp) {
        return Status::Unchanged;
    }
    let tmp = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return Status::Failed(format!("cannot create temp dir: {e}")),
    };
    let dir = tmp.path().to_path_buf();
    let reader = match blocking(move || open_bundle(&source, &dir)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Status::Failed(e.to_string()),
        Err(e) => return Status::Failed(e),
    };
    match backup(reader, &ctx.key, &ctx.discord, &ctx.opts(&t.name)).await {
        Ok(m) => {
            let id8 = m.backup_id[..8].to_string();
            state.repos.insert(
                t.name.clone(),
                RepoState { fingerprint: fp, last_backup_at: m.created_at, last_backup_id8: id8.clone() },
            );
            Status::BackedUp { id8, parts: m.part_count, plain_bytes: m.plain_bytes }
        }
        Err(e) => Status::Failed(e.to_string()),
    }
}

/// 全対象を順に(1つずつ)バックアップする。`only` を渡すとその名前だけ。
pub async fn backup_pass(ctx: &Ctx, force: bool, only: Option<&str>) -> Result<Vec<Outcome>, String> {
    let mut targets = resolve_targets(ctx).await?;
    if let Some(name) = only {
        targets.retain(|t| t.name == name);
        if targets.is_empty() {
            return Err(format!("{name} is not in the backup targets"));
        }
    }
    log(format!("{} repositories to check", targets.len()));
    let state_path = ctx.cfg.schedule.state_file.clone();
    let mut state = State::load(&state_path);
    let mut outcomes = Vec::new();
    let n = targets.len();
    for (i, t) in targets.iter().enumerate() {
        let status = backup_one(ctx, t, &mut state, force).await;
        match &status {
            Status::BackedUp { id8, parts, plain_bytes } => {
                log(format!("backed up {} ({plain_bytes} bytes, {parts} parts, id {id8})", t.name))
            }
            Status::Unchanged => log(format!("unchanged, skipped: {}", t.name)),
            Status::Empty => log(format!("empty repository, skipped: {}", t.name)),
            Status::Failed(e) => log(format!("FAILED {}: {e}", t.name)),
        }
        if matches!(status, Status::BackedUp { .. }) {
            if let Err(e) = state.save(&state_path) {
                log(format!("warning: cannot save state file: {e}"));
            }
        }
        let backed = matches!(status, Status::BackedUp { .. });
        outcomes.push(Outcome { name: t.name.clone(), status });
        if backed && i + 1 < n {
            tokio::time::sleep(Duration::from_secs(ctx.cfg.schedule.pause_secs)).await;
        }
    }
    Ok(outcomes)
}

pub async fn prune_pass(ctx: &Ctx, dry_run: bool, keep: Option<usize>) -> Result<PruneReport, String> {
    let keep = keep.unwrap_or(ctx.cfg.schedule.keep);
    let r = prune(&ctx.discord, &ctx.key, keep, None, dry_run).await.map_err(|e| e.to_string())?;
    log(format!(
        "prune (keep {keep}{}): {} kept, {} deleted, {} unreadable (left untouched)",
        if dry_run { ", dry run" } else { "" },
        r.kept.len(),
        r.deleted.len(),
        r.unreadable
    ));
    Ok(r)
}

/// 配備前の自己診断。本物のDiscordに小さなバックアップを往復させて消す。
/// 各段階の成否を返す(`(名前, 成功したか, 詳細)`)。
pub async fn doctor(ctx: &Ctx) -> Vec<(String, bool, String)> {
    let mut steps: Vec<(String, bool, String)> = Vec::new();
    let mut add = |name: &str, ok: bool, detail: String| steps.push((name.to_string(), ok, detail));

    add("設定・秘密ファイルの読み込み", true, "ok".into());

    match std::process::Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => add("git", true, String::from_utf8_lossy(&o.stdout).trim().to_string()),
        _ => add("git", false, "git コマンドが見つからない".into()),
    }

    let plain: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let name = format!("bdg-doctor/{}", bdg_core_hex8());
    match backup(Cursor::new(plain.clone()), &ctx.key, &ctx.discord, &ctx.opts(&name)).await {
        Err(e) => add("Discordへ送信", false, with_hint(&e.to_string())),
        Ok(m) => {
            add("Discordへ送信", true, format!("{} 個のファイルを投稿", m.part_count + 1));
            let r = list_backups(&ctx.discord).await.map(|v| v.into_iter().find(|r| m.backup_id.starts_with(&r.id8)));
            match r {
                Ok(Some(r)) => {
                    add("履歴の読み取り(メッセージ履歴を読む権限)", true, "ok".into());
                    let mut out = Vec::new();
                    match restore(&ctx.discord, &ctx.key, &r, &mut out).await {
                        Ok(_) if out == plain => add("復元と照合", true, "送信した内容と一致".into()),
                        Ok(_) => add("復元と照合", false, "復元結果が一致しない".into()),
                        Err(e) => add("復元と照合", false, e.to_string()),
                    }
                    match read_manifest(&ctx.discord, &ctx.key, &r).await {
                        Ok(mf) => match delete_backup(&ctx.discord, &mf, &r).await {
                            Ok(()) => add("削除(自分の投稿の削除)", true, "試験データを削除".into()),
                            Err(e) => add("削除(自分の投稿の削除)", false, with_hint(&e.to_string())),
                        },
                        Err(e) => add("削除(自分の投稿の削除)", false, e.to_string()),
                    }
                }
                Ok(None) => add("履歴の読み取り(メッセージ履歴を読む権限)", false, "送ったメッセージが一覧に出ない".into()),
                Err(e) => add("履歴の読み取り(メッセージ履歴を読む権限)", false, with_hint(&e.to_string())),
            }
        }
    }

    if ctx.gh_token.is_some() {
        match resolve_targets(ctx).await {
            Ok(t) => add("GitHubの対象一覧", true, format!("{} 件", t.len())),
            Err(e) => add("GitHubの対象一覧", false, e),
        }
    }
    steps
}

fn bdg_core_hex8() -> String {
    // 乱数源はcoreの暗号用と同じ。
    let b: [u8; 4] = bdg_core::crypto::random_bytes();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn with_hint(msg: &str) -> String {
    let hint = if msg.contains(" 401") {
        "Botトークンが違う/無効"
    } else if msg.contains(" 403") {
        "Botにこのチャンネルの権限が無い(メッセージ送信・ファイル添付・履歴閲覧)"
    } else if msg.contains(" 404") {
        "チャンネルIDが違う、またはBotがそのチャンネルを見られない"
    } else {
        return msg.to_string();
    };
    format!("{msg} → {hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_specs() {
        let t = parse_repo_spec("aon-co-jp/RPoem").unwrap();
        assert_eq!(t.name, "aon-co-jp/RPoem");
        assert_eq!(t.url, "https://github.com/aon-co-jp/RPoem.git");
        let t = parse_repo_spec("o/r.git").unwrap();
        assert_eq!(t.name, "o/r");
        let t = parse_repo_spec("https://gitlab.com/g/p.git").unwrap();
        assert_eq!(t.name, "g/p");
        assert!(parse_repo_spec("--upload-pack=x").is_err());
        assert!(parse_repo_spec("../x/y").is_err());
        assert!(parse_repo_spec("ext::sh -c evil").is_err());
        assert!(parse_repo_spec("relative/path").is_err() || parse_repo_spec("relative/path").unwrap().url.starts_with("https://github.com/"));
    }

    #[test]
    fn github_token_is_only_attached_to_github() {
        let tok = Some("ghp_x".to_string());
        let gh = Target { name: "o/r".into(), url: "https://github.com/o/r.git".into() };
        let other = Target { name: "o/r".into(), url: "https://example.com/o/r.git".into() };
        assert!(source_for(&gh, &tok).token.is_some());
        assert!(source_for(&other, &tok).token.is_none());
    }
}
