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
- `crates/bdg-core`: 暗号化(XChaCha20-Poly1305、パートごと独立・AADにID/連番/最終フラグ)、Argon2id、分割、`git bundle` のストリーミング作成、Discord REST(429/5xx再試行・失敗時の掃除)、復元(SHA-256・連番・最終パート・合計サイズ検証)、世代整理(`prune`)、GitHub一覧(公開・非公開)、変更検知(`git ls-remote` の指紋)。
- `crates/bdg-cli`(コマンド `bdg`): `doctor` `backup` `list` `restore` `prune` `run`。定期実行は `bdg run --once` をOSのスケジューラから呼ぶ(`deploy/` に systemd timer と Windows タスクの雛形)か、`bdg run` を常駐させる。前回から参照が変わらないリポジトリは送らない。1リポジトリの失敗で全体を止めない。
- 試験: ユニット + RPoem 製モック Discord/GitHub と本物の git を相手にした通し試験(計 31 件)。**本物の Discord での実機試験は未実施**: 手順は下の「実Discord試験」。
- 未実装: HTTP/Webhook 層(RPoem)、`easy-web.tokyo` への配置。

## 使う前に(利用者が行うこと)
Discord の非公開サーバー・Bot の作成、チャンネルへの権限付与(メッセージ送信・ファイル添付・履歴閲覧・メッセージ管理)、Bot トークンと暗号化パスフレーズの用意。いずれも `F:` 直下の txt などに置き、リポジトリへは入れない。

## 実Discord試験(最初の1回)
1. 非公開サーバーとバックアップ専用チャンネルを作る。Bot を作り、そのチャンネルに「チャンネルを見る・メッセージを送信・ファイルを添付・メッセージ履歴を読む・メッセージの管理」を付ける。
2. Botトークン、暗号化パスフレーズ、(非公開リポジトリ用)GitHubトークンを、それぞれ1行のtxtファイルにして置く(パスフレーズは無くすと復元できないので別の場所にも保管)。
3. `bdg.example.toml` をコピーして `bdg.toml` にし、パスとチャンネルIDを書き換える。
4. `bdg --config bdg.toml doctor` — 小さな暗号化データを送って、一覧・復元・照合・削除まで往復し、段階ごとに OK/NG と原因のヒントを表示する。**すべて OK になってから**次へ。
5. `bdg --config bdg.toml backup --repo owner/name` で1つだけ実バックアップ → `list` → `restore --id <ID> --out x.bundle` → `git clone x.bundle` で中身を確認。
6. 問題なければ `bdg run --once` をスケジューラに登録する(`deploy/windows/register-task.ps1`、`deploy/systemd/`)。

終了コード: 0=成功、1=一部のリポジトリが失敗(他は処理済み)、2=設定・起動エラー。
