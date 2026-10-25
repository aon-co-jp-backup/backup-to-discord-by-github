use bdg_cli::config::{read_secret, Config};
use bdg_cli::log;
use bdg_cli::runner::Ctx;
use bdg_server::{build, spawn_background, Settings};
use clap::Parser;
use open_runo_router::hyper_compat::serve;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "bdg-server", version, about = "bdgの管理APIとGitHub Webhook受信")]
struct Cli {
    /// 設定ファイル(TOML)。`[server]` に admin_token_file が必須。
    #[arg(long, short, default_value = "bdg.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let cfg = Config::load(&cli.config)?;
    let s = &cfg.server;
    let token_file = s.admin_token_file.clone().ok_or("server.admin_token_file is required")?;
    let admin_token = read_secret(&token_file, "admin token")?;
    if admin_token.len() < 24 {
        return Err("the admin token must be at least 24 characters (use a long random string)".into());
    }
    let webhook_secret = match &s.webhook_secret_file {
        Some(p) => Some(read_secret(p, "webhook secret")?),
        None => None,
    };
    let addr: SocketAddr = s.listen.parse().map_err(|e| format!("invalid server.listen {:?}: {e}", s.listen))?;
    let settings = Settings {
        admin_token,
        webhook_secret,
        webhook_delay: Duration::from_secs(s.webhook_delay_secs),
        api_rate_per_min: s.api_rate_per_min,
        run_schedule: s.run_schedule,
    };
    let has_webhook = settings.webhook_secret.is_some();
    let ctx = Arc::new(Ctx::from_config(cfg)?);
    let (router, app) = build(ctx, settings);
    let _background = spawn_background(&app);
    let (bound, server) = serve(router, addr).await.map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    log(format!("listening on http://{bound} (webhook {})", if has_webhook { "enabled" } else { "disabled" }));
    if !bound.ip().is_loopback() {
        log("warning: not a loopback address and there is no TLS; put this behind a TLS reverse proxy");
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log("interrupted, exiting"),
        _ = server => log("server stopped"),
    }
    Ok(())
}
