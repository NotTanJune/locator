# Changelog

## 0.3.4

- Removed the interactive 1,000-result ceiling and stream all valid indexed, live, and hybrid matches in 500-result batches.
- Added request-aware progressive search updates, cancellation of stale searches, selection preservation, and viewport-only result rendering.
- Added an Apple silicon Finder reveal worker using compiled `NSAppleScript` automation and descriptor arguments.
- Added a selected-row and total-results footer counter.
- Fixed fragmented mouse packets entering the search field and eliminated free-spin direction-reversal latency.
- Fixed Finder reveal panics and delayed Apple-event completion on Apple silicon.

## 0.1.59

- Switched stable Homebrew installs on Apple silicon macOS to the prebuilt release binary.
- Added release automation for Homebrew and Scoop manifest updates.
- Tightened README install, usage, and privacy docs.

## 0.1.41

- Gated Unix-only permission and symlink tests so Windows CI can compile all test targets.

## 0.1.40

- Fixed Linux and Windows `cargo clippy --all-targets -- -D warnings` by marking macOS-native scanner helpers as intentionally unused on non-macOS targets.

## 0.1.39

- Added a comparison table for `lctr` against sist2, Cardinal, fuz, KatSearch, File Find, fsindex, WindFind, cling, and fd.
- Added a Windows PowerShell installer with GitHub Release download and Rust fallback.
- Added a cross-platform release workflow that packages macOS, Linux, and Windows binaries.
- Expanded CI to run on macOS, Linux, and Windows.
- Documented the Windows `winget` path and native NTFS scan optimization plan.

## 0.1.38

- Relicensed locator as GPL-3.0-only for Homebrew core eligibility.
- Added `install.sh` for tap-based Homebrew installation.
- Added `Formula/lctr.rb` in the main repository.
- Updated release URLs to `https://github.com/NotTanJune/locator`.

## 0.1.37

- Renamed the project to `locator`.
- Renamed the CLI command to `lctr`.
- Renamed local indexes from `.locatr` to `.locator`.
- Renamed environment variables from `LOCATR_*` to `LCTR_*`.
- Added release docs, CI, and Homebrew tap preparation.
- Polished `lctr search` with separated status, search, controls, result table, and instant local sorting/filtering.

## Earlier private builds

- Built the scanner, SQLite index, one-shot search, and interactive search TUI.
- Added indexed, hybrid, and live search behavior.
- Added metadata filters, query modes, sorting, and scan progress profiling.
