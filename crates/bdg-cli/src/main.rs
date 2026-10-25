use bdg_cli::config::Config;
use bdg_cli::log;
use bdg_cli::runner::{backup_pass, doctor, prune_pass, Ctx, Status};
use bdg_core::backup::{list_backups, read_manifest, restore};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "bdg", version, about = "GitHubリポジトリを暗号化して非公開Discordへバックアップする")]
struct Cli {
    /// 設定ファイル(TOML)。
    #[arg(long, short, global = true, default_value = "bdg.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 配備前の自己診断(本物のDiscordへ小さなバックアップを往復させて消す)。
    Doctor,
    /// 1回だけバックアップする(世代整理はしない)。
    Backup {
        /// 変更が無くても送る。
        #[arg(long)]
        force: bool,
        /// この `owner/name` だけ。
        #[arg(long)]
        repo: Option<String>,
    },
    /// Discord上のバックアップを一覧する(復号してリポジトリ名・日時を表示)。
    List,
    /// バックアップを復元して git bundle ファイルに書き出す。
    Restore {
        /// `list` に出るID(先頭8文字)。
        #[arg(long)]
        id: String,
        #[arg(long)]
        out: PathBuf,
        /// 既存ファイルを上書きする。
        #[arg(long)]
        overwrite: bool,
    },
    /// 古い世代を削除する。
    Prune {
        #[arg(long)]
        keep: Option<usize>,
        /// 削除せず、対象だけ表示する。
        #[arg(long)]
        dry_run: bool,
    },
    /// バックアップ+世代整理。`--once` なら1回で終了(OSのスケジューラ向け)、
    /// 無ければ `schedule.interval_hours` ごとに繰り返す。
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start runtime: {e}");
            return ExitCode::from(2);
        }
    };
    match rt.block_on(real_main(cli)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

async fn real_main(cli: Cli) -> Result<bool, String> {
    let cfg = Config::load(&cli.config)?;
    let ctx = Ctx::from_config(cfg)?;
    match cli.cmd {
        Cmd::Doctor => {
            let steps = doctor(&ctx).await;
            let mut all = true;
            for (name, ok, detail) in &steps {
                println!("[{}] {name}: {detail}", if *ok { "OK" } else { "NG" });
                all &= ok;
            }
            println!("{}", if all { "すべて成功。定期実行を始められます。" } else { "失敗した項目があります。" });
            Ok(all)
        }
        Cmd::Backup { force, repo } => {
            let outcomes = backup_pass(&ctx, force, repo.as_deref()).await?;
            Ok(!outcomes.iter().any(|o| matches!(o.status, Status::Failed(_))))
        }
        Cmd::List => {
            let refs = list_backups(&ctx.discord).await.map_err(|e| e.to_string())?;
            let mut rows = Vec::new();
            for r in &refs {
                match read_manifest(&ctx.discord, &ctx.key, r).await {
                    Ok(m) => rows.push((m.created_at, r.id8.clone(), m.repo, format!("{} bytes, {} parts", m.plain_bytes, m.part_count))),
                    Err(_) => rows.push((0, r.id8.clone(), "(読めない: 別の鍵か破損)".into(), String::new())),
                }
            }
            rows.sort_by_key(|r| std::cmp::Reverse(r.0));
            for (ts, id, repo, info) in rows {
                let when = bdg_cli::fmt_unix(ts);
                println!("{id}  {when}  {repo}  {info}");
            }
            Ok(true)
        }
        Cmd::Restore { id, out, overwrite } => {
            if out.exists() && !overwrite {
                return Err(format!("{} already exists (use --overwrite)", out.display()));
            }
            let refs = list_backups(&ctx.discord).await.map_err(|e| e.to_string())?;
            let matches: Vec<_> = refs.iter().filter(|r| r.id8.starts_with(&id)).collect();
            let r = match matches.as_slice() {
                [one] => *one,
                [] => return Err(format!("no backup with id {id}")),
                _ => return Err(format!("id {id} is ambiguous")),
            };
            // 途中で失敗しても欠けたbundleを残さないよう、一時ファイルへ書いてから置き換える。
            let partial = out.with_extension("partial");
            let mut f = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
            let res = restore(&ctx.discord, &ctx.key, r, &mut f).await;
            drop(f);
            match res {
                Ok(m) => {
                    std::fs::rename(&partial, &out).map_err(|e| e.to_string())?;
                    log(format!("restored {} -> {}", m.repo, out.display()));
                    Ok(true)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&partial);
                    Err(e.to_string())
                }
            }
        }
        Cmd::Prune { keep, dry_run } => {
            let rep = prune_pass(&ctx, dry_run, keep).await?;
            for (repo, id) in &rep.deleted {
                println!("{} {repo} {id}", if dry_run { "would delete" } else { "deleted" });
            }
            Ok(true)
        }
        Cmd::Run { once, force } => {
            let interval = Duration::from_secs(ctx.cfg.schedule.interval_hours * 3600);
            loop {
                let ok = match backup_pass(&ctx, force, None).await {
                    Ok(outcomes) => {
                        let failed = outcomes.iter().filter(|o| matches!(o.status, Status::Failed(_))).count();
                        let pruned = prune_pass(&ctx, false, None).await;
                        if let Err(e) = &pruned {
                            log(format!("prune failed: {e}"));
                        }
                        failed == 0 && pruned.is_ok()
                    }
                    Err(e) => {
                        log(format!("pass failed: {e}"));
                        false
                    }
                };
                if once {
                    return Ok(ok);
                }
                log(format!("next run in {} hours", ctx.cfg.schedule.interval_hours));
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = tokio::signal::ctrl_c() => {
                        log("interrupted, exiting");
                        return Ok(true);
                    }
                }
            }
        }
    }
}

