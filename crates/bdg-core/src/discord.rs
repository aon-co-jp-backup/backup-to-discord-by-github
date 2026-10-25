//! Discord REST API の最小クライアント(Bot トークン認証)。
//!
//! 必要な権限: 対象チャンネルで「メッセージを送信」「ファイルを添付」
//! 「メッセージ履歴を読む」(+ 失敗時の掃除に「メッセージの管理」)。
//!
//! - 429(レート制限)は `retry_after` に従って待って再試行する。
//! - 5xx・接続エラーは指数バックオフで再試行する。
//! - トークンは `Debug` に出さない。添付URLは署名つきなのでエラーに含めない。

use crate::{Error, Result};
use serde::Deserialize;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://discord.com/api/v10";

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// 1回の待ちの上限(`retry_after` がこれより長ければ失敗にする)。
    pub max_wait: Duration,
    /// 5xx/接続エラーの初回待ち(以降は2倍)。
    pub backoff_base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 6, max_wait: Duration::from_secs(120), backoff_base: Duration::from_secs(1) }
    }
}

pub struct DiscordClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    channel_id: String,
    /// これを超えるファイルは送らない(サーバーの添付上限より小さく設定する)。
    max_upload_bytes: usize,
    retry: RetryPolicy,
}

impl std::fmt::Debug for DiscordClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordClient")
            .field("channel_id", &self.channel_id)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

impl DiscordClient {
    pub fn new(token: impl Into<String>, channel_id: impl Into<String>) -> Result<Self> {
        let token = token.into();
        let channel_id = channel_id.into();
        if token.trim().is_empty() {
            return Err(Error::Config("discord bot token is empty".into()));
        }
        // チャンネルIDはURLに入れるので、数字だけに限る(パス注入を防ぐ)。
        if channel_id.is_empty() || !channel_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Config("discord channel id must be numeric".into()));
        }
        Ok(Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?,
            base_url: DEFAULT_BASE_URL.to_string(),
            token,
            channel_id,
            max_upload_bytes: 8 * 1024 * 1024,
            retry: RetryPolicy::default(),
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_max_upload_bytes(mut self, n: usize) -> Self {
        self.max_upload_bytes = n;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn max_upload_bytes(&self) -> usize {
        self.max_upload_bytes
    }

    fn messages_url(&self) -> String {
        format!("{}/channels/{}/messages", self.base_url, self.channel_id)
    }

    /// リクエストを作って送る。429/5xx/接続エラーは再試行する。
    /// 成功(2xx)のレスポンスを返す。
    async fn send_with_retry<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut backoff = self.retry.backoff_base;
        for attempt in 1..=self.retry.max_attempts {
            let last_attempt = attempt == self.retry.max_attempts;
            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    if status.as_u16() == 429 && !last_attempt {
                        let wait = retry_after(resp).await;
                        if wait > self.retry.max_wait {
                            return Err(Error::Discord {
                                status: 429,
                                message: format!("rate limited; retry_after {wait:?} exceeds max_wait"),
                            });
                        }
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if status.is_server_error() && !last_attempt {
                        tokio::time::sleep(backoff).await;
                        backoff *= 2;
                        continue;
                    }
                    let message = error_summary(resp).await;
                    return Err(Error::Discord { status: status.as_u16(), message });
                }
                Err(e) if (e.is_connect() || e.is_timeout() || e.is_request()) && !last_attempt => {
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Discord { status: 0, message: "retries exhausted".into() })
    }

    /// ファイルを1つ添付したメッセージを投稿する。
    pub async fn upload(&self, filename: &str, bytes: Vec<u8>) -> Result<Message> {
        if bytes.len() > self.max_upload_bytes {
            return Err(Error::Config(format!(
                "file of {} bytes exceeds max_upload_bytes {}",
                bytes.len(),
                self.max_upload_bytes
            )));
        }
        let url = self.messages_url();
        let resp = self
            .send_with_retry(|| {
                let part = reqwest::multipart::Part::bytes(bytes.clone())
                    .file_name(filename.to_string())
                    .mime_str("application/octet-stream")
                    .expect("static mime type is valid");
                let form = reqwest::multipart::Form::new().part("files[0]", part);
                self.http.post(&url).header("Authorization", format!("Bot {}", self.token)).multipart(form)
            })
            .await?;
        Ok(resp.json::<Message>().await?)
    }

    pub async fn get_message(&self, message_id: &str) -> Result<Message> {
        check_id(message_id)?;
        let url = format!("{}/{}", self.messages_url(), message_id);
        let resp = self
            .send_with_retry(|| self.http.get(&url).header("Authorization", format!("Bot {}", self.token)))
            .await?;
        Ok(resp.json::<Message>().await?)
    }

    /// 新しい順に最大100件。`before` を渡すとそれより古いものを返す。
    pub async fn list_messages(&self, before: Option<&str>) -> Result<Vec<Message>> {
        let mut url = format!("{}?limit=100", self.messages_url());
        if let Some(b) = before {
            check_id(b)?;
            url.push_str(&format!("&before={b}"));
        }
        let resp = self
            .send_with_retry(|| self.http.get(&url).header("Authorization", format!("Bot {}", self.token)))
            .await?;
        Ok(resp.json::<Vec<Message>>().await?)
    }

    /// 添付ファイルをダウンロードする(CDN。認証ヘッダは付けない)。
    pub async fn download(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        let url = attachment.url.clone();
        let resp = self.send_with_retry(|| self.http.get(&url)).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// 失敗したバックアップの掃除用。
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        check_id(message_id)?;
        let url = format!("{}/{}", self.messages_url(), message_id);
        self.send_with_retry(|| self.http.delete(&url).header("Authorization", format!("Bot {}", self.token)))
            .await?;
        Ok(())
    }
}

fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Format(format!("invalid discord id {id:?}")));
    }
    Ok(())
}

async fn retry_after(resp: reqwest::Response) -> Duration {
    let header = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok());
    let body = resp.json::<serde_json::Value>().await.ok();
    let secs = body
        .as_ref()
        .and_then(|b| b.get("retry_after"))
        .and_then(|v| v.as_f64())
        .or(header)
        .unwrap_or(1.0);
    // 少しだけ余裕を持たせる。
    Duration::from_secs_f64(secs.clamp(0.0, 3600.0) + 0.05)
}

async fn error_summary(resp: reqwest::Response) -> String {
    let text = resp.text().await.unwrap_or_default();
    // Discordのエラーは {"message": "...", "code": N}。長い本文は切る。
    let msg = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or(text);
    msg.chars().take(200).collect()
}
