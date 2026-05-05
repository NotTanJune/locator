# Windows Distribution And Optimization

## Install Path

Recommended Scoop path:

```powershell
scoop bucket add locator https://github.com/NotTanJune/locator
scoop install lctr
lctr setup-shell --shell powershell
```

This repository contains a Scoop bucket manifest at:

```text
bucket/lctr.json
```

Upgrade through Scoop:

```powershell
scoop update
scoop update lctr
```

Direct PowerShell installer:

```powershell
irm https://raw.githubusercontent.com/NotTanJune/locator/main/install.ps1 | iex
```

The direct installer asks whether to enable shell integration. That integration lets `lctr scan <dir>` move the current PowerShell session into `<dir>` after a successful scan.

Non-interactive install with shell integration enabled:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/NotTanJune/locator/main/install.ps1))) -ShellIntegration yes
```

The installer downloads `lctr-x86_64-pc-windows-msvc.zip` from the latest GitHub Release and installs `lctr.exe` into:

```text
%LOCALAPPDATA%\Programs\lctr\bin
```

It adds that directory to the user PATH. If the release asset is missing and Rust is installed, it falls back to:

```powershell
cargo install --git https://github.com/NotTanJune/locator --locked --force
```

Cargo installs can enable shell integration after installation:

```powershell
lctr setup-shell --shell powershell
```

## Scoop Maintenance

The current Scoop manifest installs the Windows release asset:

```text
https://github.com/NotTanJune/locator/releases/download/v0.1.41/lctr-x86_64-pc-windows-msvc.zip
```

The manifest uses GitHub release checking and Scoop autoupdate with download hashing, so future releases can be updated with Scoop's `checkver.ps1` workflow.

## WinGet Path

After the Windows install path has usage and validation:

1. Generate SHA256 for `lctr-x86_64-pc-windows-msvc.zip`.
2. Add a package manifest to `microsoft/winget-pkgs`.
3. Use package identifier `NotTanJune.lctr`, package name `lctr`, moniker `lctr`, license `GPL-3.0-only`.
4. Point the installer URL at the GitHub Release asset.
5. After acceptance, users can install with:

```powershell
winget install lctr
```

## Native Windows Scan Optimization

Baseline Windows behavior uses the existing parallel filesystem walk. That is correct for the first release because it is portable and does not require elevated access.

Next native backend:

1. Add a Windows-only scanner module behind `#[cfg(windows)]`.
2. Add `ScanBackend::WindowsNtfs` with CLI value `windows-ntfs`.
3. Make `ScanBackend::Auto` select `windows-ntfs` only for readable NTFS fixed or removable volumes.
4. Fall back to `ParallelWalk` for non-NTFS, network paths, permission errors, or unsupported metadata.
5. Keep output as `FileRecord` so the database and TUI do not change.

Acceptance checks:

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
lctr scan $env:USERPROFILE
lctr find powershell --limit 5
```
