# hizuke 0.2 検証記録

## Windows CIのテスト修正

2026-09-21、Windows、Rust 1.98.1で実施しました。MP4対応の検証時に残っていた2件の失敗を解消しました。

- 管理範囲のテストでは、テスト用の操作パスをOS標準の区切り文字で組み立て、親フォルダーの管理状態を明示的に用意しました。パス形式の不一致ではなく、管理範囲の重複が理由で拒否されることを確認します。
- 更新日時のテストでは、Windowsで保存できる100 ns単位の値を使います。リネームで情報が変わらないことに加え、秒を変えずに100 nsだけ更新した場合も変更を検出することを確認します。

| 検証 | 結果 |
| --- | --- |
| `cargo fmt --check` | 成功 |
| `cargo clippy --locked --all-targets -- -D warnings` | 成功・Clippy警告なし |
| `cargo test --locked --no-fail-fast` | 90件成功、失敗・スキップなし |
| `cargo build --release --locked` | 成功・コンパイラ警告なし |

変更はテストコードと検証記録です。macOS/Linuxでの実行とGitHub Actions上での再実行は、この環境では実施していません。

## 警告ゼロ化の検証

2026-09-21、Windows、Rust 1.98.1で実施しました。
`AGENTS.md`に警告ゼロのルールを追加し、`src/engine.rs`の `unused_mut` と `chunks_exact_to_as_chunks` を修正しました。
`cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --release --locked` はすべて成功し、Rustコードのコンパイラ・Clippy警告はゼロです。
`cargo test --locked witness` も2件成功し、記録末尾の欠損検出・修復を確認しました。全体テストとmacOS/Linux実機検証はこの修正では再実行していません。

## MP4対応の追加検証

2026-09-21、Windows、Rust 1.98.1で実施しました。

- MP4の追加テスト9件（パーサー3件、スキャナー・CLI6件）はすべて成功しました。
- 32/64-bitの作成日時、通常・拡張ボックス長、末尾のmoov、UTC/ローカル時刻、日時欠落・不正・切り詰め、再帰走査、重複、同名衝突、再実行、プレビュー、apply/undoによる内容・mtime保持を確認しました。
- 仮想5 GiB動画のテストでは、動画本体を読み込まず末尾の日時へシークすることを確認しました。大量ボックスの探索上限とI/Oエラーの伝播も確認しました。
- `cargo fmt --check`、`git diff --check`、`cargo build --release --locked --offline`、リリースバイナリのヘルプと画像プレビューは成功しました。
- `cargo test --locked --offline --no-fail-fast` は88件成功、既存の2件が失敗しました。変更前のHEADを別ディレクトリへ展開して再実行し、同じ2件の失敗を確認しました。
  - `persistent_fingerprint_survives_rename_but_detects_mtime_change`: テストが123 nsを期待する一方、このWindows環境では100 nsとして保存されます。
  - `nested_collection_boundaries_prevent_stealing_images`: `Store::open(&new_child).is_err()` の既存アサーションが失敗します。
- Clippyは今回の追加箇所に警告なし。`-D warnings`付き実行は、変更していない `src/engine.rs` の `unused_mut` と `chunks_exact_to_as_chunks` により失敗しました。

MP4データはテスト内で生成したメタデータコンテナで、実カメラの動画や再生・デコードの検証ではありません。MP4対応についてのmacOS/Linux実機検証は未実施です。

## 0.2の既存検証

2026-09-16、macOS arm64、Rust 1.95.0 で実施しました。

| 検証 | 結果 |
| --- | --- |
| `cargo fmt --check` | 成功 |
| `cargo clippy --locked --all-targets -- -D warnings` | 成功・警告なし |
| `cargo test --locked` | ライブラリ50件＋引数解析2件＋CLI31件＋形式別3件＋生成シナリオ1件、計87件成功 |
| `cargo build --release --locked` | 成功 |
| リリースバイナリの `--help` | 成功 |
| 実際の端末入力による既定動作・重複選択・競合・キャンセル | 10シナリオ成功 |
| SIGKILLによる中断復旧、SIGINTによる自動復元 | 4シナリオ成功 |
| 旧0.1バイナリが作った履歴を0.2で取り消す | 成功・内容とナノ秒mtimeを保持 |
| 旧名の実行ファイルからの移行（0.1.0・0.2.0） | 完了履歴の取り消しと強制終了後の復旧、計4シナリオ成功 |

名称変更後の検証では、実際の旧 `imgrename` 0.1.0・0.2.0 が作成した `.imgrename` の履歴を、新しい `hizuke` で読み取り・復元しました。通常実行は各4ファイル、SIGKILLで中断した実行は各48ファイルについて、元の名前・SHA-256・ナノ秒mtime・inodeを保持して復元できました。履歴確認と復元プレビューで書き込みがないこと、新しい実行も既存の履歴を使い `.hizuke` を作らないことを確認しています。結果は `tests/rename-compatibility-results.json` にあります。

端末入力テストでは、2番目のファイルを残すと選択した inode が残ること、もう片方が退避されること、`undo` で両方の元の inode が戻ることを検証しました。重複質問での中止と、最終確認での拒否について、履歴を含め一切の変更がないことも確認しています。
引数なし・ディレクトリだけの既定動作、端末上でもJSON指定時は読み取り専用になること、quietでも質問が消えないことも確認しました。stdin/stderrが端末でもstdoutをリダイレクトすると既定は読み取り専用になります。最終確認の待機中に選択済み画像を書き換えた場合や、予定された移動先を別途作成した場合には、ファイルを失わず停止します。

強制終了テストでは、それぞれ48個の一意なテストファイルを用意し、実際のファイル配置を観測して `SIGKILL` を送りました。

- apply の途中：元の名前45個、ステージ内3個
- apply の最終名への移動中：最終名3個、ステージ内45個
- undo の復元中：元の名前3個、ステージ内45個

各ケースで `recover --yes` により48個すべての元の名前・SHA-256・ナノ秒単位のmtimeが復元され、ステージ内が空になることを確認しました。結果は `tests/crash-smoke-results.json` に保存しています。これはプロセス終了の検証であり、物理的な電源断の実験ではありません。
追加のSIGINTケースでは、3ファイルの移動を観測してからシグナルを送信し、手動recoverなしに48ファイルすべてが元に戻り、終了コード130・履歴状態`rolled_back`になることを確認しました。端末テストとシグナルテストはリリースビルドでも実施しています。

ファイル名・拡張子・サブディレクトリ・既存の衝突を変えた生成シナリオでは、4組合計1,040画像について、内容保持、決定的な計画、衝突回避、繰り返し実行の安定性、後から追加した画像の処理を確認しました。

JPEG / TIFF / PNG / WebP / HEICは、kamadak-exifから提供された5つのコンテナと、日時を付加した派生データで検証しています。50種類の切り詰め・破損入力も処理し、パニックや入力の変更がないことを確認しました。出典とライセンスは `tests/fixtures/SOURCES.md` にあります。
これらは合成データと相互運用性テスト用コンテナであり、ユーザーの写真や大規模な実カメラ写真コーパスではありません。各社RAWの実画像コーパス、Linux / Windows 実機でのテストは、この環境では実施していません。対応するCI定義は同梱しています。

## 性能の比較

名称変更前の各バージョンで測定した記録です。異なる内容を持つ小さなファイル4,000件に同じmtimeと拡張子を付け、JSON形式の読み取り専用プレビューを各バージョン3回測定しました。

| バージョン | 中央値 |
| --- | --- |
| 0.1.0 | 1.662秒 |
| 0.2.0 | 0.106秒 |

この条件では約15.7倍です。同一秒の連番探索の改善と並列走査の効果を測ったテストであり、大きな実画像のディスク読み取りや全作業の所要時間に同じ倍率を保証するものではありません。OSキャッシュと実行順序の影響も含みます。全測定値は `tests/benchmark-results.json`、再現スクリプトは `tests/benchmark.py` にあります。

## 再現手順

プロジェクトディレクトリで以下を実行します。PythonスモークテストはmacOS / Linux用で、Python標準ライブラリだけを使います。

```sh
cargo test --locked
cargo build --release --locked
python3 tests/smoke_pty.py target/release/hizuke
python3 tests/smoke_crash.py --binary target/release/hizuke
python3 tests/benchmark.py --binary target/release/hizuke
```

ファイルの読み取り中に他プロセスがメタデータを更新すると、保守的な変更検出によってテスト対象処理が停止することがあります。強制終了テストの初回試行でもこの停止と自動復元が発生したため、最終試行はシステムの一時ディレクトリで行いました。変更検出を緩める修正はしていません。
