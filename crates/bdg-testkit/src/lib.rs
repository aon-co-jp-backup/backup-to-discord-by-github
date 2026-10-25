//! テスト用のモックDiscord/GitHub API(RPoemで実装。ストリーミングmultipartの実利用も兼ねる)。
//! 公開しない(`publish = false`)。`start(state)` がルートURLを返す:
//! Discordは `{root}/api`、GitHubは `{root}/github` を基点にする。

use bytes::Bytes;
use hyper::{Method, StatusCode};
use open_runo_router::hyper_compat::{fixed_body, json_response, query_params, serve, Handler, Response, Router};
use open_runo_router::multipart_stream::{MultipartLimits, MultipartStream};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct State {
    pub next_id: u64,
    pub msgs: Vec<(u64, String, Vec<u8>)>,
    pub uploads: usize,
    /// GitHub APIが返すリポジトリ(JSON)。
    pub repos: Vec<serde_json::Value>,
    /// 受け取った `Authorization` ヘッダ(GitHub側)。
    pub github_auth: Vec<String>,
    /// 1回目のアップロードだけ429を返す。
    pub rate_limit_first: bool,
    /// n回目(1始まり)のアップロードで403を返す。
    pub reject_upload_n: Option<usize>,
}

pub type Shared = Arc<Mutex<State>>;

pub fn authorized(req: &open_runo_router::hyper_compat::Request) -> bool {
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

pub async fn start(state: Shared) -> String {
    let gh_state = state.clone();
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
        .route(Method::GET, "/cdn/:mid/:fname", cdn)
        .route(Method::GET, "/github/orgs/:o/repos", github_repos(gh_state.clone()))
        .route(Method::GET, "/github/user/repos", github_repos(gh_state));
    let (addr, _h) = serve(router, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    format!("http://{addr}")
}


/// 1ページ目だけ全件を返し、2ページ目以降は空(ページング終了の確認用)。
fn github_repos(st: Shared) -> Handler {
    Arc::new(move |req, _p| {
        let st = st.clone();
        Box::pin(async move {
            let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let page: u32 = query_params(&req).get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
            let mut s = st.lock().unwrap();
            s.github_auth.push(auth);
            let out = if page == 1 { s.repos.clone() } else { Vec::new() };
            json_response(StatusCode::OK, &out)
        })
    })
}
