# Changelog

## Unreleased

- Added MP4 discovery and movie-header creation dates (32/64-bit `mvhd`), local/UTC conversion, and mtime fallback without external video tools.
- Added MP4 date counts to preview/JSON summaries and documented MP4 timestamp semantics.

## 0.2.0

- Renamed the project, executable, crate, documentation, and shell completions to `hizuke`.
- New collections use `.hizuke` for recoverable state. Existing `.imgrename` histories remain usable in place; ambiguous collections containing both state directories are rejected.
- Added the default `hizuke [DIRECTORY]` workflow: terminal review and confirmation, read-only preview otherwise. No arguments selects the current directory.
- Added adaptive progress/color, `NO_COLOR`, quiet/verbose output, grouped warnings, and byte/count/date summaries.
- Added cooperative interrupt handling: apply rolls back on the first interrupt; interrupted undo/recover remains resumable. Cancellation exits with code 130.
- Added JSON runtime errors, explicit schema versions, dry-run restoration, and detailed transaction inspection.
- Bounded scan concurrency to at most four workers while preserving deterministic results and full-content verification.
- Removed quadratic same-second collision allocation and quadratic journal phase validation.
- Added overlapping-directory coordination and independent collection boundaries.
- Made history fully read-only, added durable journal-tail digests and stronger state validation, and retained v0.1 journal reading/upgrading.
- Expanded validation to 87 Rust tests, ten terminal scenarios, four interruption scenarios, five EXIF container formats, malformed input, generated planning cases, and legacy-name state compatibility.
- Bundled shell completions, third-party notices, and reproducible benchmark/validation scripts.

## 0.1.0

Initial EXIF-first CLI with mtime fallback, exact duplicate decisions, no-clobber renaming, undo, and crash recovery.
