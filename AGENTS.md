# 開発ルール

## 警告をゼロにする

- RustのビルドとClippyは、エラーだけでなく警告もゼロで完了させる。変更箇所に限らず、検証で見つかった既存の警告も修正する。
- 警告を隠すためだけの `#[allow(...)]`、`-A warnings`、`--cap-lints allow` などは使わず、原因を修正する。
- Windows / macOS / Linuxの条件付きコンパイルを考慮する。特定OSでしか必要ない `mut` やimportは、そのOSのコード内に限定する。
- `Cargo.toml` の最低Rustバージョンとの互換性を維持する。
- Rustコード変更後は次のチェックを実行し、警告・エラーがないことを確認する。

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

- 変更内容に応じて既存テストも実行する。検証結果には実行環境と、未実施・失敗したチェックを明記し、未確認の結果を成功と扱わない。
