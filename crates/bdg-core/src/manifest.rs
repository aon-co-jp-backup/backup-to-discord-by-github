//! 暗号化前のマニフェスト(JSON)。暗号化して最後に投稿する。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartEntry {
    pub index: u32,
    /// 暗号化後のファイルサイズ。
    pub size: u64,
    /// 暗号化後のファイルのSHA-256(16進)。復号前に破損を検出するため。
    pub sha256: String,
    /// Discord上のメッセージID。
    pub message_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    /// 16進32文字。
    pub backup_id: String,
    /// `owner/name` などの人間向けの名前(暗号化されるので平文で書いてよい)。
    pub repo: String,
    /// 作成時刻(UNIX秒)。
    pub created_at: i64,
    /// `git-bundle`。
    pub kind: String,
    pub part_count: u32,
    /// 暗号化前の合計バイト数。
    pub plain_bytes: u64,
    pub parts: Vec<PartEntry>,
}

/// Discordに置くファイル名。内容を示す情報は含めない。
pub fn part_filename(backup_id_hex: &str, index: u32) -> String {
    format!("bdg-{}-{:05}.bin", &backup_id_hex[..8.min(backup_id_hex.len())], index)
}

pub fn manifest_filename(backup_id_hex: &str) -> String {
    format!("bdg-{}-manifest.bin", &backup_id_hex[..8.min(backup_id_hex.len())])
}

/// `bdg-<id8>-manifest.bin` から `<id8>` を取り出す。
pub fn parse_manifest_filename(name: &str) -> Option<&str> {
    let id = name.strip_prefix("bdg-")?.strip_suffix("-manifest.bin")?;
    (id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_generic_and_parseable() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(part_filename(id, 7), "bdg-01234567-00007.bin");
        assert_eq!(manifest_filename(id), "bdg-01234567-manifest.bin");
        assert_eq!(parse_manifest_filename("bdg-01234567-manifest.bin"), Some("01234567"));
        assert_eq!(parse_manifest_filename("bdg-01234567-00007.bin"), None);
        assert_eq!(parse_manifest_filename("bdg-zzzzzzzz-manifest.bin"), None);
    }
}
