# backup-to-discord-by-github

GitHub のリポジトリ(公開・非公開の両方)を、`git bundle` にして**クライアント側で暗号化**し、
小さく分割して**非公開の Discord サーバー**へ自動投稿するバックアップサービス。
運用先: `easy-web.tokyo/backup-to-discord-by-github`

## 方針
- 開発: Rust + [RPoem](https://github.com/aon-co-jp/RPoem)。RPoem に足りない機能(ストリーミング multipart・Webhook 署名検証・ボディ上限・認証/キー別レート制限)は RPoem 側を同時に拡張して使う。
- バックアップ対象: 公開・非公開の両方。**Discord へ送る前に必ず暗号化**する(平文は送らない)。
- 保存先の Discord サーバーは**非公開**。
- コアロジック(分割・暗号化・Discord 送信・復元)はフレームワーク非依存のクレートに置く。
- 改造元の参考: [open-gitea](https://github.com/aon-co-jp/open-gitea)。

## 注意
Discord をファイル保存先として使うことは利用規約上のリスクがあり、アカウント/Bot が停止されるとデータを失い得る。
**唯一のバックアップにせず**、`open-github-backup`(GitHub→`runo-aruaru`)の補助として運用する。

## 状況
- `crates/bdg-core`(フレームワーク非依存のコア)を実装済み: XChaCha20-Poly1305 でパートごとに暗号化(AAD に バックアップID・連番・最終フラグ)、Argon2id によるパスフレーズ鍵、`git bundle` のストリーミング作成、Discord REST クライアント(429/5xx 再試行・失敗時の掃除)、マニフェスト、復元(SHA-256・連番・最終パート・合計サイズを検証)。
- 試験: ユニット 12 件 + RPoem 製モック Discord を相手にした通し試験 5 件(実 git リポジトリの往復、改ざん検出、レート制限、平文が Discord 側に出ないこと)。**本物の Discord での実機試験は未実施**(Bot とサーバーが必要)。
- 未実装: HTTP/Webhook 層(RPoem)、CLI、定期実行、保持期間(古いバックアップの削除)、`easy-web.tokyo` への配置。

## 使う前に(利用者が行うこと)
Discord の非公開サーバー・Bot の作成、チャンネルへの権限付与(メッセージ送信・ファイル添付・履歴閲覧・メッセージ管理)、Bot トークンと暗号化パスフレーズの用意。いずれも `F:` 直下の txt などに置き、リポジトリへは入れない。
