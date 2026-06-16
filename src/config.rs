//! Persistent user configuration (`config.toml`) so common preferences don't
//! have to be passed as flags every run. Environment variables and explicit
//! flags still override config at the call site.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Settings persisted to `<config_dir>/locator/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Show Nerd Font file-type icons in the results list.
    pub icons: bool,
    /// Default TUI theme name (default/catppuccin/tokyonight/gruvbox/nord/ocean/mono).
    pub theme: String,
    /// Default scan backend (auto/native/dirent/parallel).
    pub backend: String,
    /// Show the file preview pane in wide terminals.
    pub preview: bool,
    /// Check GitHub for newer releases.
    pub update_check: bool,
    /// Also index full paths (not just filenames) for substring search. Enables
    /// directory-name substring matching at the cost of a much slower scan.
    pub index_paths: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            icons: false,
            theme: "default".to_string(),
            backend: "parallel".to_string(),
            preview: true,
            update_check: true,
            index_paths: false,
        }
    }
}

/// All settable keys, for `config` listing and validation.
pub const KEYS: [&str; 6] = [
    "icons",
    "theme",
    "backend",
    "preview",
    "update_check",
    "index_paths",
];

impl Config {
    pub fn path() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("LCTR_CONFIG_DIR") {
            return Ok(PathBuf::from(dir).join("config.toml"));
        }
        let base = dirs::config_dir().context("locate config directory")?;
        Ok(base.join("locator").join("config.toml"))
    }

    /// Load config, returning defaults if the file is missing or unparsable.
    pub fn load() -> Self {
        Self::path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config directory {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialize config")?;
        std::fs::write(&path, text).with_context(|| format!("write config {}", path.display()))
    }

    pub fn get(&self, key: &str) -> Result<String> {
        Ok(match key {
            "icons" => self.icons.to_string(),
            "theme" => self.theme.clone(),
            "backend" => self.backend.clone(),
            "preview" => self.preview.to_string(),
            "update_check" => self.update_check.to_string(),
            "index_paths" => self.index_paths.to_string(),
            other => return Err(unknown_key(other)),
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "icons" => self.icons = parse_bool(value)?,
            "preview" => self.preview = parse_bool(value)?,
            "update_check" => self.update_check = parse_bool(value)?,
            "index_paths" => self.index_paths = parse_bool(value)?,
            "theme" => self.theme = validate_one_of(value, &THEME_VALUES, "theme")?,
            "backend" => self.backend = validate_one_of(value, &BACKEND_VALUES, "backend")?,
            other => return Err(unknown_key(other)),
        }
        Ok(())
    }

    /// `(key, value)` pairs in stable order, for `config` listing.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        KEYS.iter()
            .map(|&key| (key, self.get(key).unwrap_or_default()))
            .collect()
    }
}

/// Human-readable label for a key (for the config UI).
pub fn key_label(key: &str) -> &'static str {
    match key {
        "icons" => "File icons",
        "theme" => "Theme",
        "backend" => "Scan backend",
        "preview" => "Preview pane",
        "update_check" => "Update check",
        "index_paths" => "Index full paths",
        _ => "",
    }
}

/// One-line description of what a setting does (for the config UI).
pub fn key_description(key: &str) -> &'static str {
    match key {
        "icons" => {
            "Show Nerd Font file-type icons next to results. Requires a Nerd Font in your terminal, otherwise glyphs render as boxes."
        }
        "theme" => "Colour theme for the search UI. Changes here preview live below.",
        "backend" => {
            "Default filesystem walker for `lctr scan`. parallel is fastest on large trees; native uses macOS getattrlistbulk; auto picks per-OS."
        }
        "preview" => {
            "Show the syntax-highlighted file preview pane (with inline images) when the terminal is wide enough."
        }
        "update_check" => "Check GitHub at most once a day for a newer lctr release.",
        "index_paths" => {
            "Index full file paths, not just filenames, so substring search also matches directory names. Makes the scan's optimize phase much slower (paths are ~7x more text). Off = filename search only, fast scans. Takes effect on the next scan."
        }
        _ => "",
    }
}

/// Allowed values for a key, in cycle order (for the config UI).
pub fn key_choices(key: &str) -> Vec<&'static str> {
    match key {
        "theme" => THEME_VALUES.to_vec(),
        "backend" => BACKEND_VALUES.to_vec(),
        // booleans
        _ => vec!["true", "false"],
    }
}

const THEME_VALUES: [&str; 7] = [
    "default",
    "catppuccin",
    "tokyonight",
    "gruvbox",
    "nord",
    "ocean",
    "mono",
];
const BACKEND_VALUES: [&str; 4] = ["auto", "native", "dirent", "parallel"];

fn unknown_key(key: &str) -> anyhow::Error {
    anyhow!(
        "unknown config key '{key}', expected one of: {}",
        KEYS.join(", ")
    )
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(anyhow!("expected a boolean (true/false), got '{other}'")),
    }
}

fn validate_one_of(value: &str, allowed: &[&str], field: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if allowed.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(anyhow!(
            "invalid {field} '{value}', expected one of: {}",
            allowed.join(", ")
        ))
    }
}
