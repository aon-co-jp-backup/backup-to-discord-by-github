//! GitHub API でバックアップ対象のリポジトリを列挙する(公開・非公開の両方)。
//! 非公開を含めるには、`repo` 権限(または fine-grained の Contents 読み取り +
//! Metadata)を持つトークンが要る。

use crate::{Error, Result};
use serde::Deserialize;

pub const DEFAULT_API_BASE: &str = "https://api.github.com";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RepoInfo {
    pub full_name: String,
    pub clone_url: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Clone)]
pub enum Owner {
    /// 組織(`/orgs/{name}/repos`)。
    Org(String),
    /// トークンの持ち主自身が所有するリポジトリ(`/user/repos`、非公開を含む)。
    AuthenticatedUser,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !s.starts_with('.')
}

pub async fn list_repos(api_base: &str, token: &str, owner: &Owner) -> Result<Vec<RepoInfo>> {
    let base = api_base.trim_end_matches('/');
    let path = match owner {
        Owner::Org(name) => {
            if !valid_name(name) {
                return Err(Error::Config(format!("invalid organization name {name:?}")));
            }
            format!("{base}/orgs/{name}/repos?type=all&per_page=100")
        }
        Owner::AuthenticatedUser => format!("{base}/user/repos?affiliation=owner&per_page=100"),
    };
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?;
    let mut out = Vec::new();
    for page in 1..=500u32 {
        let resp = http
            .get(format!("{path}&page={page}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "backup-to-discord-by-github")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
            return Err(Error::Http_status("github api", status.as_u16(), body));
        }
        let items: Vec<RepoInfo> = resp.json().await?;
        let n = items.len();
        out.extend(items);
        if n < 100 {
            break;
        }
    }
    Ok(out)
}
