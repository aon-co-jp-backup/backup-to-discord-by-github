//! 前回の実行結果(リポジトリごとの参照の指紋)。変更が無いリポジトリを
//! 送り直さないために使う。壊れていても致命的ではない(全部送り直すだけ)。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub repos: BTreeMap<String, RepoState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub fingerprint: String,
    pub last_backup_at: i64,
    pub last_backup_id8: String,
}

impl State {
    pub fn load(path: &Path) -> State {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// 一時ファイルに書いてから置き換える(書き込み途中で落ちても壊れない)。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_corruption_falls_back_to_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        let mut s = State::default();
        s.repos.insert("o/r".into(), RepoState { fingerprint: "f".into(), last_backup_at: 1, last_backup_id8: "abcd1234".into() });
        s.save(&p).unwrap();
        assert_eq!(State::load(&p).repos["o/r"].fingerprint, "f");
        std::fs::write(&p, b"{not json").unwrap();
        assert!(State::load(&p).repos.is_empty());
        assert!(State::load(&d.path().join("missing.json")).repos.is_empty());
    }
}
