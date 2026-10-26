//! 管理APIとGitHub Webhook受信(RPoem)。バックアップ本体は `bdg-cli` の
//! `runner` を使い、ここは「受けて、積んで、1つずつ実行する」だけ。
//!
//! 守っていること:
//! - 管理APIは Bearer トークン必須(定数時間比較)。クライアント単位のレート制限と
//!   本文サイズ上限つき。
//! - Webhook は HMAC-SHA256 署名を検証してから中身を見る。同じ配送IDの再送は拒否。
//!   バックアップ対象に無いリポジトリのイベントは無視する。
//! - 実行は常に1つずつ。連続pushは1回にまとめる。
//! - 応答に秘密・内部エラーの詳細を出さない。
//! - 復元(リポジトリの中身を返す操作)はHTTPには出さない。CLIだけ。

#![allow(clippy::result_large_err)]

pub mod queue;

use bdg_cli::runner::{backup_pass, prune_pass, resolve_targets, Ctx, Status};
use open_runo_router::body_limit::{read_body_limited, with_content_length_limit};
use open_runo_router::hyper_compat::{json_response, query_params, Handler, Params, Request, Response, Router};
use open_runo_router::middleware_hyper::{build_rate_limiter, with_rate_limit_by, RateLimitKeyFn};
use open_runo_router::webhook::{webhook_handler, VerifiedWebhook, WebhookConfig};
use open_runo_security::RateLimit;
use queue::{Action, EnqueueError, Queue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

use hyper::{Method, StatusCode};

const API_BODY_LIMIT: usize = 16 * 1024;
const WEBHOOK_BODY_LIMIT: usize = 5 * 1024 * 1024;
const TARGETS_TTL: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct Settings {
    pub admin_token: String,
    /// `None` なら Webhook 受信は無効(ルート自体を作らない)。
    pub webhook_secret: Option<String>,
    pub webhook_delay: Duration,
    pub api_rate_per_min: u32,
    pub run_schedule: bool,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings").field("webhook", &self.webhook_secret.is_some()).finish_non_exhaustive()
    }
}

pub struct App {
    pub ctx: Arc<Ctx>,
    pub queue: Arc<Queue>,
    settings: Settings,
    targets: Mutex<Option<(Instant, Vec<String>)>>,
    /// 一覧(全マニフェストの復号=重い)を同時に1つだけにする。
    list_gate: tokio::sync::Semaphore,
}

fn json(status: StatusCode, v: serde_json::Value) -> Response {
    json_response(status, &v)
}

fn err(status: StatusCode, msg: &str) -> Response {
    json(status, serde_json::json!({ "error": msg }))
}

impl App {
    /// バックアップ対象の名前一覧(10分キャッシュ)。GitHubの一覧取得を毎回しない。
    async fn target_names(&self) -> Result<Vec<String>, String> {
        if let Some((at, names)) = self.targets.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            if at.elapsed() < TARGETS_TTL {
                return Ok(names.clone());
            }
        }
        let names: Vec<String> = resolve_targets(&self.ctx).await?.into_iter().map(|t| t.name).collect();
        *self.targets.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), names.clone()));
        Ok(names)
    }

    /// 大文字小文字を無視して対象の正式な名前を探す(GitHubの名前は大小を区別しない)。
    async fn canonical_target(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.target_names().await?.into_iter().find(|t| t.eq_ignore_ascii_case(name)))
    }
}

fn enqueue_response(r: Result<(u64, bool), EnqueueError>) -> Response {
    match r {
        Ok((id, coalesced)) => json(StatusCode::ACCEPTED, serde_json::json!({ "job_id": id, "coalesced": coalesced })),
        Err(EnqueueError::Full) => err(StatusCode::SERVICE_UNAVAILABLE, "job queue is full"),
    }
}

/// ハンドラを `Arc<App>` つきで作る。
fn handler<F, Fut>(app: &Arc<App>, f: F) -> Handler
where
    F: Fn(Arc<App>, Request, Params) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    let app = Arc::clone(app);
    let f = Arc::new(f);
    Arc::new(move |req, params| {
        let fut = f(Arc::clone(&app), req, params);
        Box::pin(fut)
    })
}

fn bearer_token(req: &Request) -> Option<&str> {
    let v = req.headers().get("authorization")?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
}

/// Bearer トークンを定数時間で検証する。長さの違いが漏れないよう、
/// 両方をSHA-256にしてから比べる。
fn with_admin_auth(inner: Handler, expected: [u8; 32]) -> Handler {
    Arc::new(move |req, params| {
        let ok = bearer_token(&req)
            .map(|t| {
                let got: [u8; 32] = Sha256::digest(t.as_bytes()).into();
                got.ct_eq(&expected).into()
            })
            .unwrap_or(false);
        let inner = Arc::clone(&inner);
        Box::pin(async move {
            if !ok {
                let mut r = err(StatusCode::UNAUTHORIZED, "missing or invalid token");
                r.headers_mut().insert("www-authenticate", hyper::header::HeaderValue::from_static("Bearer"));
                return r;
            }
            inner(req, params).await
        })
    })
}

/// レート制限のキー: プロキシが付けた最初のクライアントIP、無ければ共有の1バケット。
/// (直接公開しない前提。信頼できるプロキシの背後でだけ意味がある。)
fn client_key(req: &Request) -> String {
    let h = req.headers();
    h.get("x-forwarded-for")
        .or_else(|| h.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .map(|v| format!("ip:{}", v.split(',').next().unwrap_or(v).trim()))
        .unwrap_or_else(|| "anonymous".to_string())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BackupBody {
    repo: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PruneBody {
    keep: Option<usize>,
    #[serde(default)]
    dry_run: bool,
}

/// 空の本文は `T::default()`、それ以外はJSONとして読む。
async fn read_optional_json<T: for<'de> Deserialize<'de> + Default>(req: Request) -> Result<T, Response> {
    let bytes = read_body_limited(req, API_BODY_LIMIT).await?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid JSON body"))
}

pub fn build(ctx: Arc<Ctx>, settings: Settings) -> (Router, Arc<App>) {
    let app = Arc::new(App {
        ctx,
        queue: Arc::new(Queue::new()),
        settings: settings.clone(),
        targets: Mutex::new(None),
        list_gate: tokio::sync::Semaphore::new(1),
    });
    let token_hash: [u8; 32] = Sha256::digest(settings.admin_token.as_bytes()).into();
    let limiter: Arc<dyn RateLimit> = build_rate_limiter(settings.api_rate_per_min, 60);
    let key_fn: RateLimitKeyFn = Arc::new(client_key);

    // 管理API: 外側から レート制限 → サイズ上限 → 認証 → 本体。
    let api = |h: Handler| -> Handler {
        with_rate_limit_by(
            with_content_length_limit(with_admin_auth(h, token_hash), API_BODY_LIMIT as u64),
            Arc::clone(&limiter),
            Arc::clone(&key_fn),
        )
    };

    let health = handler(&app, |app, _req, _p| async move {
        let (queued, running) = app.queue.counts();
        json(StatusCode::OK, serde_json::json!({ "status": "ok", "queued": queued, "running": running }))
    });

    let jobs = handler(&app, |app, req, _p| async move {
        let limit = query_params(&req).get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).clamp(1, 200);
        json(StatusCode::OK, serde_json::json!({ "jobs": app.queue.list(limit) }))
    });

    let job = handler(&app, |app, _req, p| async move {
        match p.get("id").and_then(|i| i.parse::<u64>().ok()).and_then(|id| app.queue.get(id)) {
            Some(j) => json(StatusCode::OK, serde_json::to_value(j).unwrap_or_default()),
            None => err(StatusCode::NOT_FOUND, "no such job"),
        }
    });

    let targets = handler(&app, |app, _req, _p| async move {
        match app.target_names().await {
            Ok(names) => json(StatusCode::OK, serde_json::json!({ "targets": names })),
            Err(e) => {
                bdg_cli::log(format!("target listing failed: {e}"));
                err(StatusCode::BAD_GATEWAY, "cannot list backup targets")
            }
        }
    });

    let start_backup = handler(&app, |app, req, _p| async move {
        let body: BackupBody = match read_optional_json(req).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let repo = match body.repo {
            None => None,
            Some(name) => match app.canonical_target(&name).await {
                Ok(Some(canon)) => Some(canon),
                Ok(None) => return err(StatusCode::NOT_FOUND, "not a backup target"),
                Err(_) => return err(StatusCode::BAD_GATEWAY, "cannot list backup targets"),
            },
        };
        enqueue_response(app.queue.enqueue("manual", Action::Backup { repo, force: body.force }, Duration::ZERO))
    });

    let start_prune = handler(&app, |app, req, _p| async move {
        let body: PruneBody = match read_optional_json(req).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        if body.keep == Some(0) {
            return err(StatusCode::BAD_REQUEST, "keep must be at least 1");
        }
        enqueue_response(app.queue.enqueue("manual", Action::Prune { keep: body.keep, dry_run: body.dry_run }, Duration::ZERO))
    });

    let backups = handler(&app, |app, _req, _p| async move {
        let Ok(_permit) = app.list_gate.try_acquire() else {
            return err(StatusCode::TOO_MANY_REQUESTS, "a listing is already in progress");
        };
        let refs = match bdg_core::backup::list_backups(&app.ctx.discord).await {
            Ok(r) => r,
            Err(e) => {
                bdg_cli::log(format!("listing backups failed: {e}"));
                return err(StatusCode::BAD_GATEWAY, "cannot read the Discord channel");
            }
        };
        let mut items = Vec::new();
        let mut unreadable = 0;
        for r in &refs {
            match bdg_core::backup::read_manifest(&app.ctx.discord, &app.ctx.key, r).await {
                Ok(m) => items.push(serde_json::json!({
                    "id": r.id8, "repo": m.repo, "created_at": m.created_at,
                    "parts": m.part_count, "plain_bytes": m.plain_bytes,
                })),
                Err(_) => unreadable += 1,
            }
        }
        items.sort_by_key(|i| std::cmp::Reverse(i["created_at"].as_i64().unwrap_or(0)));
        json(StatusCode::OK, serde_json::json!({ "backups": items, "unreadable": unreadable }))
    });

    let mut router = Router::new()
        .route(Method::GET, "/healthz", health)
        .route(Method::GET, "/api/jobs", api(jobs))
        .route(Method::GET, "/api/jobs/:id", api(job))
        .route(Method::GET, "/api/targets", api(targets))
        .route(Method::GET, "/api/backups", api(backups))
        .route(Method::POST, "/api/backup", api(start_backup))
        .route(Method::POST, "/api/prune", api(start_prune));

    if let Some(secret) = &settings.webhook_secret {
        let a = Arc::clone(&app);
        let delay = settings.webhook_delay;
        let cfg = WebhookConfig::github(secret.as_bytes().to_vec()).with_max_body_bytes(WEBHOOK_BODY_LIMIT);
        let hook = webhook_handler(cfg, move |hook: VerifiedWebhook| {
            let a = Arc::clone(&a);
            async move { on_github_event(a, hook, delay).await }
        });
        router = router.route(Method::POST, "/webhook/github", hook);
    }
    // 型駆動のAPIドキュメント(OpenAPI/Swagger UI/ReDoc/Cosmo互換SDL/Tauriマニフェスト、2026-10-26、
    // RPoemのopen-runo-openapi)。
    let router = open_runo_router::openapi::mount_docs(router, "/api", api_spec(settings.webhook_secret.is_some()));
    (router, app)
}

/// 署名検証済みのGitHubイベントを処理する。
async fn on_github_event(app: Arc<App>, hook: VerifiedWebhook, delay: Duration) -> Response {
    let event = hook.headers.get("x-github-event").and_then(|v| v.to_str().ok()).unwrap_or("");
    match event {
        "ping" => json(StatusCode::OK, serde_json::json!({ "pong": true })),
        "push" => {
            let payload: serde_json::Value = match serde_json::from_slice(&hook.body) {
                Ok(v) => v,
                Err(_) => return err(StatusCode::BAD_REQUEST, "invalid payload"),
            };
            let Some(name) = payload["repository"]["full_name"].as_str() else {
                return err(StatusCode::BAD_REQUEST, "invalid payload");
            };
            match app.canonical_target(name).await {
                Ok(Some(canon)) => {
                    let r = app.queue.enqueue("webhook", Action::Backup { repo: Some(canon), force: false }, delay);
                    enqueue_response(r)
                }
                Ok(None) => json(StatusCode::ACCEPTED, serde_json::json!({ "ignored": "not a backup target" })),
                // 対象一覧を確認できないときは失敗として返す(GitHub側で再配送できる)。
                Err(_) => err(StatusCode::SERVICE_UNAVAILABLE, "cannot verify backup targets"),
            }
        }
        _ => json(StatusCode::ACCEPTED, serde_json::json!({ "ignored": "unsupported event" })),
    }
}

/// ジョブを1つ実行して `(成功したか, 要約)` を返す。
pub async fn run_action(ctx: &Ctx, action: Action) -> (bool, Vec<String>) {
    fn lines(outs: &[bdg_cli::runner::Outcome]) -> Vec<String> {
        outs.iter()
            .map(|o| match &o.status {
                Status::BackedUp { id8, parts, plain_bytes } => format!("{}: backed up ({plain_bytes} bytes, {parts} parts, id {id8})", o.name),
                Status::Unchanged => format!("{}: unchanged", o.name),
                Status::Empty => format!("{}: empty repository", o.name),
                Status::Failed(e) => format!("{}: FAILED {e}", o.name),
            })
            .collect()
    }
    let prune_lines = |r: &bdg_core::backup::PruneReport, dry: bool| -> Vec<String> {
        let mut v: Vec<String> = r.deleted.iter().map(|(repo, id)| format!("{}{repo} {id}", if dry { "would delete " } else { "deleted " })).collect();
        v.push(format!("kept {}, unreadable {} (untouched)", r.kept.len(), r.unreadable));
        v
    };
    match action {
        Action::Backup { repo, force } => match backup_pass(ctx, force, repo.as_deref()).await {
            Ok(outs) => (!outs.iter().any(|o| matches!(o.status, Status::Failed(_))), lines(&outs)),
            Err(e) => (false, vec![e]),
        },
        Action::Prune { keep, dry_run } => match prune_pass(ctx, dry_run, keep).await {
            Ok(r) => (true, prune_lines(&r, dry_run)),
            Err(e) => (false, vec![e]),
        },
        Action::Scheduled => {
            let (mut ok, mut msgs) = match backup_pass(ctx, false, None).await {
                Ok(outs) => (!outs.iter().any(|o| matches!(o.status, Status::Failed(_))), lines(&outs)),
                Err(e) => (false, vec![e]),
            };
            match prune_pass(ctx, false, None).await {
                Ok(r) => msgs.extend(prune_lines(&r, false)),
                Err(e) => {
                    ok = false;
                    msgs.push(format!("prune failed: {e}"));
                }
            }
            (ok, msgs)
        }
    }
}

/// ワーカーと(設定されていれば)定期実行を起動する。
pub fn spawn_background(app: &Arc<App>) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    let a = Arc::clone(app);
    handles.push(tokio::spawn(async move {
        let ctx = Arc::clone(&a.ctx);
        a.queue
            .work(|action| {
                let ctx = Arc::clone(&ctx);
                async move { run_action(&ctx, action).await }
            })
            .await
    }));
    if app.settings.run_schedule {
        let a = Arc::clone(app);
        handles.push(tokio::spawn(async move {
            let interval = Duration::from_secs(a.ctx.cfg.schedule.interval_hours * 3600);
            // 起動直後は少し待つ。変更の無いリポジトリは送らないので、再起動しても軽い。
            tokio::time::sleep(Duration::from_secs(30)).await;
            loop {
                let _ = a.queue.enqueue("schedule", Action::Scheduled, Duration::ZERO);
                tokio::time::sleep(interval).await;
            }
        }));
    }
    handles
}

/// このサーバーのAPI宣言。`/api/openapi.json` ほかの元になる。
fn api_spec(with_webhook: bool) -> open_runo_openapi::Spec {
    use open_runo_openapi::{Operation as Op, Spec};
    let mut spec = Spec::new("backup-to-discord-by-github", env!("CARGO_PKG_VERSION"))
        .description("GitHub→暗号化→非公開Discordのバックアップ制御API")
        .op(Op::get("/healthz").summary("Health check").tag("system").public().response_empty(200, "OK"))
        .op(Op::get("/api/jobs").id("listJobs").summary("List backup jobs").tag("jobs").response_empty(200, "Jobs"))
        .op(Op::get("/api/jobs/:id").id("getJob").summary("Get one backup job").tag("jobs").response_empty(200, "Job").response_empty(404, "Not found"))
        .op(Op::get("/api/targets").id("listTargets").summary("List backup targets").tag("targets").response_empty(200, "Targets"))
        .op(Op::get("/api/backups").id("listBackups").summary("List stored backups").tag("backups").response_empty(200, "Backups"))
        .op(Op::post("/api/backup").id("startBackup").summary("Start a backup").tag("backups").response_empty(200, "Started"))
        .op(Op::post("/api/prune").id("startPrune").summary("Start pruning old backups").tag("backups").response_empty(200, "Started"));
    if with_webhook {
        spec = spec.op(Op::post("/webhook/github").id("githubWebhook").summary("GitHub webhook (HMAC-verified)").tag("webhook").public().body_raw("application/json").response_empty(200, "Accepted"));
    }
    spec
}
