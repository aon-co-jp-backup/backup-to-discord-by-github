//! 設定ファイル(TOML)。秘密(Botトークン・パスフレーズ・GitHubトークン)は
//! 設定に直接書かず、**ファイルのパス**で指す(`F:\` 直下のtxtなど)。
//! 秘密は引数にも環境変数にも出さない。

use bdg_core::crypto::{KdfParams, KeySource};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub discord: DiscordCfg,
    pub crypto: CryptoCfg,
    #[serde(default)]
    pub github: GithubCfg,
    #[serde(default)]
    pub schedule: ScheduleCfg,
    /// `bdg-server`(管理APIとWebhook受信)用。CLIだけなら不要。
    #[serde(default)]
    pub server: ServerCfg,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    /// 待ち受けアドレス。TLSは持たないので、既定はループバックのみ
    /// (公開する場合はnginx等のリバースプロキシの背後に置く)。
    #[serde(default = "default_listen")]
    pub listen: String,
    /// 管理APIの Bearer トークン(1行のtxt)。`bdg-server` には必須。
    pub admin_token_file: Option<PathBuf>,
    /// GitHub Webhook の秘密(1行のtxt)。無ければ Webhook 受信は無効。
    pub webhook_secret_file: Option<PathBuf>,
    /// Webhook を受けてからバックアップを始めるまでの待ち(秒)。
    /// 連続pushは1回にまとめる。
    #[serde(default = "default_webhook_delay")]
    pub webhook_delay_secs: u64,
    /// サーバー自身が `schedule.interval_hours` ごとに全体を実行する。
    #[serde(default = "yes")]
    pub run_schedule: bool,
    /// 管理APIの1分あたりの上限(クライアント単位)。
    #[serde(default = "default_api_rate")]
    pub api_rate_per_min: u32,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            admin_token_file: None,
            webhook_secret_file: None,
            webhook_delay_secs: default_webhook_delay(),
            run_schedule: true,
            api_rate_per_min: default_api_rate(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8787".to_string()
}
fn default_webhook_delay() -> u64 {
    60
}
fn default_api_rate() -> u32 {
    120
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordCfg {
    pub channel_id: String,
    pub token_file: PathBuf,
    /// 1ファイルの上限(MiB)。Discordの添付上限より小さく。
    #[serde(default = "default_upload_mib")]
    pub max_upload_mib: u32,
    /// 試験用に差し替える。通常は書かない。
    pub api_base: Option<String>,
}

fn default_upload_mib() -> u32 {
    8
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CryptoCfg {
    pub passphrase_file: PathBuf,
    /// Argon2idのメモリ(MiB)。
    #[serde(default = "default_kdf_mib")]
    pub kdf_memory_mib: u32,
}

fn default_kdf_mib() -> u32 {
    64
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubCfg {
    /// 非公開リポジトリの取得・一覧に使うトークン(PAT)。
    pub token_file: Option<PathBuf>,
    /// 組織ごとに全リポジトリを列挙する。
    #[serde(default)]
    pub orgs: Vec<String>,
    /// トークンの持ち主のリポジトリ(非公開を含む)も列挙する。
    #[serde(default)]
    pub include_user: bool,
    #[serde(default)]
    pub include_forks: bool,
    #[serde(default = "yes")]
    pub include_archived: bool,
    /// 追加で明示するリポジトリ(`owner/name` またはcloneのURL)。
    #[serde(default)]
    pub repos: Vec<String>,
    /// 試験用に差し替える。通常は書かない。
    pub api_base: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleCfg {
    /// `bdg run`(常駐)の実行間隔。
    #[serde(default = "default_interval")]
    pub interval_hours: u64,
    /// リポジトリごとに残す世代数(1以上)。
    #[serde(default = "default_keep")]
    pub keep: usize,
    /// リポジトリ間の待ち(秒)。Discordのレート制限への配慮。
    #[serde(default = "default_pause")]
    pub pause_secs: u64,
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    /// 前回から参照が変わっていないリポジトリは送らない。
    #[serde(default = "yes")]
    pub only_if_changed: bool,
}

impl Default for ScheduleCfg {
    fn default() -> Self {
        Self {
            interval_hours: default_interval(),
            keep: default_keep(),
            pause_secs: default_pause(),
            state_file: default_state_file(),
            only_if_changed: true,
        }
    }
}

fn default_interval() -> u64 {
    24
}
fn default_keep() -> usize {
    3
}
fn default_pause() -> u64 {
    5
}
fn default_state_file() -> PathBuf {
    PathBuf::from("bdg-state.json")
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).map_err(|e| format!("invalid config {}: {e}", path.display()))?;
        cfg.validate()?;
        // 状態ファイルは、設定ファイルからの相対パスなら設定ファイルの隣に置く。
        if cfg.schedule.state_file.is_relative() {
            if let Some(dir) = path.parent() {
                cfg.schedule.state_file = dir.join(&cfg.schedule.state_file);
            }
        }
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schedule.keep == 0 {
            return Err("schedule.keep must be at least 1".into());
        }
        if self.schedule.interval_hours == 0 {
            return Err("schedule.interval_hours must be at least 1".into());
        }
        if self.discord.max_upload_mib == 0 || self.discord.max_upload_mib > 100 {
            return Err("discord.max_upload_mib must be 1..=100".into());
        }
        if self.crypto.kdf_memory_mib < 19 {
            return Err("crypto.kdf_memory_mib must be at least 19 (OWASP minimum)".into());
        }
        let has_targets = !self.github.orgs.is_empty() || self.github.include_user || !self.github.repos.is_empty();
        if !has_targets {
            return Err("no backup targets: set github.orgs, github.include_user or github.repos".into());
        }
        if (!self.github.orgs.is_empty() || self.github.include_user) && self.github.token_file.is_none() {
            return Err("github.token_file is required to list organization / user repositories".into());
        }
        Ok(())
    }

    pub fn kdf(&self) -> KdfParams {
        KdfParams { m_cost: self.crypto.kdf_memory_mib * 1024, t_cost: 3, p_cost: 1 }
    }

    pub fn key_source(&self) -> Result<KeySource, String> {
        Ok(KeySource::Passphrase(read_secret(&self.crypto.passphrase_file, "passphrase")?))
    }
}

/// 秘密のファイルを読む。前後の空白・改行は取り除く。
/// エラーに中身は含めない。
pub fn read_secret(path: &Path, what: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {what} file {}: {e}", path.display()))?;
    let s = text.trim().to_string();
    if s.is_empty() {
        return Err(format!("{what} file {} is empty", path.display()));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, String> {
        let c: Config = toml::from_str(s).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }

    const BASE: &str = r#"
[discord]
channel_id = "1"
token_file = "t"
[crypto]
passphrase_file = "p"
"#;

    #[test]
    fn requires_targets_and_token_for_listing() {
        assert!(parse(BASE).unwrap_err().contains("no backup targets"));
        let c = format!("{BASE}[github]\nrepos = [\"o/r\"]\n");
        assert!(parse(&c).is_ok());
        let c = format!("{BASE}[github]\norgs = [\"o\"]\n");
        assert!(parse(&c).unwrap_err().contains("token_file"));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let c = format!("{BASE}[github]\nrepos = [\"o/r\"]\nbogus = 1\n");
        assert!(parse(&c).is_err(), "typos in keys must not be silently ignored");
        let c = format!("{BASE}[github]\nrepos = [\"o/r\"]\n[schedule]\nkeep = 0\n");
        assert!(parse(&c).is_err());
    }

    #[test]
    fn secrets_are_trimmed_and_errors_do_not_leak() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.txt");
        std::fs::write(&p, "  secret-value \r\n").unwrap();
        assert_eq!(read_secret(&p, "x").unwrap(), "secret-value");
        std::fs::write(&p, "\n").unwrap();
        assert!(read_secret(&p, "x").is_err());
    }
}
