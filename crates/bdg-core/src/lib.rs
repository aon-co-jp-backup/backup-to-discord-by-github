//! `bdg-core`: git bundle を暗号化・分割して Discord へ保存し、復元する。
//!
//! Webフレームワークには依存しない(HTTP層は別クレートで RPoem を載せる)。
//!
//! 流れ:
//! 1. [`gitbundle`] が `git bundle` を作る(ミラークローン → bundle)。
//! 2. [`split::PartIter`] が平文を一定サイズごとに区切り、各パートを
//!    XChaCha20-Poly1305 で暗号化する(パートごとに独立した認証付き暗号。
//!    入れ替え・欠落・末尾切り詰めは認証で検出される)。
//! 3. [`discord::DiscordClient`] が各パートを添付ファイルとして投稿し、最後に
//!    暗号化したマニフェストを投稿する。
//! 4. 復元は逆順。マニフェストを探し、パートを取得・検証・復号して連結する。
//!
//! 平文(リポジトリ名・内容)は Discord へ一切送らない。ファイル名も
//! バックアップIDと連番だけ。

pub mod backup;
pub mod crypto;
pub mod discord;
pub mod error;
pub mod github;
pub mod gitbundle;
pub mod manifest;
pub mod split;

pub use error::{Error, Result};
