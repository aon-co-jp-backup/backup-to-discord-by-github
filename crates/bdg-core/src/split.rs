//! 平文ストリームを一定サイズに区切り、パートごとに暗号化する。
//! メモリ使用量はパート1つ分(`part_plain_size`)で、入力の大きさに依存しない。

use crate::crypto::{self, Key, PART_OVERHEAD};
use crate::{Error, Result};
use std::io::Read;

/// 暗号化済みの1パート。
#[derive(Debug)]
pub struct EncryptedPart {
    pub index: u32,
    pub last: bool,
    /// 暗号化後のファイル内容(`BDG1 | nonce | 暗号文+タグ`)。
    pub bytes: Vec<u8>,
    /// このパートに含まれる平文のバイト数。
    pub plain_len: usize,
}

pub struct PartIter<'k, R: Read> {
    reader: R,
    key: &'k Key,
    backup_id: [u8; 16],
    part_plain_size: usize,
    next_index: u32,
    /// 先読みした1バイト(次のパートの先頭)。
    carry: Option<u8>,
    done: bool,
}

impl<'k, R: Read> PartIter<'k, R> {
    /// `max_file_size` は暗号化後の1ファイルの上限(Discordの添付上限より
    /// 余裕を持たせた値)。平文のサイズはそこからオーバーヘッドを引いて決める。
    pub fn new(reader: R, key: &'k Key, backup_id: [u8; 16], max_file_size: usize) -> Result<Self> {
        if max_file_size <= PART_OVERHEAD + 1024 {
            return Err(Error::Config(format!(
                "max_file_size {max_file_size} is too small (needs > {})",
                PART_OVERHEAD + 1024
            )));
        }
        Ok(Self {
            reader,
            key,
            backup_id,
            part_plain_size: max_file_size - PART_OVERHEAD,
            next_index: 0,
            carry: None,
            done: false,
        })
    }

    fn read_part(&mut self) -> Result<EncryptedPart> {
        // 上限+1バイトまで読み、1バイト余れば「まだ続きがある」と分かる。
        let want = self.part_plain_size + 1;
        let mut buf = Vec::with_capacity(want);
        if let Some(b) = self.carry.take() {
            buf.push(b);
        }
        let mut limited = (&mut self.reader).take((want - buf.len()) as u64);
        limited.read_to_end(&mut buf)?;
        let last = buf.len() < want;
        if !last {
            self.carry = buf.pop();
        }
        let index = self.next_index;
        let plain_len = buf.len();
        let bytes = crypto::encrypt_part(self.key, &self.backup_id, index, last, &buf)?;
        self.next_index = index
            .checked_add(1)
            .ok_or_else(|| Error::Config("too many parts".into()))?;
        Ok(EncryptedPart { index, last, bytes, plain_len })
    }
}

impl<R: Read> Iterator for PartIter<'_, R> {
    type Item = Result<EncryptedPart>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let part = self.read_part();
        match &part {
            Ok(p) if p.last => self.done = true,
            Err(_) => self.done = true,
            _ => {}
        }
        Some(part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{KdfParams, KeySource};

    fn key() -> Key {
        Key::derive(
            &KeySource::Passphrase("pw".into()),
            &[1u8; 16],
            KdfParams { m_cost: 8, t_cost: 1, p_cost: 1 },
        )
        .unwrap()
    }

    fn roundtrip(len: usize, max_file: usize) -> usize {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let k = key();
        let id = [9u8; 16];
        let parts: Vec<_> = PartIter::new(&data[..], &k, id, max_file).unwrap().map(|p| p.unwrap()).collect();
        // 最終パートだけがlast。
        assert!(parts.last().unwrap().last);
        assert!(parts[..parts.len() - 1].iter().all(|p| !p.last));
        let mut out = Vec::new();
        for p in &parts {
            assert!(p.bytes.len() <= max_file, "part exceeds max file size");
            let (plain, last) = crypto::decrypt_part(&k, &id, p.index, &p.bytes).unwrap();
            assert_eq!(last, p.last);
            out.extend_from_slice(&plain);
        }
        assert_eq!(out, data);
        parts.len()
    }

    #[test]
    fn boundaries_around_the_part_size() {
        let max_file = PART_OVERHEAD + 2048;
        let plain = 2048;
        assert_eq!(roundtrip(0, max_file), 1, "empty input still yields one (last) part");
        assert_eq!(roundtrip(1, max_file), 1);
        assert_eq!(roundtrip(plain - 1, max_file), 1);
        assert_eq!(roundtrip(plain, max_file), 1, "exact fit must not create an empty trailing part");
        assert_eq!(roundtrip(plain + 1, max_file), 2);
        assert_eq!(roundtrip(plain * 3, max_file), 3);
        assert_eq!(roundtrip(plain * 3 + 5, max_file), 4);
    }

    #[test]
    fn too_small_part_size_is_refused() {
        let k = key();
        assert!(PartIter::new(&b""[..], &k, [0; 16], 100).is_err());
    }

    #[test]
    fn dropping_the_last_part_is_detectable() {
        let k = key();
        let id = [3u8; 16];
        let data = vec![5u8; 5000];
        let parts: Vec<_> =
            PartIter::new(&data[..], &k, id, PART_OVERHEAD + 2048).unwrap().map(|p| p.unwrap()).collect();
        assert!(parts.len() >= 3);
        // 最後のパートを捨てると、残りの末尾は「lastではない」ので検出できる。
        let (_, last) = crypto::decrypt_part(&k, &id, parts[1].index, &parts[1].bytes).unwrap();
        assert!(!last);
    }
}
