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
