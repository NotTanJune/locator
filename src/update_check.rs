use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPOSITORY: &str = "NotTanJune/locator";
const REPOSITORY_URL: &str = "https://github.com/NotTanJune/locator";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/NotTanJune/locator/releases/latest";
const MAX_RELEASE_ASSET_BYTES: u64 = 512 * 1024 * 1024;

pub struct UpdateStatus {
    pub latest: String,
    pub current: String,
    pub update_cmd: String,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn cache_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("locator").join("update_check"))
}

fn disable_marker_file() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("locator")
            .join("update_check_disabled"),
    )
}

fn read_cache() -> Option<(u64, String)> {
    let path = cache_file()?;
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let ts: u64 = lines.next()?.trim().parse().ok()?;
    let version = lines.next()?.trim().to_string();
    if version.is_empty() {
        return None;
    }
    Some((ts, version))
}

fn write_cache(ts: u64, version: &str) {
    let Some(path) = cache_file() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, format!("{}\n{}\n", ts, version));
}

pub fn checks_disabled() -> bool {
    if let Ok(val) = std::env::var("LCTR_NO_UPDATE_CHECK") {
        if !val.is_empty() && val != "0" {
            return true;
        }
    }
    disable_marker_file().map(|p| p.exists()).unwrap_or(false)
}

pub fn persist_disable() {
    let Some(path) = disable_marker_file() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, "");
}

fn github_agent(connect_timeout: Duration, read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .build()
}

fn fetch_latest_body() -> Result<String> {
    let version = current_version();
    let response = github_agent(Duration::from_secs(2), Duration::from_secs(2))
        .get(LATEST_RELEASE_URL)
        .set("User-Agent", &format!("lctr/{version}"))
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("fetch latest GitHub release metadata")?;
    response
        .into_string()
        .context("read latest GitHub release metadata")
}

fn fetch_latest() -> Option<String> {
    let body = fetch_latest_body().ok()?;
    extract_tag_name(&body)
}

fn fetch_latest_release() -> Result<LatestRelease> {
    let body = fetch_latest_body()?;
    parse_latest_release(&body)
}

fn parse_latest_release(body: &str) -> Result<LatestRelease> {
    serde_json::from_str(body).context("parse latest GitHub release metadata")
}

fn extract_tag_name(body: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let pos = body.find(key)?;
    let after_key = &body[pos + key.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let inner = &after_colon[1..];
    let end = inner.find('"')?;
    let tag = inner[..end].trim_start_matches('v').to_string();
    if tag.is_empty() {
        return None;
    }
    Some(tag)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSource {
    Homebrew,
    Cargo,
    WindowsPackageManager,
    Unsupported,
}

fn detect_install_source(exe_path: &str, homebrew_prefix: &str, windows: bool) -> InstallSource {
    if exe_path.contains("/Cellar/")
        || (!homebrew_prefix.is_empty() && exe_path.starts_with(homebrew_prefix))
    {
        return InstallSource::Homebrew;
    }

    if exe_path.contains("/.cargo/bin") || exe_path.contains("\\.cargo\\bin") {
        return InstallSource::Cargo;
    }

    if windows {
        return InstallSource::WindowsPackageManager;
    }

    InstallSource::Unsupported
}

fn semver_gt(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|part| {
                // strip any non-digit suffix (e.g. pre-release like "1alpha")
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u64>().unwrap_or(0)
            })
            .collect()
    };
    let l = parse(latest);
    let c = parse(current);
    let len = l.len().max(c.len());
    for i in 0..len {
        let lv = l.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if lv > cv {
            return true;
        }
        if lv < cv {
            return false;
        }
    }
    false
}

fn detect_update_cmd_for(exe_path: &str, homebrew_prefix: &str, windows: bool) -> String {
    match detect_install_source(exe_path, homebrew_prefix, windows) {
        InstallSource::Homebrew => "brew upgrade lctr".to_string(),
        InstallSource::Cargo => "lctr update".to_string(),
        InstallSource::WindowsPackageManager => "winget upgrade NotTanJune.locator".to_string(),
        InstallSource::Unsupported => {
            "see https://github.com/NotTanJune/locator/releases".to_string()
        }
    }
}

fn detect_update_cmd() -> String {
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_default();
    let homebrew_prefix = std::env::var("HOMEBREW_PREFIX").unwrap_or_default();

    detect_update_cmd_for(&exe_path, &homebrew_prefix, cfg!(windows))
}

fn run_update_command(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run `{program} {}`", args.join(" ")))?;

    if !status.success() {
        bail!(
            "`{program} {}` failed with status {}",
            args.join(" "),
            status
        );
    }

    Ok(())
}

fn prebuilt_asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("lctr-aarch64-apple-darwin.tar.gz"),
        ("linux", "x86_64") => Some("lctr-x86_64-unknown-linux-gnu.tar.gz"),
        ("windows", "x86_64") => Some("lctr-x86_64-pc-windows-msvc.zip"),
        _ => None,
    }
}

fn release_version(tag: &str) -> Result<&str> {
    let version = tag.trim().trim_start_matches('v');
    if version.is_empty() {
        bail!("latest GitHub release has an invalid tag `{tag}`");
    }
    Ok(version)
}

fn download_release_asset(url: &str, destination: &Path) -> Result<()> {
    let response = github_agent(Duration::from_secs(10), Duration::from_secs(60))
        .get(url)
        .set("User-Agent", &format!("lctr/{}", current_version()))
        .set("Accept", "application/octet-stream")
        .call()
        .with_context(|| format!("download release asset from {url}"))?;
    let mut reader = response.into_reader().take(MAX_RELEASE_ASSET_BYTES + 1);
    let mut file = fs::File::create(destination)
        .with_context(|| format!("create temporary release asset {}", destination.display()))?;
    let bytes = io::copy(&mut reader, &mut file)
        .with_context(|| format!("write downloaded release asset {}", destination.display()))?;
    if bytes > MAX_RELEASE_ASSET_BYTES {
        bail!(
            "release asset exceeds the {} MiB safety limit",
            MAX_RELEASE_ASSET_BYTES / 1024 / 1024
        );
    }
    Ok(())
}

fn extract_release_archive(archive: &Path, destination: &Path, asset_name: &str) -> Result<()> {
    let status = if asset_name.ends_with(".tar.gz") {
        Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(destination)
            .status()
    } else if asset_name.ends_with(".zip") && cfg!(windows) {
        let script = concat!(
            "$ErrorActionPreference = 'Stop'; ",
            "Expand-Archive -LiteralPath $env:LCTR_UPDATE_ARCHIVE ",
            "-DestinationPath $env:LCTR_UPDATE_DESTINATION -Force"
        );
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("LCTR_UPDATE_ARCHIVE", archive)
            .env("LCTR_UPDATE_DESTINATION", destination)
            .status()
    } else {
        bail!("unsupported release archive `{asset_name}`");
    }
    .with_context(|| format!("extract release archive {}", archive.display()))?;

    if !status.success() {
        bail!("extracting release archive failed with status {status}");
    }
    Ok(())
}

fn extracted_binary_path(directory: &Path) -> Result<PathBuf> {
    let binary_name = if cfg!(windows) { "lctr.exe" } else { "lctr" };
    let binary = directory.join(binary_name);
    let metadata = fs::metadata(&binary)
        .with_context(|| format!("find extracted executable {}", binary.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        bail!(
            "extracted executable {} is empty or not a file",
            binary.display()
        );
    }
    Ok(binary)
}

fn stage_executable(source: &Path, destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .context("determine installed executable directory")?;
    let file_name = destination
        .file_name()
        .context("determine installed executable name")?
        .to_string_lossy();
    let staged = parent.join(format!(".{file_name}.lctr-update-{}", std::process::id()));
    fs::copy(source, &staged).with_context(|| {
        format!(
            "stage downloaded executable {} as {}",
            source.display(),
            staged.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(destination)
            .context("read installed executable permissions")?
            .permissions()
            .mode();
        fs::set_permissions(&staged, fs::Permissions::from_mode(mode))
            .context("preserve installed executable permissions")?;
    }

    Ok(staged)
}

#[cfg(not(windows))]
fn install_staged_executable(staged: &Path, destination: &Path) -> Result<()> {
    fs::rename(staged, destination).with_context(|| {
        format!(
            "replace installed executable {} with {}",
            destination.display(),
            staged.display()
        )
    })?;
    Ok(())
}

#[cfg(windows)]
fn install_staged_executable(staged: &Path, destination: &Path) -> Result<()> {
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
for ($attempt = 0; $attempt -lt 100; $attempt++) {
    try {
        Move-Item -LiteralPath $env:LCTR_UPDATE_SOURCE -Destination $env:LCTR_UPDATE_DESTINATION -Force -ErrorAction Stop
        exit 0
    } catch {
        Start-Sleep -Milliseconds 100
    }
}
exit 1
"#;
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("LCTR_UPDATE_SOURCE", staged)
        .env("LCTR_UPDATE_DESTINATION", destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("schedule installed executable replacement")?;
    Ok(())
}

fn update_from_prebuilt_release(exe_path: &Path) -> Result<()> {
    let release = fetch_latest_release()?;
    let latest_version = release_version(&release.tag_name)?;
    if !semver_gt(latest_version, current_version()) {
        println!("lctr is already up to date (v{}).", current_version());
        return Ok(());
    }

    let asset_name = prebuilt_asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
        .with_context(|| {
            format!(
                "no prebuilt lctr release is available for {}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS
            )
        })?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .with_context(|| format!("release {} has no asset `{asset_name}`", release.tag_name))?;

    let temp_dir = tempfile::tempdir().context("create temporary update directory")?;
    let archive = temp_dir.path().join(asset_name);
    download_release_asset(&asset.browser_download_url, &archive)?;
    let extracted = temp_dir.path().join("extracted");
    fs::create_dir(&extracted).context("create temporary extraction directory")?;
    extract_release_archive(&archive, &extracted, asset_name)?;
    let downloaded_binary = extracted_binary_path(&extracted)?;
    let staged = stage_executable(&downloaded_binary, exe_path)?;

    if let Err(error) = install_staged_executable(&staged, exe_path) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }

    println!("Updated lctr to v{latest_version}.");
    Ok(())
}

pub fn run_update() -> Result<()> {
    let exe_path = std::env::current_exe().context("determine the installed lctr executable")?;
    let exe_path_string = exe_path.to_string_lossy();
    let homebrew_prefix = std::env::var("HOMEBREW_PREFIX").unwrap_or_default();
    let source = detect_install_source(&exe_path_string, &homebrew_prefix, cfg!(windows));

    println!("Updating lctr...");
    match source {
        InstallSource::Homebrew => run_update_command("brew", &["upgrade", "lctr"]),
        InstallSource::Cargo => update_from_prebuilt_release(&exe_path),
        InstallSource::WindowsPackageManager => {
            run_update_command("winget", &["upgrade", REPOSITORY])
        }
        InstallSource::Unsupported => bail!(
            "automatic updates are unavailable for this installation; reinstall from {}",
            REPOSITORY_URL
        ),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn check(force_disabled: bool) -> Option<UpdateStatus> {
    if force_disabled || checks_disabled() {
        return None;
    }

    const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
    let now = now_unix();

    let latest = if let Some((ts, cached_version)) = read_cache() {
        if now.saturating_sub(ts) < CACHE_TTL_SECS {
            cached_version
        } else {
            let fetched = fetch_latest()?;
            write_cache(now, &fetched);
            fetched
        }
    } else {
        let fetched = fetch_latest()?;
        write_cache(now, &fetched);
        fetched
    };

    let current = current_version().to_string();
    if semver_gt(&latest, &current) {
        Some(UpdateStatus {
            latest,
            current,
            update_cmd: detect_update_cmd(),
        })
    } else {
        None
    }
}

pub fn check_async(force_disabled: bool) -> mpsc::Receiver<Option<UpdateStatus>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = check(force_disabled);
        let _ = tx.send(result);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semver_gt_minor_bump() {
        assert!(semver_gt("0.2.0", "0.1.59"));
    }

    #[test]
    fn test_semver_gt_patch_bump() {
        assert!(semver_gt("0.1.60", "0.1.59"));
    }

    #[test]
    fn test_semver_gt_equal_is_false() {
        assert!(!semver_gt("0.1.59", "0.1.59"));
    }

    #[test]
    fn test_semver_gt_major_bump() {
        assert!(semver_gt("1.0.0", "0.9.9"));
    }

    #[test]
    fn test_semver_gt_older_is_false() {
        assert!(!semver_gt("0.1.58", "0.1.59"));
    }

    #[test]
    fn test_extract_tag_name_v_prefix() {
        let json = r#"{"tag_name": "v0.2.0", "name": "Release 0.2.0"}"#;
        assert_eq!(extract_tag_name(json), Some("0.2.0".to_string()));
    }

    #[test]
    fn test_extract_tag_name_no_v_prefix() {
        let json = r#"{"url": "...", "tag_name": "1.0.0", "draft": false}"#;
        assert_eq!(extract_tag_name(json), Some("1.0.0".to_string()));
    }

    #[test]
    fn test_extract_tag_name_missing() {
        let json = r#"{"name": "no release"}"#;
        assert_eq!(extract_tag_name(json), None);
    }

    #[test]
    fn cargo_install_update_uses_lctr_subcommand() {
        assert_eq!(
            detect_update_cmd_for("/Users/test/.cargo/bin/lctr", "", false),
            "lctr update"
        );
        assert_eq!(
            detect_update_cmd_for(r"C:\Users\test\.cargo\bin\lctr.exe", "", true),
            "lctr update"
        );
    }

    #[test]
    fn prebuilt_asset_name_matches_release_targets() {
        assert_eq!(
            prebuilt_asset_name_for("macos", "aarch64"),
            Some("lctr-aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(
            prebuilt_asset_name_for("linux", "x86_64"),
            Some("lctr-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            prebuilt_asset_name_for("windows", "x86_64"),
            Some("lctr-x86_64-pc-windows-msvc.zip")
        );
        assert_eq!(prebuilt_asset_name_for("linux", "aarch64"), None);
    }

    #[test]
    fn latest_release_parses_matching_asset() {
        let body = r#"
        {
          "tag_name": "v0.4.0",
          "assets": [
            {
              "name": "lctr-aarch64-apple-darwin.tar.gz",
              "browser_download_url": "https://example.test/lctr.tar.gz"
            }
          ]
        }
        "#;
        let release = parse_latest_release(body).expect("release metadata");
        assert_eq!(
            release_version(&release.tag_name).expect("release version"),
            "0.4.0"
        );
        assert_eq!(release.assets[0].name, "lctr-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            release.assets[0].browser_download_url,
            "https://example.test/lctr.tar.gz"
        );
    }

    #[test]
    fn current_release_does_not_need_an_update() {
        assert!(!semver_gt(current_version(), current_version()));
    }
}
