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

CI also runs `scripts/test-update-release-manifests.sh` (Unix only) to validate the Homebrew formula and Scoop manifest update scripts.

## Releasing

Releases are tag-triggered. The tag must match `version` in `Cargo.toml` exactly.

1. Bump `version` in `Cargo.toml`
2. Update `release-notes.md`
3. Run `cargo generate-lockfile` and commit `Cargo.lock` (version must match or `--locked` fails in CI)
4. Commit and push to main
5. `git tag v<version> && git push origin v<version>`
6. `release.yml` builds two archives per platform: the lean `lctr-<target>` and the full `lctr-full-<target>`, which has the text model inside (`--features bundled-model`, `LCTR_BUNDLED_MODEL` set to the downloaded, SHA-256-checked model). The update-manifests job reads only the lean archives, and `scripts/test-update-release-manifests.sh` checks that the full archive is ignored. The release notes section must start with the heading `# lctr v<version>`; the publish job fails without it.

## Architecture

The binary is `lctr` (`src/main.rs`); the library crate is `locator` (`src/lib.rs`). All subcommands are defined with clap derive in `main.rs` (the `Commands` enum) and dispatch into library functions.

Subcommands: `scan <root>` (build index), `search` (interactive TUI), `find` (non-interactive query, scriptable output), `watch <root>` (poll-refresh the index every 5s via `src/watch.rs`), `status`, `update` (update through the detected installation method), `roots` / `remove-root` (manage indexed roots), `delete-index`, `vacuum`, `shell-init` / `setup-shell` (install the `lctr`-cd shell wrapper).

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

`src/update_check.rs` — non-blocking, cached GitHub release check. Hits `api.github.com/repos/NotTanJune/locator/releases/latest` via `ureq` (2s timeout) at most once per 24h. Cache at `dirs::config_dir()/locator/update_check`. Opt out: `--no-update-check` flag (persists a marker file) or `LCTR_NO_UPDATE_CHECK=1` env var. `lctr update` downloads the matching prebuilt GitHub release for Cargo-installed binaries, updates the Windows direct-installer location the same way, delegates to Homebrew, and prints the Scoop command for Scoop installs, based on the executable path.

### Key env vars

| Var | Effect |
|---|---|
| `LCTR_DB` | Override DB path |
| `LCTR_DATA_DIR` | Override data directory |
| `LCTR_NO_UPDATE_CHECK` | Disable release check |
| `LCTR_SHELL` | Override shell for setup-shell |
| `LCTR_ICONS` | Set to `1`/`true` to show Nerd Font file-type icons in results (default off) |
| `LCTR_TIMING` | Set to print per-stage search timing (sql/hydrate/filter/frecency/total) to stderr |

## Embeddings

### Modules

- `src/embed/mod.rs` re-exports the public functions. `model.rs` downloads and verifies the model files.
- `runtime.rs` loads llama.cpp (text and vision) and implements the `Embedder` trait. `mixed_batch.rs` holds the mixed text and image batch.
- `chunk.rs` extracts text and splits it into chunks (1800-character target, 200-character overlap, at most 128 chunks per file). `skip.rs` holds the never-read rules and the low-value rules.
- `pipeline.rs` runs the pass (stages, batching, dedupe, cancel). `store.rs` owns the sidecar. `search.rs` runs semantic queries. `vector.rs` encodes f16 and normalizes. `projection.rs` projects vectors for the galaxy screen.
- `reference.rs`, `image_sizing.rs`, `bench.rs`, and `selftest.rs` are for checks and measurements. `cli.rs` holds the handlers for `scan --em` and `lctr embed`. `fake.rs` is the test embedder.
- `src/embed_ui.rs` is the galaxy screen and the summary card.

### Sidecar

The vectors live in `index.embeddings.sqlite` next to the filename index (`sidecar_path_for_index`). They are kept out of the filename index on purpose: the filename index keeps its schema, and `lctr delete-index` removes both files. Tables: `meta` (model id, dims, complete flag per root), `embed_files` (stamp, status, `chunks_total`, `depth_done`), and `chunks` (f16 vector, snippet, ordinal, unique per path and ordinal). The file is created with mode 0600.

### Passes

`STAGE_ENDS` in `pipeline.rs` is `[8, 32, 128]`. Pass N embeds the chunk ordinals below `STAGE_ENDS[N-1]` for every file that has them. Pass 1 covers every file before pass 2 starts. `needs_embedding` decides pass 1 work (new, changed, or failed files); `needs_deeper` decides later passes. `write_stage` replaces a file at ordinal 0 and appends after that; it refuses a pass that does not start at the stored depth. Each file is written in one transaction per pass, so a cancelled run keeps every committed file. Ordinals in code count from 0; user-facing text counts chunks from 1 (`pass_range_text`).

### Mixed batch

`mtmd_helper_eval_chunks` decodes chunk by chunk, and each decode pools its own tokens. The pooled vector then keeps only the last chunk, so every image got the same vector. `embed_mixed` in `mixed_batch.rs` puts the text tokens and the encoder rows into one `llama_batch_ext` and calls `llama_process` once, which is how DigUp does it. The raw pointers it needs come from accessors that the llama-cpp-2 fork adds.

### Embedder and fake

`with_embedder` takes a closure. The model and context are created and freed inside the call, so no embedder handle outlives a scan. `HashEmbedder` (`fake.rs`) is a deterministic stand-in. `LCTR_EMBED_FAKE=1` switches to it, and most tests use it.

### MODEL_ID

`MODEL_ID` in `pipeline.rs` fingerprints the vectors. Change it whenever the same input would give a different vector: a different model file, a different token cap (`MAX_INPUT_TOKENS`), a different image size, or a different batching path. `ensure_compatible` clears the sidecar when the id changes. The current value is `embeddinggemma-2-Q8_0-img140-tok2048-imgfix1`.

### Embedding env vars

| Var | Effect |
|---|---|
| `LCTR_EMBED_FAKE` | `1` uses the fake embedder (tests and CI) |
| `LCTR_HF_BASE` | Base URL for model downloads (default Hugging Face) |
| `LCTR_EM_PLAIN` | `1` prints plain progress even on a terminal |
| `LCTR_TIMING` | Prints the embedding pass timing: model load, wall, GPU busy, tokens, chunks, per-pass wall, chunks and tokens, and time to the end of pass 1 |
| `LCTR_BUNDLED_MODEL` | Build variable: absolute path to `embeddinggemma-2-Q8_0.gguf`, needed with `--features bundled-model` |

### Config keys

- `embed_dims`: `128`, `256`, `512`, or `768` (default `768`). Changing it re-embeds on the next `--em`.
- `embed_depth`: `quick`, `balanced`, or `deep` (default `quick`). `lctr scan --depth` overrides it.

### Dependency fork

`llama-cpp-2` and `llama-cpp-sys-2` come from `https://github.com/NotTanJune/llama-cpp-rs` at commit `ce4339952a272a9ca306ad718c09dc5c055e9049`, through `[patch.crates-io]` in `Cargo.toml`. The fork pins llama.cpp `9cf967a04`, which has the EmbeddingGemma 2 architecture. It adds three raw pointer accessors (`raw_context_ptr`, `raw_mtmd_ptr`, `raw_model_ptr`) and a timing field fix. Drop the patch once a llama-cpp-2 release includes all three: delete the `[patch.crates-io]` section, then update only those two packages in `Cargo.lock`.

### Build requirements

cmake, libclang (bindgen in `llama-cpp-sys-2`), and a C++ toolchain. llama.cpp compiles from source the first time, which takes minutes. On Windows, set `LIBCLANG_PATH` to the LLVM `bin` folder. CI caches the build with `Swatinem/rust-cache`.

### Reference test

`cargo test --test embed_reference_tests -- --ignored reference_vectors` embeds the DigUp reference inputs (text and images) with the real models and requires every cosine to be at least 0.999. It needs the text and vision models (`lctr embed fetch --vision`) and skips with a message if they are missing. `lctr embed selftest --reference` runs the same check. The reference data is in `tests/fixtures/digup-reference/` (see its `NOTICE.md`).

### Test env lock rule

Opening a `Database` reads the global user config through `fts_index_paths`. A test that changes `LCTR_CONFIG_DIR` or `LCTR_DATA_DIR` therefore races every other test in the same binary that opens a database. `tests/db_tests.rs` has `env_guard()`, and every test in that file takes it once. Any new test that changes a `LCTR_*` variable, or that opens a database in a binary where another test changes one, must take the same guard. Each integration test file is its own process. Unit tests under `src/` have no environment changes, so they need no guard.

