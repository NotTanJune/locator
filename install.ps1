param(
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\lctr\bin"
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

    Write-Host "No Windows release asset found. Falling back to cargo install from GitHub."
    cargo install --git "https://github.com/$Repo" --locked --force
    lctr --version
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
    & (Join-Path $InstallDir "lctr.exe") --version
} finally {
    if (Test-Path $tempRoot) {
        Remove-Item -Path $tempRoot -Recurse -Force
    }
}
