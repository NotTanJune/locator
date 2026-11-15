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
    /// Width of stored semantic-search vectors (128, 256, 512, or 768). Changing it re-embeds on the next `scan --em`.
    pub embed_dims: u16,
    /// How deep `scan --em` embeds each file: quick, balanced or deep (see `lctr scan --depth`).
    pub embed_depth: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            icons: true,
            theme: "default".to_string(),
            backend: "parallel".to_string(),
            preview: true,
            update_check: true,
            embed_dims: 768,
            embed_depth: "quick".to_string(),
        }
    }
}

/// All settable keys, for `config` listing and validation.
pub const KEYS: [&str; 7] = [
    "icons",
    "theme",
    "backend",
    "preview",
    "update_check",
    "embed_dims",
    "embed_depth",
];

const EMBED_DIMS_VALUES: [&str; 4] = ["128", "256", "512", "768"];
const EMBED_DEPTH_VALUES: [&str; 3] = ["quick", "balanced", "deep"];

/// Directory holding `config.toml` and the update-check files. `LCTR_CONFIG_DIR` wins.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("LCTR_CONFIG_DIR") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(dirs::config_dir()?.join("locator")),
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(dir()
            .context("locate config directory")?
            .join("config.toml"))
    }

    /// Load config, returning defaults if the file is missing or unparsable.
    /// Runs on every database open, so it never prints.
    pub fn load() -> Self {
        Self::load_reporting()
            .map(|(config, _)| config)
            .unwrap_or_default()
    }

    /// Load config keeping every valid key. Returns the names of keys that were
    /// present but invalid (those keep their default). A file that is not valid
    /// TOML is an error, so callers never overwrite it with defaults.
    pub fn load_reporting() -> Result<(Self, Vec<String>)> {
        let path = Self::path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), Vec::new()))
            }
            Err(e) => return Err(e).with_context(|| format!("read config {}", path.display())),
        };
        let table: toml::Table = text.parse().with_context(|| {
            format!(
                "config {} is not valid TOML; fix it by hand or run `lctr config reset`",
                path.display()
            )
        })?;
        let mut config = Self::default();
        let mut ignored = Vec::new();
        for (key, value) in table {
            if !KEYS.contains(&key.as_str()) {
                continue;
            }
            // Deserialize the key alone so a bad value cannot affect the others.
            let single = toml::Table::from_iter([(key.clone(), value)]);
            let valid = single
                .try_into::<Self>()
                .ok()
                .and_then(|one| config.set(&key, &one.get(&key).ok()?).ok());
            if valid.is_none() {
                ignored.push(key);
            }
        }
        Ok((config, ignored))
    }

    /// Like `load_reporting`, warning on stderr about ignored keys (for `lctr config` commands).
    pub fn load_with_warning() -> Result<Self> {
        let (config, ignored) = Self::load_reporting()?;
        if !ignored.is_empty() {
            eprintln!(
                "warning: ignoring invalid config value(s) for: {} (using defaults)",
                ignored.join(", ")
            );
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let parent = path.parent().context("config path has no directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
        let text = toml::to_string_pretty(self).context("serialize config")?;
        let tmp = parent.join(format!(".config.toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("write config {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| {
            let _ = std::fs::remove_file(&tmp);
            format!("replace config {}", path.display())
        })
    }

    pub fn get(&self, key: &str) -> Result<String> {
        Ok(match key {
            "icons" => self.icons.to_string(),
            "theme" => self.theme.clone(),
            "backend" => self.backend.clone(),
            "preview" => self.preview.to_string(),
            "update_check" => self.update_check.to_string(),
            "embed_dims" => self.embed_dims.to_string(),
            "embed_depth" => self.embed_depth.clone(),
            other => return Err(unknown_key(other)),
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "icons" => self.icons = parse_bool(value)?,
            "preview" => self.preview = parse_bool(value)?,
            "update_check" => self.update_check = parse_bool(value)?,
            "theme" => self.theme = validate_one_of(value, &THEME_VALUES, "theme")?,
            "backend" => self.backend = validate_one_of(value, &BACKEND_VALUES, "backend")?,
            "embed_dims" => {
                let valid = validate_one_of(value, &EMBED_DIMS_VALUES, "embed_dims")?;
                self.embed_dims = valid.parse().context("embed_dims must be a number")?;
            }
            "embed_depth" => {
                self.embed_depth = validate_one_of(value, &EMBED_DEPTH_VALUES, "embed_depth")?
            }
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
        "embed_dims" => "Embedding dimensions",
        "embed_depth" => "Embedding depth",
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
        "embed_dims" => {
            "Size of stored embedding vectors for semantic search. Smaller is faster and uses less disk, larger is more accurate. Changing it re-embeds on the next `scan --em`."
        }
        "embed_depth" => {
            "How much of each file `scan --em` embeds. quick (default): the first 8 chunks of every file, so search works soonest. balanced: up to 32 chunks per file. deep: up to 128 chunks per file. A later run with a deeper setting adds only the missing chunks. `lctr scan --depth` overrides this for one run."
        }
        _ => "",
    }
}

/// Allowed values for a key, in cycle order (for the config UI).
pub fn key_choices(key: &str) -> Vec<&'static str> {
    match key {
        "theme" => THEME_VALUES.to_vec(),
        "backend" => BACKEND_VALUES.to_vec(),
        "embed_dims" => EMBED_DIMS_VALUES.to_vec(),
        "embed_depth" => EMBED_DEPTH_VALUES.to_vec(),
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
