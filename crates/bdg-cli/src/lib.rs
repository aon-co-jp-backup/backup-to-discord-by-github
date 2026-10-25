//! `bdg` の本体ロジック(設定・状態・1回分の実行)。`main.rs` は引数解析だけ。

pub mod config;
pub mod runner;
pub mod state;

pub fn log(msg: impl AsRef<str>) {
    eprintln!("{} {}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"), msg.as_ref());
}

/// UNIX秒を `YYYY-MM-DD HH:MM:SSZ`(UTC)にする。0以下は `-`。
pub fn fmt_unix(ts: i64) -> String {
    use chrono::TimeZone;
    if ts <= 0 {
        return "-".into();
    }
    chrono::Utc
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| "-".into())
}
