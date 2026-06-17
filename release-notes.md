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
