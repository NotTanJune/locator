# locator

Fast local file metadata search from the terminal.

`locator` builds a local SQLite index for filenames, paths, file kinds, sizes, and dates. The command is `lctr`. It does not upload data and does not index file contents.

## Install

Homebrew, after the tap is published:

```bash
brew install nottanjune/locator/lctr
```

From GitHub:

```bash
cargo install --git https://github.com/nottanjune/locator --tag v0.1.37
```

From a local checkout:

```bash
cargo install --path .
```

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

## License

This project is source-available for personal use. See [LICENSE](LICENSE).
