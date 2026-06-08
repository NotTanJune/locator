# lctr v0.2.3

A big release: faster search, a live-updating index, rich file previews, and an interactive settings editor.

## Highlights

- **Lightning search.** Substring search now uses a trigram full-text index instead of a full table scan. On a 1.3M file index, searching dropped from several seconds to ~20-30ms.
- **Live index.** While the search TUI is open, file creates, edits, and deletes are reflected in results within about a second, no rescan needed. Toggle with `w`.
- **File preview pane.** Wide terminals get a side preview: syntax-highlighted text, inline images (kitty/iterm2/sixel, with a half-block fallback), and extracted PDF text. Previews load only after you pause scrolling, so the list stays smooth.
- **Match highlighting.** The matched part of each filename and path is highlighted in results.
- **Frecency ranking.** Files you open often and recently rank higher.

## Settings

- **`lctr config`** opens an interactive settings editor: pick a value, read what it does, save. Scriptable too: `lctr config set <key> <value>`.
- Settings: `theme`, `icons`, `preview`, `backend`, `update_check`, `index_paths`.
- **7 themes**: default, catppuccin, tokyonight, gruvbox, nord, ocean, mono. Cycle live with `t`.
- Optional Nerd Font file-type **icons** (`icons`).

## Search UI

- Compact redesigned layout: the controls are now one line, with a full `?` help overlay.
- Long paths in the detail bar now wrap instead of truncating.

## Scanning

- Faster default scan backend (parallel walker).
- Filenames are indexed by default for fast scans. Enable full-path substring search with `lctr config set index_paths true` (slower scan).
- Cleaner scan summary timings (sub-millisecond stages show `<0.00s`).

## Notes

- Existing indexes upgrade automatically on first use.
- New environment variables: `LCTR_ICONS`, `LCTR_TIMING`.
