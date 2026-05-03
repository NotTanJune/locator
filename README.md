# 🔍 locator

Fast local file metadata search from the terminal.

`locator` builds a local SQLite index for filenames, paths, file kinds, sizes, and dates. The command is `lctr`. It does not upload data and does not index file contents.

## ✨ Demo

### External Drives

Finder search is so slow on this indexed external SSD workflow that it is literally pointless to use here. `lctr` returns local metadata results immediately from the terminal.

Apple does try to index external drives, but in practice it can be slow, fail silently, or lose usefulness when a drive is disconnected. `lctr` keeps the searchable index tied to the directory or drive you scan, so the workflow is explicit and repeatable.
<details>
<summary><b> 🎬 Click to expand the indexed external SSD demo GIF</b></summary>

![lctr search on an indexed external SSD](assets/ssd-30fps.gif)

</details>

### Internal Drive

On the internal SSD, `lctr` is still faster by a bit, and the returned results are more relevant than Finder search while arriving sooner.
<details>
<summary><b> 🎬 Click to expand the indexed internal SSD demo GIF</b></summary>

![lctr search on an indexed internal SSD](assets/internal-ssd.gif)

</details>

## Install

### 🍎 macOS And Linux

Homebrew:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

After the tap is installed:

```bash
brew install lctr
brew upgrade lctr
```

Install script:

```bash
curl -fsSL https://raw.githubusercontent.com/NotTanJune/locator/main/install.sh | sh
```

Cargo:

```bash
cargo install --git https://github.com/NotTanJune/locator --tag v0.1.41
```

Local checkout:

```bash
cargo install --path .
```

### 🪟 Windows

Scoop:

```powershell
scoop bucket add locator https://github.com/NotTanJune/locator
scoop install lctr
```

Scoop upgrades:

```powershell
scoop update
scoop update lctr
```

PowerShell installer:

```powershell
irm https://raw.githubusercontent.com/NotTanJune/locator/main/install.ps1 | iex
```

Future WinGet target:

```powershell
winget install lctr
```

That command requires acceptance into the Windows Package Manager Community Repository.

### After Install

```bash
lctr --version
lctr scan
lctr search
```

### Shell Integration

Optional zsh integration:

```bash
eval "$(lctr shell-init zsh)"
```

With shell integration installed, `lctr scan ~/Documents` scans `~/Documents`, stores the index in `~/Documents/.locator/index.sqlite`, and moves your current shell into `~/Documents` after a successful scan.

## Usage

Scan the current folder:

```bash
lctr scan
```

Scan your home folder, a target folder, or an external drive:

```bash
lctr scan ~
lctr scan ~/Documents
lctr scan /Volumes/MyDrive
```

Open the interactive search UI:

```bash
lctr search
```

Search a specific indexed or unindexed folder:

```bash
lctr search /Volumes/MyDrive
```

If no index exists, `lctr search` uses live filesystem search. If an interrupted scan left an incomplete index, `lctr search` uses hybrid search: indexed results first, then live search for paths missing from the partial index.

Scriptable one-shot search:

```bash
lctr find invoice --type pdf --min-size 100kb --modified-after 2024-01-01
```

More filters:

```bash
lctr find passport --ext jpg,png --created-after 2023-01-01 --limit 20
```

Sort and reverse order:

```bash
lctr find archive --sort size
lctr find archive --sort size --reverse
```

## Commands

```bash
lctr scan [ROOT]
lctr status
lctr search [ROOT]
lctr find <QUERY> [FILTERS]
lctr watch [ROOT]
lctr roots
lctr remove-root <ROOT>
lctr delete-index [ROOT]
lctr vacuum
lctr shell-init zsh
```

## Search UI Keys

```text
/      focus search
Esc    normal mode
Enter  confirm search or run live search
j/k    move selection
g/G    first or last result
m      cycle query mode
f      cycle type filter
s      cycle sort field
S      toggle sort order
t      cycle theme
o      open file
r      reveal file
y      copy path
?      show help text
```

## Index Storage

Directory-local index:

```bash
<ROOT>/.locator/index.sqlite
```

Default app-support index:

```bash
~/Library/Application Support/locator/index.sqlite
```

When you run `lctr search`, `lctr find`, or `lctr status`, locator first looks for the nearest `.locator/index.sqlite` in the current directory or its parents. If none exists, it uses the default app-support index.

Delete the current directory index:

```bash
lctr delete-index
```

Delete a target directory index:

```bash
lctr delete-index /Volumes/MyDrive
```

Override paths for tests or experiments:

```bash
LCTR_DB=/tmp/locator.sqlite lctr scan /tmp/files
LCTR_DATA_DIR=/tmp/locator-data lctr scan /tmp/files
```

## Scan Notes

Default scans are tuned for high-throughput local and external-drive indexing:

```bash
lctr scan /Volumes/MyDrive --backend dirent --no-eta --stage-index --batch-size 500000 --writer-queue-batches 32 --native-workers 8 --native-output-batch-size 4096 --profile-detail
```

Useful tuning options:

```bash
lctr scan ~/Documents --backend auto
lctr scan ~/Documents --backend native
lctr scan ~/Documents --backend dirent
lctr scan ~/Documents --backend parallel
lctr scan ~/Documents --eta
lctr scan /Volumes/MyDrive --no-stage-index --no-profile-detail
```

`--stage-index` builds a fresh SQLite database in app-support storage, then copies the finished index into `<ROOT>/.locator/index.sqlite`. If the root is read-only, the finished index stays in app-support fallback storage.

## Security, Privacy, And Index Deletion

`lctr` does not collect, upload, sell, or transmit any information from your machine. There is no account, telemetry, analytics, cloud sync, or background service.

Indexes are local SQLite files. By default, a scan writes to the directory you scan:

```bash
<ROOT>/.locator/index.sqlite
```

If that location is not writable, `lctr` stores a fallback index in local app-support storage for your user account. The indexed data is metadata only: paths, filenames, extensions, roots, volumes, file kinds, sizes, and dates. `lctr` does not index file contents.

Delete an index whenever you want:

```bash
lctr delete-index
lctr delete-index /Volumes/MyDrive
```

After deletion, `lctr search` falls back to live filesystem search until you scan that directory again.

Scanner behavior is session-scoped, not a daemon. Common noisy files and directories are skipped: `.DS_Store`, `._*`, `__MACOSX`, `.git`, `.locator`, `node_modules`, caches, build outputs, DerivedData, Spotlight metadata, and trash.

##📊 Comparison

`lctr` is for people who want local file search from the terminal: a fast metadata index, a dense keyboard-first TUI, and a scriptable `find` command. It intentionally does not index file contents in v1.

| Tool | Focus | Interface | Index model | Content search | Platform | How `lctr` differs |
|---|---|---|---|---|---|---|
| `lctr` | Local metadata search | CLI + TUI | SQLite metadata index | No | macOS, Linux, Windows via Scoop or PowerShell | Terminal-first, local `.locator` indexes, hybrid indexed/live search, no daemon |
| [sist2](https://github.com/sist2app/sist2) | Full file system indexing with media extraction | Web UI + CLI | SQLite or Elasticsearch search index | Yes, including OCR and archives | Docker on Windows, Linux, and macOS; executable path is Linux/WSL focused | `lctr` is lighter, terminal-native, and metadata-only |
| [Cardinal](https://github.com/cardisoft/cardinal) | Fast macOS file search app | Native GUI | App-managed search index | Yes | macOS | `lctr` is CLI/TUI first and built for repeatable terminal workflows |
| [fuz](https://github.com/Magnushhoie/fuz) | Fuzzy text, file, and folder search | Terminal fuzzy UI | Live toolchain over `fzf`, `rg`, and `bat` | Yes for text workflows | Shell environments with required tools | `lctr` persists a metadata index for fast repeated filename/path searches |
| [KatSearch](https://github.com/sveinbjornt/KatSearch) | macOS filesystem catalog search | Native GUI | No persistent index | No | macOS | `lctr` stores portable SQLite indexes and adds scriptable CLI output |
| [File Find](https://github.com/Pixel-Master/File-Find) | Desktop file search utility | GUI | Cache-based search | Limited raw text search | macOS, Linux, Windows | `lctr` is lower-friction for terminal users and automation |
| [fsindex](https://github.com/xandwr/fsindex) | Filesystem indexing library | Rust API | Library-managed index | Optional content/chunk features | Rust library | `lctr` is an end-user command, not an embedded library |
| [WindFind](https://github.com/kingToolbox/WindFind) | Windows-first instant file location | CLI | Compact Windows-focused index | No | Windows | `lctr` aims to bring indexed search to a cross-platform CLI/TUI workflow |
| [cling](https://github.com/root-project/cling) | C++ interpreter | REPL | Not a file search tool | Not a file search tool | Cross-platform source | Not a direct competitor; included only because it came up in the comparison set |
| [fd](https://github.com/sharkdp/fd) | Fast live filesystem finding | CLI | No persistent index | No | Cross-platform | `lctr` trades an initial scan for instant indexed search, richer metadata filters, and an interactive TUI |

##🗓️ Future Improvements

The current distribution story is intentionally practical: Homebrew works through the project tap, Windows works through Scoop and the direct PowerShell installer, and GitHub Releases publish platform binaries.

The bigger packaging goal is still zero-friction installation from the default package channels. For Homebrew, that means a future `homebrew/core` submission once the project has enough public usage to clear Homebrew's notability checks. For Windows, Scoop is available now, while WinGet is the next native package-manager target after more release validation.

Windows scanning also has room for a native speed pass. The current Windows path uses the portable parallel filesystem walker. A later NTFS-aware backend can make large local-volume scans faster while falling back to the portable scanner for non-NTFS, network, or permission-limited roots.

## License

This project is licensed under GPL-3.0-only. See [LICENSE](LICENSE).

## ⭐️ Star History

<a href="https://www.star-history.com/?repos=NotTanJune%2Flocator&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=NotTanJune/locator&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=NotTanJune/locator&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=NotTanJune/locator&type=date&legend=top-left" />
 </picture>
</a>
