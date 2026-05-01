use std::path::Path;

use anyhow::{Context, Result};

pub fn open_file(path: &Path) -> Result<()> {
    std::process::Command::new("open")
        .arg(path)
        .status()
        .with_context(|| format!("open {}", path.display()))?;
    Ok(())
}

pub fn reveal_in_finder(path: &Path) -> Result<()> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status()
        .with_context(|| format!("reveal {}", path.display()))?;
    Ok(())
}

pub fn copy_path(path: &Path) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("open clipboard")?;
    clipboard
        .set_text(path.to_string_lossy().to_string())
        .context("copy path")?;
    Ok(())
}
