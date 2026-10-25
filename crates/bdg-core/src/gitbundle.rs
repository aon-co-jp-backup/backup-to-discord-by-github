//! `git bundle` の作成。リポジトリを `--mirror` でクローンしてから
//! `git bundle create - --all` の標準出力をそのままストリームとして読む
//! (巨大なbundleでもファイルに溜めない)。
//!
//! 認証トークンはコマンドライン引数にも `.git/config` にも残さず、
//! 環境変数経由の `http.extraHeader` で渡す。

use crate::{Error, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[derive(Clone)]
pub struct GitSource {
    /// `https://github.com/owner/name.git` またはローカルパス。
    pub url: String,
    /// 非公開リポジトリ用のトークン(GitHub PAT)。
    pub token: Option<String>,
}

impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource").field("url", &self.url).field("token", &self.token.is_some()).finish()
    }
}

fn git(source: &GitSource) -> Command {
    let mut cmd = Command::new("git");
    // ext:: など任意コマンドを実行できるトランスポートを許さない。
    cmd.env("GIT_ALLOW_PROTOCOL", "file:https:ssh");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(token) = &source.token {
        let basic = base64(format!("x-access-token:{token}").as_bytes());
        cmd.env("GIT_CONFIG_COUNT", "1");
        cmd.env("GIT_CONFIG_KEY_0", "http.extraHeader");
        cmd.env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}"));
    }
    cmd
}

/// bundleを読み出すReader。EOFで `git` の終了状態を検査し、失敗なら
/// `io::Error` を返す(途中で切れたbundleを「完了」として保存しないため)。
pub struct BundleReader {
    child: Child,
    stdout: std::process::ChildStdout,
    finished: bool,
    /// ミラーの置き場。Dropで消える。
    _dir: Option<std::path::PathBuf>,
}

impl Read for BundleReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 && !self.finished {
            self.finished = true;
            let status = self.child.wait()?;
            if !status.success() {
                return Err(std::io::Error::other(format!("git bundle exited with {status}")));
            }
        }
        Ok(n)
    }
}

impl Drop for BundleReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = &self._dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// `workdir` の下にミラーを作り、bundleのReaderを返す(`workdir` は空の
/// 専用ディレクトリであること。Reader の Drop 時にミラーを削除する)。
pub fn open_bundle(source: &GitSource, workdir: &Path) -> Result<BundleReader> {
    if source.url.starts_with('-') || source.url.is_empty() {
        return Err(Error::Config("invalid repository url".into()));
    }
    let mirror = workdir.join("mirror.git");
    let out = git(source)
        .args(["clone", "--mirror", "--quiet", "--"])
        .arg(&source.url)
        .arg(&mirror)
        .output()
        .map_err(|e| Error::Git(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        // stderrにURLやヘッダが入りうるので、トークンを含む可能性のある部分は出さず先頭だけ。
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Git(format!("git clone --mirror failed: {}", redact(&msg, source))));
    }
    let mut child = git(source)
        .arg("-C")
        .arg(&mirror)
        .args(["bundle", "create", "-", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Git(format!("cannot run git bundle: {e}")))?;
    let stdout = child.stdout.take().ok_or_else(|| Error::Git("no stdout from git".into()))?;
    Ok(BundleReader { child, stdout, finished: false, _dir: Some(mirror) })
}

/// リモートの全参照(`git ls-remote`)のSHA-256。前回と同じなら中身は
/// 変わっていないので、バックアップを省ける。空のリポジトリは `None`。
pub fn remote_fingerprint(source: &GitSource) -> Result<Option<String>> {
    use sha2::{Digest, Sha256};
    if source.url.starts_with('-') || source.url.is_empty() {
        return Err(Error::Config("invalid repository url".into()));
    }
    let out = git(source)
        .args(["ls-remote", "--"])
        .arg(&source.url)
        .output()
        .map_err(|e| Error::Git(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Git(format!("git ls-remote failed: {}", redact(&msg, source))));
    }
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None); // 空のリポジトリ(参照が1つも無い)
    }
    let mut lines: Vec<&[u8]> = out.stdout.split(|&b| b == b'\n').collect();
    lines.sort();
    let mut h = Sha256::new();
    for l in lines {
        h.update(l);
        h.update(b"\n");
    }
    Ok(Some(hex::encode(h.finalize())))
}

fn redact(text: &str, source: &GitSource) -> String {
    let mut s: String = text.chars().take(300).collect();
    if let Some(t) = &source.token {
        s = s.replace(t.as_str(), "***");
    }
    s.trim().to_string()
}

/// 依存を増やさないための最小のBase64(標準アルファベット、パディングあり)。
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn debug_hides_token_and_redact_masks_it() {
        let s = GitSource { url: "u".into(), token: Some("ghp_SECRET".into()) };
        assert!(!format!("{s:?}").contains("ghp_SECRET"));
        assert_eq!(redact("fail ghp_SECRET here", &s), "fail *** here");
    }

    #[test]
    fn option_like_urls_are_refused() {
        let dir = std::env::temp_dir();
        let s = GitSource { url: "--upload-pack=evil".into(), token: None };
        assert!(open_bundle(&s, &dir).is_err());
    }
}
