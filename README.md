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
- 試験: ユニット + RPoem 製モック Discord/GitHub と本物の git を相手にした通し試験(計 31 件)。**本物の Discord での実機確認(2026-10-26)**: `bdg doctor` が全項目 OK、公開リポジトリ1つ(約100KB)を実バックアップ→`list`→`restore`→`git clone` で中身まで一致を確認。大きなリポジトリ(複数パート)・非公開リポジトリ・`bdg-server` の実環境は未確認。手順は下の「実Discord試験」。
- `crates/bdg-server`(コマンド `bdg-server`、RPoem): 管理API(Bearerトークン・レート制限・本文上限)と GitHub Webhook 受信(HMAC署名検証・配送ID重複拒否・対象外リポジトリは無視・連続pushは1回にまとめる)。ジョブは1つずつ実行。RPoem に追加した `webhook` / `body_limit` / `multipart_stream` の実利用。
- **配置済み(2026-10-26)**: VPS の `easy-web.tokyo/backup-to-discord-by-github`(管理API・Webhook)。`bdg-server` は専用ユーザー `bdg` で systemd 稼働(`127.0.0.1:8127`、前面は open-web-server、設定 `deploy/open-web-server/`)。秘密は `/etc/bdg/secrets/`(600)。GitHub Webhook は非公開 9 リポジトリに登録済みで、ping・push の受信と実行まで確認。VPS は Webhook 受信と管理APIのみ(`run_schedule = false`)。
- **定期実行**: Windows のタスク `bdg-backup-to-discord`(毎日 03:30、`bdg run --once`、`deploy/windows/register-task.ps1`)。VPS と PC は別々の状態ファイルを持つので、VPS が Webhook で取った分は PC の次回実行でもう一度送られうる(世代整理で最新3つに収まる)。
- **`open-gitea` の調査結果**: Gitea の Rust+RPoem 版で、git smart HTTP・OTPログイン・アクセス制御・Wiki・WASM UI を持つ。**Webhook(送信)・Issue・PR は未実装**で、流用できるコードは無い。bdg は GitHub を保存元にするため forge は不要。将来 open-gitea を保存元にするなら、`GitSource` の URL に open-gitea のクローン URL を渡すだけで動く見込み(未検証)。
- 未了: 公開 135 リポジトリ(約 2.8GB)を Discord に入れるか(`open-github-backup` が別にミラー済み・規約リスクが増すため保留)、パスフレーズの強化(下記)。

## 使う前に(利用者が行うこと)
Discord の非公開サーバー・Bot の作成、チャンネルへの権限付与(メッセージ送信・ファイル添付・履歴閲覧。メッセージの管理は不要)、Bot トークンと暗号化パスフレーズの用意。いずれも `F:` 直下の txt などに置き、リポジトリへは入れない。

## 実Discord試験(最初の1回)
1. 非公開サーバーとバックアップ専用チャンネルを作る。Bot を作り、そのチャンネルに「チャンネルを見る・メッセージを送信・ファイルを添付・メッセージ履歴を読む」を付ける(メッセージの管理は不要)。
2. Botトークン、暗号化パスフレーズ、(非公開リポジトリ用)GitHubトークンを、それぞれ1行のtxtファイルにして置く(パスフレーズは無くすと復元できないので別の場所にも保管)。
3. `bdg.example.toml` をコピーして `bdg.toml` にし、パスとチャンネルIDを書き換える。
4. `bdg --config bdg.toml doctor` — 小さな暗号化データを送って、一覧・復元・照合・削除まで往復し、段階ごとに OK/NG と原因のヒントを表示する。**すべて OK になってから**次へ。
5. `bdg --config bdg.toml backup --repo owner/name` で1つだけ実バックアップ → `list` → `restore --id <ID> --out x.bundle` → `git clone x.bundle` で中身を確認。
6. 問題なければ `bdg run --once` をスケジューラに登録する(`deploy/windows/register-task.ps1`、`deploy/systemd/`)。

終了コード: 0=成功、1=一部のリポジトリが失敗(他は処理済み)、2=設定・起動エラー。

## bdg-server(管理API / Webhook)
`bdg-server --config bdg.toml` で起動(`bdg.toml` の `[server]` が必要。既定は `127.0.0.1:8787`、TLSなし)。

| メソッド/パス | 認証 | 内容 |
|---|---|---|
| `GET /healthz` | なし | 待ち/実行中のジョブ数 |
| `GET /api/jobs`, `/api/jobs/:id` | Bearer | ジョブ一覧/詳細 |
| `GET /api/targets` | Bearer | バックアップ対象 |
| `GET /api/backups` | Bearer | Discord上のバックアップ(復号して表示) |
| `POST /api/backup` | Bearer | `{"repo":"owner/name","force":false}` でジョブ登録(省略で全対象) |
| `POST /api/prune` | Bearer | `{"keep":3,"dry_run":true}` |
| `POST /webhook/github` | HMAC署名 | pushイベントで該当リポジトリを(デバウンス後に)バックアップ |

GitHub側は、各リポジトリ(または組織)の Webhook に `https://easy-web.tokyo/backup-to-discord-by-github/webhook/github`、Content type `application/json`、Secret に `webhook_secret_file` と同じ値、イベントは `push` だけ、を設定する。
復元(リポジトリの中身を返す操作)は、情報漏えいの面積を増やさないため HTTP には出していない。CLI の `bdg restore` のみ。

## パスフレーズについて
`F:\bdg-passphrase.txt` は暗号の鍵の元です。**短いと総当たりに弱く、失うと復元できません**。長い(20文字以上のランダム)ものを推奨します。
変更すると既存のバックアップは古いパスフレーズでしか読めないため、変えるときは新しいパスフレーズで取り直してから古い世代を `prune` してください。

## API 方針: REST 全廃・gRPC / Connect へ (2026-11-04)

### 日本語
- **REST は全廃する。新しい API は REST で作らない。** ブラウザや `curl` から呼ぶ API は **Connect の単純呼び出し**(`POST /<パッケージ>.<Service>/<Method>`、`content-type: application/json`、本文は JSON)、サービス間は **gRPC / gRPC-web**(RPoem の管理API `open_runo.admin.v1.Admin` と同じ流儀)。ストリーミングは、必要になってから。
- **既存の REST は、触るときに順に移す。** 移したら旧 REST の経路は消す。移行が終わるまで残るが、**新規には追加しない**。
- **JSON は RS-JSON に統一**する(読み込みは `from_str_secure` / `parse_secure` でサイズ・深さ・重複キーの上限つき、書き出しは `rust_json::fast`)。ブラウザの WASM に `prost` は入れない(サイズが増えるため)。
- **例外(自分たちの API ではなく、相手の規格が決めているもの)**: ACME、OAuth/OIDC のコールバック、第三者(GitHub など)からの Webhook、ヘルスチェック、静的ファイル配信、OTLP など。これらは規格どおりの HTTP を受ける。
- 参考実装: `RPoem`(`open_runo.admin.v1.Admin`)、`open-easy-web`(`server/src/connect.rs`、`proto/open_easy_web/admin/v1/admin.proto`)。
- ※ このリポジトリのコードが既に方針どおりかどうかは、各リポジトリの現状による。この節は方針の記録であり、コードの変更を意味しない。

### English
- **REST is being abolished. New APIs are not built as REST.** APIs called from browsers or `curl` use **Connect unary calls** (`POST /<package>.<Service>/<Method>`, `content-type: application/json`, JSON body); service-to-service calls use **gRPC / gRPC-web** (the same style as RPoem's admin API `open_runo.admin.v1.Admin`). Streaming only when it is actually needed.
- **Existing REST endpoints are migrated one by one, when they are touched.** The old REST route is removed once migrated. Until migration is finished they remain, but **no new ones are added**.
- **JSON is unified on RS-JSON**: reading with `from_str_secure` / `parse_secure` (size, depth and duplicate-key limits), writing with `rust_json::fast`. `prost` is not put into browser WASM (it would grow the binary).
- **Exceptions (shapes dictated by someone else's standard, not our own API)**: ACME, OAuth/OIDC callbacks, webhooks from third parties (e.g. GitHub), health checks, static file serving, OTLP, and so on. These accept plain HTTP as the standard requires.
- Reference implementations: `RPoem` (`open_runo.admin.v1.Admin`) and `open-easy-web` (`server/src/connect.rs`, `proto/open_easy_web/admin/v1/admin.proto`).
- Note: whether the code in a given repository already follows this policy depends on that repository. This section records the policy; it does not mean the code has been changed.

### Deutsch (KI-Übersetzung, redaktionell überarbeitet; noch nicht von Muttersprachlern geprüft)
- **REST wird vollständig abgeschafft; neue APIs entstehen nicht mehr als REST.** APIs, die aus dem Browser oder per `curl` aufgerufen werden, verwenden **unäre Connect-Aufrufe** (`POST /<Paket>.<Service>/<Methode>`, `content-type: application/json`, JSON-Body). Zwischen Diensten kommen **gRPC bzw. gRPC-web** zum Einsatz, im selben Stil wie die Admin-API von RPoem (`open_runo.admin.v1.Admin`). Streaming wird erst ergänzt, wenn es tatsächlich gebraucht wird.
- **Bestehende REST-Endpunkte werden schrittweise umgestellt, und zwar immer dann, wenn man ohnehin an ihnen arbeitet.** Sobald ein Endpunkt umgestellt ist, wird die alte REST-Route entfernt. Bis zum Abschluss der Migration bleiben die übrigen bestehen, es werden aber **keine neuen mehr hinzugefügt**.
- **JSON wird einheitlich über RS-JSON verarbeitet**: Gelesen wird mit `from_str_secure` bzw. `parse_secure` (mit Obergrenzen für Größe, Verschachtelungstiefe und doppelte Schlüssel), geschrieben mit `rust_json::fast`. `prost` wird nicht in das Browser-WASM aufgenommen, da es die Datei vergrößern würde.
- **Ausnahmen (Schnittstellen, deren Form ein fremder Standard vorgibt und die nicht zu unserer eigenen API gehören)**: ACME, OAuth/OIDC-Callbacks, Webhooks von Drittanbietern (z. B. GitHub), Health-Checks, die Auslieferung statischer Dateien, OTLP und Ähnliches. Diese nehmen, wie vom jeweiligen Standard verlangt, gewöhnliches HTTP entgegen.
- Referenzimplementierungen: `RPoem` (`open_runo.admin.v1.Admin`) und `open-easy-web` (`server/src/connect.rs`, `proto/open_easy_web/admin/v1/admin.proto`).
- Hinweis: Ob der Code eines einzelnen Repositorys diese Richtlinie bereits erfüllt, hängt vom jeweiligen Repository ab. Dieser Abschnitt dokumentiert lediglich die Richtlinie; er besagt nicht, dass der Code schon angepasst wurde.

### Русский (перевод выполнен с помощью ИИ и отредактирован; носителями языка пока не проверен)
- **От REST мы полностью отказываемся; новые API больше не создаются в стиле REST.** API, к которым обращаются из браузера или через `curl`, используют **унарные вызовы Connect** (`POST /<пакет>.<Service>/<Method>`, `content-type: application/json`, тело запроса — JSON). Для взаимодействия между сервисами применяются **gRPC и gRPC-web** — в том же стиле, что и административный API RPoem (`open_runo.admin.v1.Admin`). Потоковая передача данных добавляется лишь тогда, когда в ней действительно возникает необходимость.
- **Существующие REST-эндпоинты переводятся поэтапно — по мере того, как с ними приходится работать.** После перевода старый REST-маршрут удаляется. Пока миграция не завершена, оставшиеся эндпоинты сохраняются, но **новые не добавляются**.
- **Работа с JSON унифицирована на основе RS-JSON**: чтение — через `from_str_secure` или `parse_secure` (с ограничениями на размер, глубину вложенности и повторяющиеся ключи), запись — через `rust_json::fast`. `prost` в браузерный WASM не включается, поскольку это увеличило бы размер файла.
- **Исключения (интерфейсы, форму которых определяет чужой стандарт, а не наш собственный API)**: ACME, обратные вызовы OAuth/OIDC, вебхуки сторонних сервисов (например, GitHub), проверки работоспособности, раздача статических файлов, OTLP и тому подобное. Такие интерфейсы принимают обычный HTTP, как того требует соответствующий стандарт.
- Эталонные реализации: `RPoem` (`open_runo.admin.v1.Admin`) и `open-easy-web` (`server/src/connect.rs`, `proto/open_easy_web/admin/v1/admin.proto`).
- Примечание: соответствует ли код конкретного репозитория этой политике, зависит от самого репозитория. Данный раздел лишь фиксирует политику и не означает, что код уже изменён.

## API 方針の補足: 用語の整理と使い分け (2026-11-04)

### 日本語
**1つに統一するものではなく、別の層の話です。** 「REST の代わり」になるのは Connect・gRPC・GraphQL だけで、WASM は画面を作る技術です。

| 用語 | 何か | REST の代わりか |
|---|---|---|
| REST | URL と HTTP メソッドで操作を表す API の作り方 | 全廃する側 |
| **Connect** | `POST /サービス名/メソッド名` に JSON を送る形式。`curl` でもブラウザの `fetch` でも呼べる | はい(**採用**) |
| **gRPC** | 同じ「サービス/メソッド」の考え方を、バイナリ(protobuf)と HTTP/2 で行う形式。速く、ストリーミングも得意 | はい(**サービス同士に採用**) |
| gRPC-web | gRPC のブラウザ向けの変形版 | はい(必要なら) |
| GraphQL | 欲しい項目を問い合わせる形式 | 一部だけ(**問い合わせ(読み取り)専用**) |
| WASM | Rust をブラウザの中で動かす仕組み | いいえ(画面を作る技術) |

- **ブラウザの画面や `curl` から呼ぶ API は Connect**(POST、JSON を RS-JSON で読み書き)。ブラウザの WASM は素の gRPC を話せず、Connect なら `fetch` のままで、`prost` を足さずに済む(画面のサイズも増えない)。
- **サービス同士の通信(サーバー間、重い処理、ストリーミング)は gRPC。** 多くは Connect で足りるので、ストリーミングが必要になった所だけ gRPC に上げる。
- **GraphQL は、まとめて問い合わせたいときの読み取り専用。** 書き込み(操作)には使わない。
- **WASM は画面を作る技術として、そのまま使う。** 画面から呼ぶ API が Connect になっただけ。
- 外部の規格が形を決めるもの(ACME、OAuth のコールバック、第三者の Webhook、ヘルスチェック、静的ファイル、OTLP)は、規格どおりの HTTP のまま。
- **迷ったら Connect。**

### English
**These are not alternatives to unify; they sit at different layers.** Only Connect, gRPC and GraphQL can replace REST; WASM is a way to build the UI.

| Term | What it is | Replaces REST? |
|---|---|---|
| REST | API style that expresses operations through URLs and HTTP methods | The thing being abolished |
| **Connect** | `POST /Service/Method` with a JSON body; callable from `curl` and from a browser `fetch` | Yes (**adopted**) |
| **gRPC** | The same service/method idea over binary (protobuf) and HTTP/2; fast, good at streaming | Yes (**for service-to-service**) |
| gRPC-web | A browser-friendly variant of gRPC | Yes (if needed) |
| GraphQL | A query language: ask for exactly the fields you want | Partly (**read-only queries**) |
| WASM | Runs Rust inside the browser | No (a UI technology) |

- **APIs called from browser screens or `curl` use Connect** (POST, JSON read and written with RS-JSON). Browser WASM cannot speak plain gRPC; with Connect it stays a `fetch`, and `prost` is not needed (the binary does not grow).
- **Service-to-service calls (server to server, heavy work, streaming) use gRPC.** Connect is enough in most places; move a call to gRPC only where streaming is actually needed.
- **GraphQL is read-only**, for queries that gather several pieces of data. It is not used for writes (operations).
- **WASM stays as the way to build the UI.** The API it calls simply became Connect.
- Interfaces whose shape an outside standard dictates (ACME, OAuth callbacks, third-party webhooks, health checks, static files, OTLP) stay plain HTTP.
- **When in doubt, use Connect.**

### Deutsch (KI-Übersetzung, sprachlich überarbeitet; noch nicht von Muttersprachlern geprüft)
**Es geht nicht darum, eine einzige Technik auszuwählen – die Begriffe liegen auf unterschiedlichen Ebenen.** Als Ersatz für REST kommen nur Connect, gRPC und GraphQL in Frage; WASM dagegen ist eine Technik, mit der die Benutzeroberfläche gebaut wird.

| Begriff | Was ist das? | Ersatz für REST? |
|---|---|---|
| REST | Ein API-Stil, bei dem Operationen über URLs und HTTP-Methoden ausgedrückt werden | wird abgeschafft |
| **Connect** | `POST /Dienst/Methode` mit JSON im Body; aufrufbar mit `curl` ebenso wie mit `fetch` im Browser | ja (**gewählt**) |
| **gRPC** | Dasselbe Prinzip aus Dienst und Methode, aber über binäres protobuf und HTTP/2; schnell und gut für Streaming geeignet | ja (**zwischen Diensten**) |
| gRPC-web | Eine für den Browser angepasste Variante von gRPC | ja (bei Bedarf) |
| GraphQL | Abfragesprache, mit der man genau die benötigten Felder anfordert | teilweise (**nur für lesende Abfragen**) |
| WASM | Führt Rust-Code direkt im Browser aus | nein (Technik für die Oberfläche) |

- **APIs, die von Browseroberflächen oder per `curl` aufgerufen werden, verwenden Connect** (POST; JSON wird mit RS-JSON gelesen und geschrieben). WASM im Browser kann kein reines gRPC sprechen. Mit Connect bleibt es bei einem gewöhnlichen `fetch`, und `prost` ist nicht nötig – die WASM-Datei wird dadurch nicht größer.
- **Für die Kommunikation zwischen Diensten (Server zu Server, rechenintensive Aufgaben, Streaming) wird gRPC eingesetzt.** In den meisten Fällen reicht Connect aus; auf gRPC wird nur dort umgestellt, wo Streaming tatsächlich gebraucht wird.
- **GraphQL dient ausschließlich lesenden Abfragen**, wenn mehrere Informationen gebündelt abgefragt werden sollen. Für Schreibzugriffe (Operationen) wird es nicht verwendet.
- **WASM bleibt die Technik für die Oberfläche.** Geändert hat sich lediglich, dass die aufgerufene API nun auf Connect beruht.
- Schnittstellen, deren Form durch einen externen Standard festgelegt ist (ACME, OAuth-Callbacks, Webhooks von Drittanbietern, Health-Checks, statische Dateien, OTLP), bleiben gewöhnliches HTTP.
- **Im Zweifel: Connect.**

### Русский (перевод выполнен с помощью ИИ и литературно отредактирован; носителями языка пока не проверен)
**Это не варианты, из которых нужно выбрать один, а технологии разных уровней.** Заменить REST могут только Connect, gRPC и GraphQL; WASM же — это способ создания пользовательского интерфейса.

| Термин | Что это | Заменяет REST? |
|---|---|---|
| REST | Стиль построения API, при котором операции выражаются через URL и HTTP-методы | отменяется |
| **Connect** | `POST /Сервис/Метод` с телом в формате JSON; вызывается как из `curl`, так и из браузерного `fetch` | да (**выбран**) |
| **gRPC** | Тот же принцип «сервис — метод», но поверх двоичного формата protobuf и HTTP/2; работает быстро и хорошо подходит для потоковой передачи | да (**для связи между сервисами**) |
| gRPC-web | Адаптированный для браузера вариант gRPC | да (при необходимости) |
| GraphQL | Язык запросов: клиент запрашивает ровно те поля, которые ему нужны | частично (**только запросы на чтение**) |
| WASM | Позволяет выполнять код на Rust прямо в браузере | нет (технология для интерфейса) |

- **API, которые вызываются из браузерных интерфейсов или через `curl`, строятся на Connect** (POST; JSON читается и записывается через RS-JSON). Браузерный WASM не умеет работать с «чистым» gRPC. С Connect достаточно обычного `fetch`, а `prost` не требуется — размер WASM-файла не увеличивается.
- **Для обмена между сервисами (сервер — сервер, ресурсоёмкие задачи, потоковая передача) применяется gRPC.** В большинстве случаев достаточно Connect; на gRPC переходят только там, где потоковая передача действительно необходима.
- **GraphQL используется только для чтения**, когда нужно получить сразу несколько связанных данных. Для записи (операций) он не применяется.
- **WASM по-прежнему остаётся технологией для интерфейса.** Изменилось лишь то, что вызываемый им API теперь построен на Connect.
- Интерфейсы, форма которых задана внешним стандартом (ACME, обратные вызовы OAuth, вебхуки сторонних сервисов, проверки работоспособности, статические файлы, OTLP), остаются обычным HTTP.
- **Если есть сомнения — выбирайте Connect.**
