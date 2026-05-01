# Changelog

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
