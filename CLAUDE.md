# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                      # dev build
cargo build --release            # release build
cargo test                       # run all tests
cargo test <name>                # run a single test by name
cargo fmt                        # format (CI enforces this)
cargo fmt --check                # check without writing
cargo clippy --all-targets -- -D warnings  # lint (CI enforces -D warnings)
cargo run -- scan ~/some/dir     # run lctr scan
cargo run -- search              # run lctr TUI search
```

CI also runs `scripts/test-update-release-manifests.sh` (Unix only) to validate the Homebrew formula and WinGet manifest update scripts.

## Releasing

Releases are tag-triggered. The tag must match `version` in `Cargo.toml` exactly.

1. Bump `version` in `Cargo.toml`
2. Update `release-notes.md`
3. Run `cargo generate-lockfile` and commit `Cargo.lock` (version must match or `--locked` fails in CI)
4. Commit and push to main
5. `git tag v<version> && git push origin v<version>`

## Architecture

The binary is `lctr` (`src/main.rs`); the library crate is `locator` (`src/lib.rs`). All subcommands are defined with clap derive in `main.rs` (the `Commands` enum) and dispatch into library functions.

Subcommands: `scan <root>` (build index), `search` (interactive TUI), `find` (non-interactive query, scriptable output), `watch <root>` (poll-refresh the index every 5s via `src/watch.rs`), `status`, `roots` / `remove-root` (manage indexed roots), `delete-index`, `vacuum`, `shell-init` / `setup-shell` (install the `lctr`-cd shell wrapper).

### Index storage

SQLite via `rusqlite`. `src/db.rs` owns all schema, queries, and DB open helpers. Index location priority:

1. Nearest `.locator/index.sqlite` walking up from the current directory
2. Fallback: `~/Library/Application Support/locator/index.sqlite` (macOS) or OS equivalent via `dirs`

Scans use a staged write strategy: data is written to a temp staging DB, then atomically copied to the final path on completion. `ScanCompletion` (Complete / Incomplete / Unknown) is persisted so the TUI can detect interrupted scans.

Full-text search uses an FTS5 virtual table `files_fts` with the **trigram** tokenizer (`recreate_fts_if_needed`), which makes substring (`Contains`) queries index-accelerated instead of a full-table `LIKE` scan — the dominant search cost on large indexes. By default it indexes **`name` only** (filenames); the `index_paths` config setting switches it to `name`+`path` for directory-substring search at the cost of a much slower scan-finish (paths are ~7× more text → trigram build dominates the "optimize" phase). The FTS table and its sync triggers (`create_fts_triggers`) are rebuilt only when columns mismatch (`fts_columns`/`fts_sql_has_path`/`fts_index_paths`) — never on every open, which would race concurrent writers (the live watcher). The search query (`files_fts MATCH`, via `trigram_match`) is column-agnostic; queries <3 chars fall back to indexed `name_lower LIKE`. Opening an older index transparently rebuilds the FTS. Per-stage search timing: `LCTR_TIMING`.

Search hydration (`hydrate_search_results`, one `symlink_metadata` per row) runs **after** sort+truncate, so only the returned page is stat'd, not the whole `candidate_limit` pool — critical on large indexes.

### Search modes (TUI)

`tui.rs` selects a `SearchBackend` variant before entering the event loop:

- `Indexed` — full index exists; queries go to SQLite only.
- `Hybrid` — scan was interrupted; SQLite results first, then live `jwalk` walk fills gaps.
- `Live` — no index at all; pure filesystem walk via `src/live_search.rs`.

The `SearchWorker` (inner struct in `tui.rs`) runs searches on a background thread and sends results back over a channel. The event loop calls `try_recv` each tick.

### Query engine

`src/query.rs` is the shared matching/sorting layer used by both the TUI and `find`. `QueryMode` (Contains / Exact / Prefix / Suffix / Fuzzy / Regex / Glob) governs how a query matches; `SortField` (Relevance / Name / Path / Kind / Size / Created / Modified) governs ordering. Both are cycled in the TUI and exposed as flags on `find`. Keep SQLite-backed and live-walk results consistent by routing both through this module.

Compile a query once per search with `CompiledQuery::compile(mode, query)` and reuse it across candidates (regex/glob compile a single time; Fuzzy uses the `nucleo-matcher` SIMD matcher). `CompiledQuery::match_positions` returns char indices for highlighting, and `fuzzy_rank` returns nucleo scores used to order Fuzzy + Relevance results. `QueryScorer` holds the reusable nucleo `Matcher` + buffers; build one per search (or per draw) and thread it through. `QueryMode::matches` is a one-shot convenience wrapper over these.

### Live index watcher

`src/live_index.rs` — while the search TUI is open on an `Indexed`/`Hybrid` index, `LiveIndex::spawn(root, db_path)` runs a `notify` filesystem watcher on a background thread. Events are debounced (~400ms) and applied as per-path upserts (`Database::upsert_file`) or soft-deletes (`Database::mark_path_deleted`) against the same WAL SQLite file the search reads. A generation counter is bumped per applied batch; the TUI re-runs the current query when it changes so new/removed files surface without a keystroke. The `w` key toggles the watcher; it falls back to the existing `Hybrid` live-backfill when the watcher can't start.

### Preview pane

`src/preview.rs` — `preview_for(path, max_lines)` returns a `Preview` (syntax-highlighted text via `syntect`, decoded image via the `image` crate, extracted PDF text via `pdf-extract`, directory listing, or binary/metadata fallback). The TUI (`tui.rs`) shows it in a right-hand pane split off the results area when width ≥ ~100 cols, caches it per selected path, and renders images inline with `ratatui-image` (auto-detected kitty/iterm2/sixel protocol, half-block fallback). Syntax sets/themes load once via a `OnceLock`.

### Frecency

Opening/revealing/copying a result records an access in the `access` table (`open_count`, `last_opened_at`). `Database::search_with_options` blends a recency-weighted open count into Relevance ordering (boost, not override). Schema is created in `migrate()`.

### Scanner

`src/scanner.rs` walks the filesystem with `jwalk` (parallel), batches `FileRecord`s, and sends them to a writer thread that bulk-inserts into SQLite. Backends: `ParallelWalk` (jwalk parallel walker, the default — fastest on large trees in benchmarks), `Dirent` (directory-entry metadata), `Native` (macOS `getattrlistbulk`), and `Auto`. Pick via `--backend`. Progress is reported via `emit_progress` to a shared `latest_progress` cell.

`src/scan_ui.rs` renders the live animated scan dashboard (separate from the search TUI) that consumes that progress while a scan runs.

### TUI

Built with `ratatui` + `crossterm`. Layout: vertical split into top chrome (status + search bar + controls panels) / results list / detail panel. Theme (Default / Ocean / Mono) is persisted to `dirs::config_dir()/locator/theme` (theme defs in `src/tui/theme.rs`). Update-check banner is prepended as a `Constraint::Length(1)` row only when a newer version is detected. Result actions (open file, reveal in Finder, copy path) live in `src/open.rs`.

### Config

`src/config.rs` — persistent settings in `<config_dir>/locator/config.toml` (serde + toml). Keys: `icons`, `theme`, `backend`, `preview`, `update_check`, `index_paths`, each with `key_label`/`key_description`/`key_choices` metadata. `lctr config` with no subcommand opens an interactive settings TUI (`src/config_ui.rs`: ratatui editor showing each setting's choices + description, theme previews live, save with `s`). Scriptable subcommands remain: `lctr config list` (plain text), `get`/`set <key> [value]`, `path`, `reset`. Precedence: explicit flag / env var > config > built-in default (e.g. `LCTR_ICONS` overrides `config.icons`; `--backend` overrides `config.backend`; the `t`-key theme file overrides `config.theme` via `Theme::load_with_default`).

### Update check

`src/update_check.rs` — non-blocking, cached GitHub release check. Hits `api.github.com/repos/NotTanJune/locator/releases/latest` via `ureq` (2s timeout) at most once per 24h. Cache at `dirs::config_dir()/locator/update_check`. Opt out: `--no-update-check` flag (persists a marker file) or `LCTR_NO_UPDATE_CHECK=1` env var.

### Key env vars

| Var | Effect |
|---|---|
| `LCTR_DB` | Override DB path |
| `LCTR_DATA_DIR` | Override data directory |
| `LCTR_NO_UPDATE_CHECK` | Disable release check |
| `LCTR_SHELL` | Override shell for setup-shell |
| `LCTR_ICONS` | Set to `1`/`true` to show Nerd Font file-type icons in results (default off) |
| `LCTR_TIMING` | Set to print per-stage search timing (sql/hydrate/filter/frecency/total) to stderr |
