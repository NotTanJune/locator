# lctr v0.4.2

Fixes unresponsive search sessions and makes the exit keys consistent.

## Search responsiveness

- File reads, PDF extraction, image decoding, and text highlighting for previews run on a background worker. A slow preview no longer blocks keyboard input. Queued previews skip superseded selections.
- Filesystem watcher startup runs in the background. Quitting requests watcher shutdown without waiting for an in-flight filesystem or database operation.
- Indexed and hybrid queries open read-only database connections instead of repeating migration writes for each query. Readers retain the existing cache and in-memory temporary-storage settings and see staged index replacements on subsequent queries.
- Search-result batches yield to keyboard input. Filesystem changes wait for the active search to finish before scheduling a refresh, rather than continually cancelling it.

## Exit keys

- `Esc` and `Ctrl+C` quit from the search field, results list, or help overlay, including when a query is present.
- `Esc` no longer clears the query or returns focus to search. Use `/` or `Tab` to return from the results list to the search field.
- The terminal returns to its normal screen and input mode before waiting for Finder cleanup.

## Semantic search

- Fixed candidate-count overflow when interactive semantic search requests unlimited results. Candidate counts saturate and are capped to the number of indexed chunks.
- A Rust panic in the semantic worker is caught without printing over the search interface. The active query receives an error status instead of remaining stuck on "searching". The failed model session is dropped; filename search and exit keys remain available. Restart lctr to retry semantic search after an internal failure.

## Compatibility

- No database schema change, migration, new dependency, or new configuration key is required.
- Terminal regression checks cover both exit keys across indexed, live, and semantic search, including missing-model errors, with terminal restoration verified. Large-directory performance depends on the filesystem and query.

# lctr v0.4.1

Makes `scan --em` skip build output and Photos library internals, and fixes a PDF crash.

## Embedding

- Xcode build folders with a suffix (`DerivedData-mac`, `DerivedData-ios`, and so on) are skipped as build output. Before, only a folder named exactly `DerivedData` was skipped.
- Inside a `.photoslibrary` package, `database/` (the library's own search index) and `derivatives/` (resized copies of photos) are skipped. Originals are still embedded. Photos stored only as HEIC are no longer searchable by image content, because their JPEG copies lived in `derivatives/`.
- A file that an older version embedded and that a skip rule now covers is marked skipped on the next `--em` run, and its chunks are removed from the embeddings index.
- On a home folder scan with many Xcode projects and a large Photos library, these rules removed about two thirds of the chunks.

## Fixes

- A malformed PDF no longer prints a `pdf-extract` panic message over the scan display. The file is recorded as failed and the scan continues, as before.
- Previewing a malformed PDF in `lctr search` shows "pdf text could not be extracted" instead of crashing search.

# lctr v0.4.0

Adds semantic search over file contents and fixes several issues in the search and preview code.

## Semantic search

- `lctr scan <dir> --em` embeds the text of documents, code, and notes, and the pixels of images, with EmbeddingGemma 2 (Google DeepMind, Apache 2.0). The model runs locally through llama.cpp. Results are stored in a separate file next to the search index. That file keeps a short plain-text snippet of each chunk, so lctr writes a `.gitignore` containing `*` into every `.locator` folder it writes an index to, and a scanned git project does not pick up the index.
- Search by meaning in `lctr search` by pressing `m` until the mode reads `semantic`. In scripts, use `lctr find "<query>" --mode semantic`; `--output jsonl` adds a score and a snippet to each result.
- Depth is set with `--depth quick|balanced|deep` or the `embed_depth` setting. The default is `quick`, which embeds the first 8 chunks of every file (pass 1). `balanced` adds chunks 9 to 32 (pass 2), and `deep` adds chunks up to 128 (pass 3). A deeper run embeds only the chunks that are missing.
- Each pass makes search useful for more of each file. Search works after pass 1 finishes; the later passes add depth.
- New subcommands: `lctr embed status`, `lctr embed fetch [--vision]`, and `lctr embed remove-model [--yes]`.
- Images are embedded with a separate vision model (555 MB), downloaded the first time a scan finds an image.
- Files whose names look like keys or credentials are never read (for example `.pem`, `.key`, `.p12`, `.env`, `id_rsa`, `credentials`). Nothing under a hidden folder is read. A text file that contains a private key block or an AWS access key id in its first 8 KiB is skipped.

## Speed and requirements

- Embedding speed is set by the model. On an M2 under load the pass runs at about 5 to 6 chunks per second with the GPU fully busy. That is why `quick` is the default: search works after the first pass, and the rest can wait.
- Apple Silicon Macs run the model on Metal. Other systems run it on the CPU, which is much slower.
- The first `--em` run downloads the text model (about 310 MB) to the lctr data folder.
- The prebuilt binaries cover Apple Silicon macOS, Linux x86_64, and Windows x86_64. Intel Macs are not covered; build from source there.
- Building from source needs cmake, libclang (for bindgen), and a C++ toolchain.

## Downloads

- `lctr` is the lean binary. It downloads the model on first use.
- `lctr-full` has the text model inside, so the first `--em` run needs no download. Its archive is about 310 MB larger.
- Homebrew and Scoop install the lean binary. `lctr update` keeps the variant you installed.

## Open, reveal, and update

- Open and reveal are added for Linux (through `xdg-open`) and Windows (through Explorer). They have been tested on macOS only; please report problems on Linux or Windows. A failed action shows a message in the status line and no longer exits search.
- `lctr update` on Windows detects Scoop installs and points them to `scoop update`. Direct installs update from the release asset. The WinGet instruction is removed because no WinGet package exists.

## Preview

- Symbolic links are followed for the size limits, so a link to a large file cannot bypass them.
- FIFOs and device files are not read.
- Directory previews list at most 5,000 entries.
- `preview = false` in the config turns previews off.

## Search and index

- Open history (frecency) is kept when a scan rebuilds the index.
- The live watcher matches paths relative to the scanned root and deletes whole subtrees exactly.
- The live watcher no longer stalls while files in the watched folder are being opened or read. It now ignores access-only events and applies a batch after at most 2 seconds, so a steady stream of events cannot hold back index updates. This showed up on Linux, where reads count as events.
- Search keeps running when one indexed path cannot be read.

## Compatibility

- The search index format is unchanged. The embeddings file is new and is created only by `--em`.
- Two config keys are new: `embed_dims` (768 by default) and `embed_depth` (`quick` by default).

# lctr v0.3.6

A responsiveness and terminal cleanup release for interactive search.

## Search interaction

- The highlighted result now moves through the visible rows before the viewport scrolls at the top or bottom edge.
- Upward and downward navigation stop at the first and last result instead of wrapping.
- The selected path and current viewport position remain stable while progressive or live results refresh.

## Startup and terminal cleanup

- Search no longer waits for keyboard or graphics capability responses before drawing its first frame.
- Image previews use the half-block renderer without a blocking terminal graphics probe.
- Queued SGR mouse-motion reports are discarded during teardown, so exiting search leaves a clean parent-shell prompt.

## Compatibility

- No database migration or schema change is required.
- No new dependency is required.
- The macOS arm64 binary remains M1-compatible.

# lctr v0.3.5

A reliability release for Finder reveal on Apple silicon.

## Finder reveal

- Finder automation now runs in a persistent helper process whose compiled `NSAppleScript` stays on its required main thread.
- Reveals remain asynchronous and continue to reuse one lctr-owned Finder window, raise it, and select the requested item.
- If Finder automation stops responding for five seconds, lctr terminates the wedged helper and recreates it for the pending or next reveal instead of leaving the shortcut permanently stuck.
- The helper uses a structured protocol that safely preserves spaces, quotes, newlines, and Unicode in paths.

## Performance and compatibility

- A 500-reveal release soak reused one Finder window with stable early-to-late latency and no failures.
- No database migration, configuration change, or new dependency is required.
- The macOS arm64 binary remains M1-compatible.

# lctr v0.3.4

A responsiveness and scale release for large searches, Apple-silicon input, and Finder reveal.

## Highlights

- Interactive indexed, live, and hybrid searches are now uncapped and stream every valid match in 500-result batches.
- Indexed results retain stable global ordering. Live and hybrid searches show discovery batches immediately, then apply one final ordering while preserving the selected path.
- Result rendering is viewport-only, keeping navigation work independent of the total result count.
- The footer now shows the selected row and total count, such as `25/2,049`.

## Apple silicon

- Arrow keys, `j`/`k`, and mouse-wheel events move exactly one row without the previous input throttle.
- Free-spin wheel bursts are drained before one redraw, so immediate direction reversals no longer wait behind queued events.
- Fragmented SGR mouse packets are reconstructed or discarded instead of leaking terminal bytes into the search field.
- Finder reveal now uses a compiled `NSAppleScript` worker with descriptor arguments, one reusable lctr-owned Finder window, and asynchronous TUI completion.
- Finder script errors return to the TUI instead of panicking on a nullable Objective-C response.

## Compatibility

- Public `find --limit` and library search limits are unchanged.
- No database migration or schema change is required.
- The macOS arm64 binary remains M1-compatible.

# lctr v0.3.3

## Fixes

- Finder reveals from one search TUI session now reuse one lctr-owned Finder window, raise it, select the requested item, and close only that window when the session exits.
- Finder automation failures during reveal now remain visible in the TUI status line instead of terminating the search session.
- TUI navigation rate-limits repeated same-direction scroll events to one row per ratchet and bounds queued-event draining to prevent skipped rows and runaway scrolling.
- Fragmented terminal arrow escape sequences are normalized so scroll bytes are not inserted into the search field as literal text.
- Added an opt-in macOS input recorder that pairs a raw terminal recording with decoded Crossterm JSONL events for scroll and search-input diagnosis.

# lctr v0.3.2

A maintenance release that makes self-updates fast without local Rust rebuilds.

## Fixes

- Cargo-installed binaries now download the matching prebuilt GitHub release archive instead of compiling the Rust dependency graph.
- `lctr update` skips the download when the installed version is already current.
- Added platform-aware executable replacement for macOS, Linux, and Windows release assets.

# lctr v0.3.1

A maintenance release for reliable updates and clearer project documentation.

## Fixes

- Added the `lctr update` subcommand.
- Cargo-installed binaries now use the repository-backed update path instead of the unavailable crates.io install command.
- Updated the CLI help and update banner to expose the working update command.
- Removed lctr from its own comparison table.

# lctr v0.3.0

A large quality release: new interaction model, visual redesign, FTS build speedup, shell completions, scriptable JSON output, and a set of P1 reliability fixes.

## Highlights

- **Two-focus interaction model.** The search TUI now has two explicit focuses: Search and Results. Type freely in Search focus (always active on open). Press `Tab` or `↓` to move focus to the Results list, where single keys act directly (`j/k` move, `o` open, `r` reveal, `y` copy, `m/f/s/S` mode/filter/sort, `t` theme, `w` watch). Press `/`, `Tab`, or `Esc` to return to Search focus. The focused panel gets an accent border; the footer shows live keys for the current focus.
- **Visual redesign.** Themed backgrounds, a branded header band, accent selection marker (`▌`), animated scan spinner, and focus-driven border highlights. Works across all 7 themes.
- **Faster scan optimize phase.** FTS5 `detail=none` with AND-of-trigram queries cuts the post-walk optimize phase by ~32% on a 150k-file corpus. Page-size tuning and removal of a redundant index add further marginal gains.
- **Shell completions.** `lctr completions zsh|bash|fish|powershell` generates tab-completion scripts. See the Advanced section of the README for install one-liners.
- **Scriptable `find --output`.** `lctr find <query> --output json|jsonl|tsv`. Default (tsv) output is unchanged. JSON output includes path, name, kind, size_bytes, created, modified.
- **Instant Esc.** Lone `Esc` now registers on the first press in terminals that support the kitty keyboard protocol (kitty, WezTerm, Ghostty). No more holding Esc.

## Reliability

- **Crash-safe index install.** Scans now write to a temp file and rename atomically. A crash or power loss mid-install can no longer leave a corrupt index.
- **Terminal restore on panic.** A RAII guard ensures raw mode and the alternate screen are always cleaned up, even on an unexpected panic.
- **Preview size caps.** Image previews cap at 50 MB and PDF previews cap at 10 MB. Pathological inputs no longer cause OOM.
- **Live index write errors visible.** When the background file watcher fails to write a DB update, the count appears in the TUI status line as `watch: N write errors`.

## DX

- `lctr find --output json|jsonl|tsv` (see above).
- `lctr completions <shell>` (see above).
- Consistent branding: all UIs now use `lctr` (scan dashboard previously showed `locator`).

## Notes

- Existing indexes upgrade automatically on first use.
- The `detail=none` FTS upgrade triggers a one-time FTS rebuild on the first open of an older index. On large indexes this takes a few seconds.
- The two-focus model replaces the v0.2.3 `Tab` action popup. `Tab` now switches focus rather than opening a menu.
