# Release Notes Draft

This file tracks user-facing changes as they are made. Before publishing a GitHub Release, compile the relevant entries into the final release body.

## v0.1.51, changes since v0.1.41

### Search UI

- Split the TUI chrome into clearer status, search, controls, results, and detail areas.
- Made the search box behave like a focused input instead of trapping shortcut keys indefinitely.
- Enter now confirms search input before opening files, so sorting and other shortcuts become active after results return.
- Added a grace timeout after indexed results complete before the search box leaves input mode.
- Made sort, filter, and other local result operations apply instantly to already-loaded results.
- Added sort order toggling for ascending and descending result order.
- Increased the TUI indexed result limit so more matches are available in the interactive view.
- Improved stale result handling so indexed search drops paths that no longer exist.

### Scan And Indexing

- Added a scan progress idle animation during SQLite optimization and index rebuild work.
- Improved rescans so deleted files are marked stale instead of continuing to appear in indexed search.
- Added current-directory scan and search guidance for directory-local indexes.
- Added shell integration so `lctr scan <dir>` can move the current shell into `<dir>` after a successful scan.

### Installation And Packaging

- Added `lctr setup-shell` as the cross-platform opt-in command for shell integration.
- Updated the curl install script to offer shell integration during install.
- Updated the PowerShell installer to offer shell integration during install.
- Added Homebrew caveats and Scoop notes that explain how to enable shell integration.
- Updated macOS, Linux, Windows, Homebrew, Scoop, Cargo, and local checkout install instructions.
- Added Windows installation and future native scan optimization documentation.

### Documentation And Marketing

- Added demo media for external-drive and internal-drive search workflows.
- Added security, privacy, and index deletion documentation.
- Added notes explaining why explicit indexing helps external-drive workflows where Apple indexing can be slow, fail, or lose usefulness after disconnects.
- Added Terminal Trove preview assets and posting support material.
- Expanded the comparison table, including Television.
- Updated Star History formatting and README presentation.

### Developer Workflow

- Kept CI passing across macOS, Linux, and Windows.
- Added tests for stale indexed path filtering, scan progress animation, shell integration setup, TUI result limits, and search interaction behavior.
