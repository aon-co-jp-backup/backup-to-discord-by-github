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
設計開始(実装は未着手)。
