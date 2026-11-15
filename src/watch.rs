use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::db::Database;
use crate::live_index::LiveIndex;
use crate::scanner::{scan_root, ScanOptions};

pub fn watch_root(root: impl AsRef<Path>) -> Result<()> {
    let root = root
        .as_ref()
        .canonicalize()
        .with_context(|| format!("resolve watch root {}", root.as_ref().display()))?;
    println!("watching {} for lctr index refreshes", root.display());
    println!("press Ctrl-C to stop");

    let db_path = refresh_once(&root)?;
    match LiveIndex::spawn(root.clone(), db_path) {
        Ok(live) => {
            let mut seen = live.generation();
            loop {
                thread::sleep(Duration::from_secs(1));
                let current = live.generation();
                if current != seen {
                    seen = current;
                    println!("applied file changes under {}", root.display());
                }
            }
        }
        Err(error) => {
            println!("file watcher unavailable ({error:#}); polling every 5s instead");
            loop {
                thread::sleep(Duration::from_secs(5));
                refresh_once(&root)?;
            }
        }
    }
}

fn refresh_once(root: &PathBuf) -> Result<PathBuf> {
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
    Ok(db_path)
}
