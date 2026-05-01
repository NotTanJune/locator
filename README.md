# locator

Fast local file metadata search from the terminal.

`locator` builds a local SQLite index for filenames, paths, file kinds, sizes, and dates. The command is `lctr`. It does not upload data and does not index file contents.

## Install

macOS recommended installer:

```bash
curl -fsSL https://raw.githubusercontent.com/NotTanJune/locator/main/install.sh | sh
```

Manual Homebrew install:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

After the tap is installed, future installs and upgrades can use the short formula name:

```bash
brew install lctr
brew upgrade lctr
```

From GitHub:

```bash
cargo install --git https://github.com/NotTanJune/locator --tag v0.1.40
```

From a local checkout:

```bash
cargo install --path .
```

Windows installer:

```powershell
irm https://raw.githubusercontent.com/NotTanJune/locator/main/install.ps1 | iex
```

The Windows installer downloads the latest release asset when one is available. If no release asset exists yet and Rust is installed, it falls back to `cargo install --git https://github.com/NotTanJune/locator`.

Future Windows package manager target:

```powershell
winget install lctr
```

That short command requires acceptance into the Windows Package Manager Community Repository.

Optional zsh integration:

```bash
eval "$(lctr shell-init zsh)"
```

With shell integration installed, `lctr scan ~/Documents` scans `~/Documents`, stores the index in `~/Documents/.locator/index.sqlite`, and moves your current shell into `~/Documents` after a successful scan.

## Usage

Scan your home folder:

```bash
lctr scan
```

Scan a target folder or external drive:

```bash
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

## Privacy

- Metadata stays local.
- v1 indexes paths, filenames, extensions, roots, volumes, file kinds, sizes, and dates.
- v1 does not index file contents.
- Scanner is session-scoped, not a background daemon.
- Common noisy files and directories are skipped: `.DS_Store`, `._*`, `__MACOSX`, `.git`, `.locator`, `node_modules`, caches, build outputs, DerivedData, Spotlight metadata, and trash.

## Comparison

`lctr` is for people who want local file search from the terminal: a fast metadata index, a dense keyboard-first TUI, and a scriptable `find` command. It intentionally does not index file contents in v1.

| Tool | Focus | Interface | Index model | Content search | Platform | How `lctr` differs |
|---|---|---|---|---|---|---|
| `lctr` | Local metadata search | CLI + TUI | SQLite metadata index | No | macOS and Linux now, Windows release path in progress | Terminal-first, local `.locator` indexes, hybrid indexed/live search, no daemon |
| [sist2](https://github.com/sist2app/sist2) | Full file system indexing with media extraction | Web UI + CLI | SQLite or Elasticsearch search index | Yes, including OCR and archives | Docker on Windows, Linux, and macOS; executable path is Linux/WSL focused | `lctr` is lighter, terminal-native, and metadata-only |
| [Cardinal](https://github.com/cardisoft/cardinal) | Fast macOS file search app | Native GUI | App-managed search index | Yes | macOS | `lctr` is CLI/TUI first and built for repeatable terminal workflows |
| [fuz](https://github.com/Magnushhoie/fuz) | Fuzzy text, file, and folder search | Terminal fuzzy UI | Live toolchain over `fzf`, `rg`, and `bat` | Yes for text workflows | Shell environments with required tools | `lctr` persists a metadata index for fast repeated filename/path searches |
| [KatSearch](https://github.com/sveinbjornt/KatSearch) | macOS filesystem catalog search | Native GUI | No persistent index | No | macOS | `lctr` stores portable SQLite indexes and adds scriptable CLI output |
| [File Find](https://github.com/Pixel-Master/File-Find) | Desktop file search utility | GUI | Cache-based search | Limited raw text search | macOS, Linux, Windows | `lctr` is lower-friction for terminal users and automation |
| [fsindex](https://github.com/xandwr/fsindex) | Filesystem indexing library | Rust API | Library-managed index | Optional content/chunk features | Rust library | `lctr` is an end-user command, not an embedded library |
| [WindFind](https://github.com/kingToolbox/WindFind) | Windows-first instant file location | CLI | Compact Windows-focused index | No | Windows | `lctr` aims to bring indexed search to a cross-platform CLI/TUI workflow |
| [cling](https://github.com/root-project/cling) | C++ interpreter | REPL | Not a file search tool | Not a file search tool | Cross-platform source | Not a direct competitor; included only because it came up in the comparison set |
| [fd](https://github.com/sharkdp/fd) | Fast live filesystem finding | CLI | No persistent index | No | Cross-platform | `lctr` trades an initial scan for instant indexed search, richer metadata filters, and an interactive TUI |

## Windows Status

Windows support is being added in two steps:

1. Release binaries and installer: publish `lctr-x86_64-pc-windows-msvc.zip`, install with PowerShell, then submit a `winget` manifest.
2. Native Windows scan speed: add an NTFS-aware backend behind Windows-only code, with automatic fallback to the current parallel filesystem walk for non-NTFS, network, or permission-limited roots.

## License

This project is licensed under GPL-3.0-only. See [LICENSE](LICENSE).

## Homebrew Core Status

The current tap-based install works now. Bare first-time `brew install lctr` for users who have not tapped this repository requires acceptance into `homebrew/core`. The project has been relicensed to GPL-3.0-only and includes a Homebrew formula to prepare that submission.
