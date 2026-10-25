//! 鍵の導出とパート/マニフェストの暗号化。
//!
//! - 暗号: XChaCha20-Poly1305(192bitランダムnonceなので、同じ鍵で多数の
//!   パートを暗号化しても衝突を気にしなくてよい)。
//! - 鍵: パスフレーズ(Argon2id)または32バイトの生の鍵。
//! - 各パートの AAD に「バックアップID・連番・最終パートか」を入れる。
//!   パートの入れ替え・別バックアップ・欠落/末尾切り詰めは復号時に失敗する。

use crate::{Error, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;

pub const PART_MAGIC: &[u8; 4] = b"BDG1";
pub const MANIFEST_MAGIC: &[u8; 4] = b"BDGM";
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// パートファイルの固定オーバーヘッド(マジック+nonce+認証タグ)。
pub const PART_OVERHEAD: usize = 4 + NONCE_LEN + TAG_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// メモリ(KiB)。
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        // OWASPの推奨範囲(64MiB / 3回 / 並列1)。
        Self { m_cost: 64 * 1024, t_cost: 3, p_cost: 1 }
    }
}

pub enum KeySource {
    Passphrase(String),
    Raw([u8; 32]),
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 鍵は決してログに出さない。
        f.write_str("KeySource(..)")
    }
}

/// 導出済みの鍵。Dropで内容を上書きする。
#[derive(Clone)]
pub struct Key([u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // 最適化で消されないよう volatile で書く。
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    pub fn derive(source: &KeySource, salt: &[u8; 16], params: KdfParams) -> Result<Key> {
        match source {
            KeySource::Raw(k) => Ok(Key(*k)),
            KeySource::Passphrase(p) => {
                if p.is_empty() {
                    return Err(Error::Config("passphrase is empty".into()));
                }
                let argon_params = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
                    .map_err(|e| Error::Config(format!("invalid KDF parameters: {e}")))?;
                let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
                let mut out = [0u8; 32];
                argon
                    .hash_password_into(p.as_bytes(), salt, &mut out)
                    .map_err(|e| Error::Config(format!("key derivation failed: {e}")))?;
                Ok(Key(out))
            }
        }
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

fn part_aad(backup_id: &[u8; 16], index: u32, last: bool) -> Vec<u8> {
    let mut aad = Vec::with_capacity(4 + 16 + 4 + 1);
    aad.extend_from_slice(PART_MAGIC);
    aad.extend_from_slice(backup_id);
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(last as u8);
    aad
}

/// 1パートを暗号化してファイル形式(`BDG1 | nonce | 暗号文+タグ`)にする。
pub fn encrypt_part(key: &Key, backup_id: &[u8; 16], index: u32, last: bool, plain: &[u8]) -> Result<Vec<u8>> {
    let nonce_bytes: [u8; NONCE_LEN] = random_bytes();
    let aad = part_aad(backup_id, index, last);
    let ct = key
        .cipher()
        .encrypt(XNonce::from_slice(&nonce_bytes), Payload { msg: plain, aad: &aad })
        .map_err(|_| Error::Decrypt("encryption failed"))?;
    let mut out = Vec::with_capacity(4 + NONCE_LEN + ct.len());
    out.extend_from_slice(PART_MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 1パートを復号する。戻り値は `(平文, 最終パートか)`。
/// 最終パートかどうかはAADに入っているので、両方を試して認証が通った方が正しい。
pub fn decrypt_part(key: &Key, backup_id: &[u8; 16], index: u32, file: &[u8]) -> Result<(Vec<u8>, bool)> {
    if file.len() < PART_OVERHEAD || &file[..4] != PART_MAGIC {
        return Err(Error::Format(format!("part {index}: bad magic or too short")));
    }
    let nonce = XNonce::from_slice(&file[4..4 + NONCE_LEN]);
    let ct = &file[4 + NONCE_LEN..];
    for last in [false, true] {
        let aad = part_aad(backup_id, index, last);
        if let Ok(plain) = key.cipher().decrypt(nonce, Payload { msg: ct, aad: &aad }) {
            return Ok((plain, last));
        }
    }
    Err(Error::Decrypt("wrong key, or the part is corrupted / reordered / from another backup"))
}

/// マニフェストファイルの先頭(平文のヘッダ): 復号前に鍵を導出するために
/// ソルトとKDFパラメータが要るため、ここだけは暗号化しない。
/// `BDGM | version(1) | mode(1) | salt(16) | m(4) | t(4) | p(4)` = 34バイト。
pub const MANIFEST_HEADER_LEN: usize = 4 + 1 + 1 + 16 + 12;

#[derive(Debug, Clone, Copy)]
pub struct ManifestHeader {
    pub salt: [u8; 16],
    pub kdf: KdfParams,
}

impl ManifestHeader {
    pub fn new(salt: [u8; 16], kdf: KdfParams, raw_key: bool) -> (Self, Vec<u8>) {
        let mut h = Vec::with_capacity(MANIFEST_HEADER_LEN);
        h.extend_from_slice(MANIFEST_MAGIC);
        h.push(1);
        h.push(raw_key as u8);
        h.extend_from_slice(&salt);
        h.extend_from_slice(&kdf.m_cost.to_be_bytes());
        h.extend_from_slice(&kdf.t_cost.to_be_bytes());
        h.extend_from_slice(&kdf.p_cost.to_be_bytes());
        (Self { salt, kdf }, h)
    }

    pub fn parse(file: &[u8]) -> Result<Self> {
        if file.len() < MANIFEST_HEADER_LEN + NONCE_LEN + TAG_LEN || &file[..4] != MANIFEST_MAGIC {
            return Err(Error::Format("manifest: bad magic or too short".into()));
        }
        if file[4] != 1 {
            return Err(Error::Format(format!("manifest: unsupported version {}", file[4])));
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&file[6..22]);
        let u32_at = |i: usize| u32::from_be_bytes([file[i], file[i + 1], file[i + 2], file[i + 3]]);
        let kdf = KdfParams { m_cost: u32_at(22), t_cost: u32_at(26), p_cost: u32_at(30) };
        Ok(Self { salt, kdf })
    }
}

pub fn encrypt_manifest(key: &Key, header_bytes: &[u8], plain_json: &[u8]) -> Result<Vec<u8>> {
    let nonce_bytes: [u8; NONCE_LEN] = random_bytes();
    let ct = key
        .cipher()
        .encrypt(XNonce::from_slice(&nonce_bytes), Payload { msg: plain_json, aad: header_bytes })
        .map_err(|_| Error::Decrypt("encryption failed"))?;
    let mut out = Vec::with_capacity(header_bytes.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(header_bytes);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn decrypt_manifest(key: &Key, file: &[u8]) -> Result<Vec<u8>> {
    // 呼び出し側が `ManifestHeader::parse` で長さ等を検証済みの前提だが、念のため再確認。
    ManifestHeader::parse(file)?;
    let (header, rest) = file.split_at(MANIFEST_HEADER_LEN);
    let nonce = XNonce::from_slice(&rest[..NONCE_LEN]);
    key.cipher()
        .decrypt(nonce, Payload { msg: &rest[NONCE_LEN..], aad: header })
        .map_err(|_| Error::Decrypt("wrong key, or the manifest is corrupted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> KdfParams {
        KdfParams { m_cost: 8, t_cost: 1, p_cost: 1 }
    }

    fn key(pass: &str) -> Key {
        Key::derive(&KeySource::Passphrase(pass.into()), &[7u8; 16], fast()).unwrap()
    }

    #[test]
    fn part_round_trip_and_last_flag() {
        let k = key("pw");
        let id = [1u8; 16];
        let f = encrypt_part(&k, &id, 3, true, b"hello").unwrap();
        assert_eq!(f.len(), PART_OVERHEAD + 5);
        assert_eq!(decrypt_part(&k, &id, 3, &f).unwrap(), (b"hello".to_vec(), true));
        let f = encrypt_part(&k, &id, 0, false, b"").unwrap();
        assert_eq!(decrypt_part(&k, &id, 0, &f).unwrap(), (Vec::new(), false));
    }

    #[test]
    fn tamper_reorder_wrong_key_and_other_backup_are_rejected() {
        let k = key("pw");
        let id = [1u8; 16];
        let f = encrypt_part(&k, &id, 1, false, b"data").unwrap();
        assert!(decrypt_part(&key("other"), &id, 1, &f).is_err());
        assert!(decrypt_part(&k, &id, 2, &f).is_err(), "index is bound");
        assert!(decrypt_part(&k, &[2u8; 16], 1, &f).is_err(), "backup id is bound");
        let mut bad = f.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decrypt_part(&k, &id, 1, &bad).is_err());
        assert!(decrypt_part(&k, &id, 1, &f[..10]).is_err());
    }

    #[test]
    fn nonces_are_unique_per_encryption() {
        let k = key("pw");
        let a = encrypt_part(&k, &[0; 16], 0, true, b"x").unwrap();
        let b = encrypt_part(&k, &[0; 16], 0, true, b"x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn manifest_round_trip_binds_header() {
        let k = key("pw");
        let (_, header) = ManifestHeader::new([7u8; 16], fast(), false);
        let f = encrypt_manifest(&k, &header, b"{\"a\":1}").unwrap();
        let parsed = ManifestHeader::parse(&f).unwrap();
        assert_eq!(parsed.kdf, fast());
        assert_eq!(decrypt_manifest(&k, &f).unwrap(), b"{\"a\":1}");
        // KDFパラメータを書き換えても(弱いパラメータへの差し替え)認証で弾かれる。
        let mut bad = f.clone();
        bad[22 + 3] ^= 1;
        assert!(decrypt_manifest(&k, &bad).is_err());
    }

    #[test]
    fn empty_passphrase_is_refused_and_debug_hides_secrets() {
        assert!(Key::derive(&KeySource::Passphrase(String::new()), &[0; 16], fast()).is_err());
        assert!(!format!("{:?}", KeySource::Passphrase("secret".into())).contains("secret"));
    }
}
