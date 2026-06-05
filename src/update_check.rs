use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct UpdateStatus {
    pub latest: String,
    pub current: String,
    pub update_cmd: String,
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

fn fetch_latest() -> Option<String> {
    let version = current_version();
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(2))
        .build();
    let response = agent
        .get("https://api.github.com/repos/NotTanJune/locator/releases/latest")
        .set("User-Agent", &format!("lctr/{version}"))
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    let body = response.into_string().ok()?;
    extract_tag_name(&body)
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

fn detect_update_cmd() -> String {
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_default();

    let homebrew_prefix = std::env::var("HOMEBREW_PREFIX").unwrap_or_default();

    if exe_path.contains("/Cellar/")
        || (!homebrew_prefix.is_empty() && exe_path.starts_with(&homebrew_prefix))
    {
        return "brew upgrade lctr".to_string();
    }

    if exe_path.contains("/.cargo/bin") {
        return "cargo install --force locator".to_string();
    }

    if cfg!(windows) {
        return "winget upgrade NotTanJune.locator".to_string();
    }

    "see https://github.com/NotTanJune/locator/releases".to_string()
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
}
