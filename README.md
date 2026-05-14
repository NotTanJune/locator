# 🔍 locator

`lctr` is a Rust-built local metadata search CLI/TUI. It stores local SQLite indexes for filenames, paths, file kinds, sizes, and dates. It does not upload data and does not index file contents.

## ✨ Demo

### External Drives

Finder search is so slow on this indexed external SSD workflow that it is literally pointless to use here. `lctr` returns local metadata results immediately from the terminal.

Apple does try to index external drives, but in practice it can be slow, fail silently, or lose usefulness when a drive is disconnected. `lctr` keeps the searchable index tied to the directory or drive you scan, so the workflow is explicit and repeatable.

<video
  src="https://github.com/user-attachments/assets/d564d410-cfd0-4685-a534-99d274d15863"
  controls
  muted
  playsinline
  width="100%">
</video>


### Internal Drive

On the internal SSD, `lctr` is still faster by quite a bit, and the returned results are also more relevant than Finder search.

<video
  src="https://github.com/user-attachments/assets/666fcb99-568f-426e-bcd5-3adcfc46b36a"
  controls
  muted
  playsinline
  width="100%">
</video>


## Install

### macOS

Homebrew:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
lctr setup-shell
```

Curl:

```bash
curl -fsSL https://raw.githubusercontent.com/NotTanJune/locator/main/install.sh | sh
```

Note: macOS Homebrew installs the prebuilt Rust binary on Apple silicon, so users do not need local Rust or LLVM.

### Windows

Scoop:

```powershell
scoop bucket add locator https://github.com/NotTanJune/locator
scoop install lctr
lctr setup-shell --shell powershell
```

Curl:

```powershell
irm https://raw.githubusercontent.com/NotTanJune/locator/main/install.ps1 | iex
```

## Usage

| Command | Purpose | Example |
|---|---|---|
| `lctr scan [ROOT]` | Build or refresh a metadata index. | `lctr scan /Volumes/MyDrive` |
| `lctr search [ROOT]` | Open the interactive search UI. | `lctr search ~/Documents` |
| `lctr find <QUERY> [FILTERS]` | Run scriptable one-shot search. | `lctr find invoice --type pdf --modified-after 2024-01-01` |
| `lctr status` | Show index status for current or target root. | `lctr status` |
| `lctr delete-index [ROOT]` | Delete the local index. | `lctr delete-index /Volumes/MyDrive` |
| `lctr setup-shell [OPTIONS]` | Enable scan auto-cd shell integration. | `lctr setup-shell --shell zsh` |

If no index exists, `lctr search` uses live filesystem search. If an interrupted scan left an incomplete index, `lctr search` uses hybrid search: indexed results first, then live search for paths missing from the partial index.

## Privacy

`lctr` does not collect, upload, sell, or transmit any information from your machine. There is no account, telemetry, analytics, cloud sync, or background service.

Indexes are local SQLite files. By default, a scan writes to the directory you scan:

```bash
<ROOT>/.locator/index.sqlite
```

If that location is not writable, `lctr` stores a fallback index in local app-support storage for your user account:

```bash
~/Library/Application Support/locator/index.sqlite
```

When you run `lctr search`, `lctr find`, or `lctr status`, locator first looks for the nearest `.locator/index.sqlite` in the current directory or its parents. If none exists, it uses the default app-support index.

The indexed data is metadata only: paths, filenames, extensions, roots, volumes, file kinds, sizes, and dates. `lctr` does not index file contents.

Delete an index whenever you want:

```bash
lctr delete-index
lctr delete-index /Volumes/MyDrive
```

After deletion, `lctr search` falls back to live filesystem search until you scan that directory again.

Scanner behavior is session-scoped, not a daemon. Common noisy files and directories are skipped: `.DS_Store`, `._*`, `__MACOSX`, `.git`, `.locator`, `node_modules`, caches, build outputs, DerivedData, Spotlight metadata, and trash.

<details>
<summary>Advanced commands</summary>

### Scan Tuning

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

### Shell Integration Internals

Manual shell integration:

```bash
eval "$(lctr shell-init zsh)"
eval "$(lctr shell-init bash)"
```

Fish:

```fish
lctr shell-init fish | source
```

PowerShell:

```powershell
Invoke-Expression (& lctr shell-init powershell)
```

With shell integration installed, `lctr scan ~/Documents` scans `~/Documents`, stores the index in `~/Documents/.locator/index.sqlite`, and moves your current shell into `~/Documents` after a successful scan. Plain `lctr scan ~/Documents` cannot change the parent shell directory without this integration.

### Index Management

```bash
lctr watch [ROOT]
lctr roots
lctr remove-root <ROOT>
lctr vacuum
```

### Path Overrides

```bash
LCTR_DB=/tmp/locator.sqlite lctr scan /tmp/files
LCTR_DATA_DIR=/tmp/locator-data lctr scan /tmp/files
```

### Search UI Keys

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

</details>

## 📊 Comparison

`lctr` is for people who want local file search from the terminal: a fast metadata index, a dense keyboard-first TUI, and a scriptable `find` command. It intentionally does not index file contents in v1.

| Tool | Focus | Interface | Index model | Content search | Platform | How `lctr` differs |
|---|---|---|---|---|---|---|
| `lctr` | Local metadata search | CLI + TUI | SQLite metadata index | No | macOS, Linux, Windows via Scoop or PowerShell | Terminal-first, local `.locator` indexes, hybrid indexed/live search, no daemon |
| [sist2](https://github.com/sist2app/sist2) | Full file system indexing with media extraction | Web UI + CLI | SQLite or Elasticsearch search index | Yes, including OCR and archives | Docker on Windows, Linux, and macOS; executable path is Linux/WSL focused | `lctr` is lighter, terminal-native, and metadata-only |
| [Cardinal](https://github.com/cardisoft/cardinal) | Fast macOS file search app | Native GUI | App-managed search index | Yes | macOS | `lctr` is CLI/TUI first and built for repeatable terminal workflows |
| [fuz](https://github.com/Magnushhoie/fuz) | Fuzzy text, file, and folder search | Terminal fuzzy UI | Live toolchain over `fzf`, `rg`, and `bat` | Yes for text workflows | Shell environments with required tools | `lctr` persists a metadata index for fast repeated filename/path searches |
| [Television](https://github.com/alexpasmantier/television) | General-purpose terminal fuzzy finder over configurable channels | TUI | Live channel commands and custom sources | Yes through text/content channels | Cross-platform | `lctr` is narrower and more self-contained: persistent directory-local SQLite indexes for filename/path metadata search, especially large folders and external drives |
| [KatSearch](https://github.com/sveinbjornt/KatSearch) | macOS filesystem catalog search | Native GUI | No persistent index | No | macOS | `lctr` stores portable SQLite indexes and adds scriptable CLI output |
| [File Find](https://github.com/Pixel-Master/File-Find) | Desktop file search utility | GUI | Cache-based search | Limited raw text search | macOS, Linux, Windows | `lctr` is lower-friction for terminal users and automation |
| [fsindex](https://github.com/xandwr/fsindex) | Filesystem indexing library | Rust API | Library-managed index | Optional content/chunk features | Rust library | `lctr` is an end-user command, not an embedded library |
| [WindFind](https://github.com/kingToolbox/WindFind) | Windows-first instant file location | CLI | Compact Windows-focused index | No | Windows | `lctr` aims to bring indexed search to a cross-platform CLI/TUI workflow |
| [cling](https://github.com/root-project/cling) | C++ interpreter | REPL | Not a file search tool | Not a file search tool | Cross-platform source | Not a direct competitor; included only because it came up in the comparison set |
| [fd](https://github.com/sharkdp/fd) | Fast live filesystem finding | CLI | No persistent index | No | Cross-platform | `lctr` trades an initial scan for instant indexed search, richer metadata filters, and an interactive TUI |

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
