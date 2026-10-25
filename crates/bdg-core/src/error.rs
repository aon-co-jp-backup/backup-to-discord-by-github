use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// 鍵が違う/データが改ざん・破損している(認証タグ不一致)。
    Decrypt(&'static str),
    /// 形式が壊れている(マジック不一致・長さ不足・連番の飛びなど)。
    Format(String),
    /// 設定(鍵が空、パートサイズが小さすぎる等)。
    Config(String),
    Io(std::io::Error),
    /// Discord API(HTTPステータスと本文の要約)。
    Discord { status: u16, message: String },
    Http(reqwest::Error),
    Json(serde_json::Error),
    /// `git` コマンドの失敗。
    Git(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Decrypt(m) => write!(f, "decryption failed: {m}"),
            Error::Format(m) => write!(f, "invalid backup format: {m}"),
            Error::Config(m) => write!(f, "configuration error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Discord { status, message } => write!(f, "discord error {status}: {message}"),
            Error::Http(e) => write!(f, "http error: {e}"),
            Error::Json(e) => write!(f, "json error: {e}"),
            Error::Git(m) => write!(f, "git error: {m}"),
        }
    }
}

impl Error {
    /// Discord以外のHTTP APIの失敗(GitHubなど)。
    #[allow(non_snake_case)]
    pub fn Http_status(service: &str, status: u16, body: String) -> Self {
        Error::Discord { status, message: format!("{service}: {body}") }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // reqwestのエラー文字列にはURLが含まれる。Discordの添付URLは
        // 署名つきなので、ログに残さないよう URL を取り除く。
        Error::Http(e.without_url())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}
