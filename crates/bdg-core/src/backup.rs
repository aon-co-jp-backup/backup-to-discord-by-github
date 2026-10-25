//! バックアップ(暗号化→分割→Discord投稿)と復元。

use crate::crypto::{self, KdfParams, Key, KeySource, ManifestHeader};
use crate::discord::DiscordClient;
use crate::manifest::{manifest_filename, parse_manifest_filename, part_filename, Manifest, PartEntry};
use crate::split::{EncryptedPart, PartIter};
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[derive(Debug, Clone)]
pub struct BackupOptions {
    /// マニフェストに書く名前(`owner/name`)。暗号化されるので平文でよい。
    pub repo: String,
    pub kdf: KdfParams,
}

impl BackupOptions {
    pub fn new(repo: impl Into<String>) -> Self {
        Self { repo: repo.into(), kdf: KdfParams::default() }
    }
}

fn is_raw(source: &KeySource) -> bool {
    matches!(source, KeySource::Raw(_))
}

async fn derive_key(source: &KeySource, salt: [u8; 16], kdf: KdfParams) -> Result<Key> {
    // Argon2idは重い(数百ms〜秒)のでasyncのスレッドを塞がない。
    let source = match source {
        KeySource::Passphrase(p) => KeySource::Passphrase(p.clone()),
        KeySource::Raw(k) => KeySource::Raw(*k),
    };
    tokio::task::spawn_blocking(move || Key::derive(&source, &salt, kdf))
        .await
        .map_err(|e| Error::Config(format!("key derivation task failed: {e}")))?
}

/// 平文(git bundle)を暗号化・分割して Discord へ保存する。
/// 途中で失敗したら、投稿済みのパートを可能な範囲で削除する。
pub async fn backup<R>(
    reader: R,
    key_source: &KeySource,
    discord: &DiscordClient,
    opts: &BackupOptions,
) -> Result<Manifest>
where
    R: Read + Send + 'static,
{
    let salt: [u8; 16] = crypto::random_bytes();
    let backup_id: [u8; 16] = crypto::random_bytes();
    let backup_id_hex = hex::encode(backup_id);
    let key = derive_key(key_source, salt, opts.kdf).await?;

    // 暗号化は別スレッドで進め、1パートずつ受け渡す(メモリは数パート分)。
    let max_file = discord.max_upload_bytes();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<EncryptedPart>>(1);
    let producer_key = key.clone();
    let producer = tokio::task::spawn_blocking(move || {
        let iter = match PartIter::new(reader, &producer_key, backup_id, max_file) {
            Ok(i) => i,
            Err(e) => {
                let _ = tx.blocking_send(Err(e));
                return;
            }
        };
        for part in iter {
            if tx.blocking_send(part).is_err() {
                return; // 受け側が止まった
            }
        }
    });

    let mut uploaded: Vec<String> = Vec::new();
    let result = async {
        let mut entries = Vec::new();
        let mut plain_bytes = 0u64;
        while let Some(part) = rx.recv().await {
            let part = part?;
            let digest = hex::encode(Sha256::digest(&part.bytes));
            let size = part.bytes.len() as u64;
            let msg = discord.upload(&part_filename(&backup_id_hex, part.index), part.bytes).await?;
            uploaded.push(msg.id.clone());
            plain_bytes += part.plain_len as u64;
            entries.push(PartEntry { index: part.index, size, sha256: digest, message_id: msg.id });
        }
        let manifest = Manifest {
            version: 1,
            backup_id: backup_id_hex.clone(),
            repo: opts.repo.clone(),
            created_at: unix_now(),
            kind: "git-bundle".into(),
            part_count: entries.len() as u32,
            plain_bytes,
            parts: entries,
        };
        let (_, header) = ManifestHeader::new(salt, opts.kdf, is_raw(key_source));
        let file = crypto::encrypt_manifest(&key, &header, &serde_json::to_vec(&manifest)?)?;
        let msg = discord.upload(&manifest_filename(&backup_id_hex), file).await?;
        uploaded.push(msg.id);
        Ok::<_, Error>(manifest)
    }
    .await;

    drop(rx);
    let producer_join = producer.await;
    match result {
        Ok(manifest) => {
            producer_join.map_err(|e| Error::Config(format!("encryption task failed: {e}")))?;
            Ok(manifest)
        }
        Err(e) => {
            for id in &uploaded {
                let _ = discord.delete_message(id).await;
            }
            Err(e)
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRef {
    /// バックアップIDの先頭8文字。
    pub id8: String,
    pub manifest_message_id: String,
}

/// チャンネルを新しい順にたどり、マニフェストのあるバックアップを列挙する。
/// 中身(リポジトリ名など)は鍵が無いと読めないので、ここではIDだけ。
pub async fn list_backups(discord: &DiscordClient) -> Result<Vec<BackupRef>> {
    let mut out = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = discord.list_messages(before.as_deref()).await?;
        if page.is_empty() {
            break;
        }
        for m in &page {
            for a in &m.attachments {
                if let Some(id8) = parse_manifest_filename(&a.filename) {
                    out.push(BackupRef { id8: id8.to_string(), manifest_message_id: m.id.clone() });
                }
            }
        }
        before = page.last().map(|m| m.id.clone());
        if page.len() < 100 {
            break;
        }
    }
    Ok(out)
}

/// マニフェストだけを取得・復号する(中身の確認用)。
pub async fn read_manifest(discord: &DiscordClient, key_source: &KeySource, r: &BackupRef) -> Result<Manifest> {
    let msg = discord.get_message(&r.manifest_message_id).await?;
    let wanted = format!("bdg-{}-manifest.bin", r.id8);
    let att = msg
        .attachments
        .iter()
        .find(|a| a.filename == wanted)
        .ok_or_else(|| Error::Format("manifest attachment not found".into()))?;
    let file = discord.download(att).await?;
    let header = ManifestHeader::parse(&file)?;
    let key = derive_key(key_source, header.salt, header.kdf).await?;
    let json = crypto::decrypt_manifest(&key, &file)?;
    let manifest: Manifest = serde_json::from_slice(&json)?;
    if !manifest.backup_id.starts_with(&r.id8) {
        return Err(Error::Format("manifest backup id does not match its file name".into()));
    }
    Ok(manifest)
}

/// 1つのバックアップを削除する。パート → マニフェストの順に消すので、
/// 途中で失敗してもマニフェストが残り、あとで再実行して続きを消せる。
pub async fn delete_backup(discord: &DiscordClient, manifest: &Manifest, r: &BackupRef) -> Result<()> {
    for p in &manifest.parts {
        discord.delete_message(&p.message_id).await?;
    }
    discord.delete_message(&r.manifest_message_id).await
}

#[derive(Debug, Default)]
pub struct PruneReport {
    /// 残したバックアップ `(repo, id8)`。
    pub kept: Vec<(String, String)>,
    /// 削除した(`dry_run` なら削除対象の)バックアップ。
    pub deleted: Vec<(String, String)>,
    /// 鍵が合わず読めなかったマニフェストの数。**削除しない**
    /// (別の鍵・別の用途のバックアップかもしれないため)。
    pub unreadable: usize,
}

/// リポジトリごとに新しい順に `keep` 件だけ残して古いものを削除する。
/// `keep` は1以上。読めないマニフェストには手を付けない。
pub async fn prune(
    discord: &DiscordClient,
    key_source: &KeySource,
    keep: usize,
    repo_filter: Option<&str>,
    dry_run: bool,
) -> Result<PruneReport> {
    if keep == 0 {
        return Err(Error::Config("keep must be at least 1".into()));
    }
    let mut report = PruneReport::default();
    let mut by_repo: std::collections::BTreeMap<String, Vec<(Manifest, BackupRef)>> = Default::default();
    for r in list_backups(discord).await? {
        match read_manifest(discord, key_source, &r).await {
            Ok(m) => {
                if repo_filter.is_none_or(|f| f == m.repo) {
                    by_repo.entry(m.repo.clone()).or_default().push((m, r));
                }
            }
            Err(Error::Decrypt(_)) | Err(Error::Format(_)) => report.unreadable += 1,
            Err(e) => return Err(e),
        }
    }
    for (repo, mut list) in by_repo {
        list.sort_by_key(|(m, _)| std::cmp::Reverse(m.created_at));
        for (i, (m, r)) in list.into_iter().enumerate() {
            let entry = (repo.clone(), r.id8.clone());
            if i < keep {
                report.kept.push(entry);
            } else {
                if !dry_run {
                    delete_backup(discord, &m, &r).await?;
                }
                report.deleted.push(entry);
            }
        }
    }
    Ok(report)
}

/// 復元する。平文(git bundle)を `out` へ順に書く。検証にすべて通るまで
/// 「成功」を返さない: SHA-256・サイズ・連番・最終パート・合計バイト数。
pub async fn restore<W: Write>(
    discord: &DiscordClient,
    key_source: &KeySource,
    backup_ref: &BackupRef,
    out: &mut W,
) -> Result<Manifest> {
    let manifest = read_manifest(discord, key_source, backup_ref).await?;
    // 鍵は read_manifest 内でも導出済みだが、パート復号用に再導出する
    // (ヘッダのソルトを使うため同じ鍵になる)。
    let msg = discord.get_message(&backup_ref.manifest_message_id).await?;
    let att = msg
        .attachments
        .iter()
        .find(|a| a.filename == manifest_filename(&manifest.backup_id))
        .ok_or_else(|| Error::Format("manifest attachment not found".into()))?;
    let mfile = discord.download(att).await?;
    let header = ManifestHeader::parse(&mfile)?;
    let key = derive_key(key_source, header.salt, header.kdf).await?;

    if manifest.part_count as usize != manifest.parts.len() || manifest.parts.is_empty() {
        return Err(Error::Format("manifest part count mismatch".into()));
    }
    let id_bytes: [u8; 16] = hex::decode(&manifest.backup_id)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| Error::Format("bad backup id".into()))?;

    let mut total = 0u64;
    let n = manifest.parts.len();
    for (i, entry) in manifest.parts.iter().enumerate() {
        if entry.index as usize != i {
            return Err(Error::Format(format!("part list is not contiguous at {i}")));
        }
        let m = discord.get_message(&entry.message_id).await?;
        let wanted = part_filename(&manifest.backup_id, entry.index);
        let att = m
            .attachments
            .iter()
            .find(|a| a.filename == wanted)
            .ok_or_else(|| Error::Format(format!("part {i}: attachment missing")))?;
        let bytes = discord.download(att).await?;
        if bytes.len() as u64 != entry.size || hex::encode(Sha256::digest(&bytes)) != entry.sha256 {
            return Err(Error::Format(format!("part {i}: size or checksum mismatch")));
        }
        let (plain, last) = crypto::decrypt_part(&key, &id_bytes, entry.index, &bytes)?;
        if last != (i + 1 == n) {
            return Err(Error::Format(format!("part {i}: unexpected last-part flag")));
        }
        total += plain.len() as u64;
        out.write_all(&plain)?;
    }
    if total != manifest.plain_bytes {
        return Err(Error::Format("restored size differs from the manifest".into()));
    }
    out.flush()?;
    Ok(manifest)
}
