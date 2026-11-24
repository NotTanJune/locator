# Changelog

## 0.5.2

- The `lctr find` date filters accept `today`, `yesterday` and relative values (`12h`, `7d`, `2w`, `3mo`, `1y`) as well as `YYYY-MM-DD`. Added `query::parse_date_at` for parsing against a fixed time.

## 0.5.0

- Replaced the SQLite row store and the trigram index with a compact name file. An index is now `index.db` plus `index.names` (lz4 blocks), about 40 times smaller.
- Scans collect their rows in memory and write the index once at the end. Removed the staging database and the writer thread.
- Old indexes convert on first open and the old `index.sqlite` is removed. An index from a newer format is refused with an update hint.
- `lctr find` and the search screen share one matching path with no candidate cap. Size and date filters and sorts read metadata from disk. A query with a path separator matches paths.
- The search screen keeps the name file open and its decoded names in memory between queries.
- `lctr watch` follows file events instead of rescanning, and the watcher keeps files inside renamed or moved-in directories.
- Fixed `scan --em` for roots inside `~/Library`, images skipped for a missing vision model, and semantic `find` filters that ran after the result cut.
- Fixed config loading with one invalid value, `update_check = false`, files named `build` or `dist`, glob `[!a]`, and the shell wrapper's handling of `--depth`.
- Fixed search-screen selection on refresh, held arrow keys, fuzzy highlight on decomposed accents, cursor width, a stuck loading state, and preview panics. JPEG previews decode at reduced size.
- Removed the `index_paths` setting. `--stage-index`, `--no-stage-index`, `--batch-size` and `--writer-queue-batches` are accepted and ignored.
- The test suite no longer writes into the real config directory.

## 0.4.2

- Fixed semantic-search candidate-count overflow with unlimited results and contained worker panics so failures appear in the status line instead of corrupting the terminal.
- Made `Esc` and `Ctrl+C` quit from every search focus and the help overlay.
- Moved preview work and live watcher startup off the input thread, and stopped waiting for watcher I/O during shutdown.
- Replaced per-query migration writes with read-only database connections using the existing search cache and memory settings.
- Bounded result-batch processing between input polls and deferred watcher refreshes until the active search finishes.
- Restored terminal input and screen state before Finder cleanup.
- Added regression tests for blocked previews, watcher shutdown, database writer contention, and exit keys in a real terminal.

## 0.3.6

- Fixed TUI result navigation so the highlighted row moves through the visible viewport before scrolling at its edge, in both directions.
- Preserved the selected path and current viewport position across progressive and refreshed search results.
- Removed blocking terminal capability probes from search startup.
- Prevented queued SGR mouse-motion reports from leaking into the parent shell after exit.

## 0.3.5

- Moved Apple-silicon Finder automation into a persistent helper process so `NSAppleScript` always executes on the helper's main thread.
- Preserved asynchronous TUI completion, one reusable lctr-owned Finder window, item selection, window raising, and session-owned cleanup.
- Added a five-second recovery boundary that terminates a wedged helper and recreates it for pending or subsequent reveals.
- Added helper protocol coverage for arbitrary text paths and error responses.

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
