param(
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\lctr\bin",
    [ValidateSet("prompt", "yes", "no")]
    [string]$ShellIntegration = "prompt"
)

$ErrorActionPreference = "Stop"
$Repo = "NotTanJune/locator"
$AssetName = "lctr-x86_64-pc-windows-msvc.zip"

function Add-LctrPath {
    param([string]$PathToAdd)

    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if ($currentPath) {
        $parts = $currentPath -split ";"
    }

    if ($parts -notcontains $PathToAdd) {
        $nextPath = if ($currentPath) { "$currentPath;$PathToAdd" } else { $PathToAdd }
        [Environment]::SetEnvironmentVariable("Path", $nextPath, "User")
        $env:Path = "$env:Path;$PathToAdd"
        Write-Host "Added $PathToAdd to user PATH. Restart your shell if lctr is not found."
    }
}

function Install-FromCargo {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "No Windows release asset was found and cargo is not installed. Install Rust from https://rustup.rs or wait for the next locator release."
    }

    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        Write-Host "Updating Rust toolchain before cargo fallback install."
        rustup update
    }

    Write-Host "No Windows release asset found. Falling back to cargo install from GitHub."
    cargo install --git "https://github.com/$Repo" --force
    lctr --version
    Invoke-LctrShellSetup -LctrCommand "lctr"
}

function Invoke-LctrShellSetup {
    param([string]$LctrCommand)

    $profilePath = $PROFILE.CurrentUserCurrentHost
    if (-not $profilePath) {
        $profilePath = $PROFILE
    }

    $setupArgs = @("setup-shell", "--shell", "powershell")
    if ($profilePath) {
        $setupArgs += @("--profile", $profilePath)
    }

    if ($ShellIntegration -eq "yes") {
        $setupArgs += "--yes"
    } elseif ($ShellIntegration -eq "no") {
        $setupArgs += "--no"
    }

    try {
        & $LctrCommand @setupArgs
    } catch {
        Write-Warning "Shell integration setup skipped. Run 'lctr setup-shell --shell powershell' later to enable scan auto-cd."
    }
}

function Get-Release {
    if ($Version -eq "latest") {
        return Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "lctr-installer" }
    }

    return Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/v$Version" -Headers @{ "User-Agent" = "lctr-installer" }
}

try {
    $release = Get-Release
} catch {
    Install-FromCargo
    exit 0
}

$asset = $release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
if (-not $asset) {
    Install-FromCargo
    exit 0
}

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("lctr-install-" + [System.Guid]::NewGuid().ToString("N"))
$zipPath = Join-Path $tempRoot $AssetName
$extractPath = Join-Path $tempRoot "extract"

New-Item -ItemType Directory -Path $tempRoot, $extractPath, $InstallDir -Force | Out-Null

try {
    Write-Host "Downloading $($asset.browser_download_url)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zipPath -Headers @{ "User-Agent" = "lctr-installer" }
    Expand-Archive -Path $zipPath -DestinationPath $extractPath -Force

    $exe = Get-ChildItem -Path $extractPath -Filter "lctr.exe" -Recurse | Select-Object -First 1
    if (-not $exe) {
        throw "Release archive did not contain lctr.exe"
    }

    Copy-Item -Path $exe.FullName -Destination (Join-Path $InstallDir "lctr.exe") -Force
    Add-LctrPath -PathToAdd $InstallDir
    $lctrPath = Join-Path $InstallDir "lctr.exe"
    & $lctrPath --version
    Invoke-LctrShellSetup -LctrCommand $lctrPath
} finally {
    if (Test-Path $tempRoot) {
        Remove-Item -Path $tempRoot -Recurse -Force
    }
}
