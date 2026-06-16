use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::db::{sort_results, SearchResult};
use crate::query::{CompiledQuery, QueryScorer, SearchFilters, SearchOptions};
use crate::scanner::{kind_for, pruned_walk, system_time_to_utc};

#[derive(Debug, Clone, Copy)]
pub struct LiveSearchOptions {
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSearchStatus {
    Complete,
    Cancelled,
}

pub fn search_live(
    root: impl AsRef<Path>,
    query: &str,
    options: LiveSearchOptions,
) -> Result<Vec<SearchResult>> {
    let search_options = SearchOptions::new(query).with_limit(options.limit);
    search_live_with_options(root, &search_options)
}

pub fn search_live_with_options(
    root: impl AsRef<Path>,
    options: &SearchOptions,
) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();
    search_live_streaming_with_options(
        root,
        options,
        || false,
        |_| {},
        |partial| {
            results = partial.to_vec();
        },
    )?;
    Ok(results)
}

pub fn search_live_streaming(
    root: impl AsRef<Path>,
    query: &str,
    options: LiveSearchOptions,
    should_cancel: impl FnMut() -> bool,
    on_match: impl FnMut(&[SearchResult]),
    on_partial: impl FnMut(&[SearchResult]),
) -> Result<LiveSearchStatus> {
    let search_options = SearchOptions::new(query).with_limit(options.limit);
    search_live_streaming_with_options(root, &search_options, should_cancel, on_match, on_partial)
}

pub fn search_live_streaming_with_options(
    root: impl AsRef<Path>,
    options: &SearchOptions,
    mut should_cancel: impl FnMut() -> bool,
    mut on_match: impl FnMut(&[SearchResult]),
    mut on_partial: impl FnMut(&[SearchResult]),
) -> Result<LiveSearchStatus> {
    options.validate()?;
    if options.limit == 0 {
        on_partial(&[]);
        return Ok(LiveSearchStatus::Complete);
    }

    let needle = normalize_query(&options.query);
    if needle.is_empty() {
        on_partial(&[]);
        return Ok(LiveSearchStatus::Complete);
    }

    let root = root
        .as_ref()
        .canonicalize()
        .with_context(|| format!("resolve live search root {}", root.as_ref().display()))?;
    let mut results = Vec::with_capacity(options.limit.min(50));
    let mut last_partial = Instant::now();
    let compiled = CompiledQuery::compile(options.mode, &options.query)?;
    let mut scorer = QueryScorer::new();

    for entry in pruned_walk(&root) {
        if should_cancel() {
            return Ok(LiveSearchStatus::Cancelled);
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root || entry.file_type().is_dir() {
            continue;
        }

        let Some(name) = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
        else {
            continue;
        };
        let Some(result) = result_from_path(path, name) else {
            continue;
        };
        if !matches_filters(&result, &options.filters) {
            continue;
        }
        if !compiled.matches_any(&mut scorer, [result.name.as_str(), result.path.as_str()]) {
            continue;
        }
        results.push(result);
        sort_results(&mut results, options);
        on_match(&results);

        if results.len() >= options.limit {
            on_partial(&results);
            return Ok(LiveSearchStatus::Complete);
        }

        if results.len() <= 10 || last_partial.elapsed() >= Duration::from_millis(100) {
            on_partial(&results);
            last_partial = Instant::now();
        }
    }

    sort_results(&mut results, options);
    on_partial(&results);
    Ok(LiveSearchStatus::Complete)
}

fn result_from_path(path: PathBuf, name: String) -> Option<SearchResult> {
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase());
    let kind = kind_for(&path, metadata.is_dir());

    Some(SearchResult {
        path: path.to_string_lossy().to_string(),
        name,
        extension,
        kind,
        size_bytes: metadata.len(),
        created_at: metadata.created().ok().and_then(system_time_to_utc),
        modified_at: metadata.modified().ok().and_then(system_time_to_utc),
    })
}

fn normalize_query(query: &str) -> String {
    query.trim().to_ascii_lowercase()
}

fn matches_filters(result: &SearchResult, filters: &SearchFilters) -> bool {
    if let Some(kind) = &filters.kind {
        if result.kind != kind.as_str() {
            return false;
        }
    }
    if !filters.exts.is_empty()
        && !result
            .extension
            .as_ref()
            .is_some_and(|ext| filters.exts.iter().any(|filter| filter == ext))
    {
        return false;
    }
    if filters
        .min_size
        .is_some_and(|min_size| result.size_bytes < min_size)
    {
        return false;
    }
    if filters
        .max_size
        .is_some_and(|max_size| result.size_bytes > max_size)
    {
        return false;
    }
    if filters.created_after.is_some_and(|created_after| {
        result
            .created_at
            .is_none_or(|created| created < created_after)
    }) {
        return false;
    }
    if filters.created_before.is_some_and(|created_before| {
        result
            .created_at
            .is_none_or(|created| created > created_before)
    }) {
        return false;
    }
    if filters.modified_after.is_some_and(|modified_after| {
        result
            .modified_at
            .is_none_or(|modified| modified < modified_after)
    }) {
        return false;
    }
    if filters.modified_before.is_some_and(|modified_before| {
        result
            .modified_at
            .is_none_or(|modified| modified > modified_before)
    }) {
        return false;
    }
    if let Some(name) = &filters.name {
        if !result
            .name
            .to_ascii_lowercase()
            .contains(&name.to_ascii_lowercase())
        {
            return false;
        }
    }
    true
}
