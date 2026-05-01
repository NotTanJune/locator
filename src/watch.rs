use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::db::Database;
use crate::scanner::{scan_root, ScanOptions};

pub fn watch_root(root: impl AsRef<Path>) -> Result<()> {
    let root = root
        .as_ref()
        .canonicalize()
        .with_context(|| format!("resolve watch root {}", root.as_ref().display()))?;
    println!("watching {} for lctr index refreshes", root.display());
    println!("press Ctrl-C to stop");

    loop {
        refresh_once(&root)?;
        thread::sleep(Duration::from_secs(5));
    }
}

fn refresh_once(root: &PathBuf) -> Result<()> {
    let (db, db_path) = Database::open_for_scan_root(root)?;
    let stats = scan_root(
        &db,
        root,
        ScanOptions {
            estimate_totals: false,
            ..Default::default()
        },
    )?;
    println!(
        "indexed {} files from {} using {}",
        stats.indexed_files,
        root.display(),
        db_path.display()
    );
    Ok(())
}
