//! Semantic search: embed the query, score every stored chunk, keep the best chunk per file,
//! and hydrate the surviving files from the main index so the results are ordinary `SearchResult`s.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use super::model::{self, TEXT_MODEL};
use super::runtime::{fake_from_env, query_input, with_embedder, Embedder, EmbedderConfig};
use super::store::{sidecar_path_for_index, EmbedStore, VectorIndex};
use super::vector::truncate_and_normalize;
use crate::db::{Database, SearchResult};
use crate::live_search::matches_filters;
use crate::query::SearchOptions;

/// Candidate chunks pulled from the vector index, relative to the requested limit.
const CANDIDATE_FACTOR: usize = 5;
const MIN_CANDIDATES: usize = 100;

#[derive(Debug, Clone)]
pub struct SemanticHit {
    pub result: SearchResult,
    pub score: f32,
    pub snippet: String,
}

/// Vectors held in memory with the main index path used for hydration.
pub struct SemanticSearcher {
    index: VectorIndex,
    store: EmbedStore,
    index_path: std::path::PathBuf,
    dims: usize,
    data_version: i64,
    last_check: std::time::Instant,
}

/// Minimum time between checks of the sidecar for new commits.
pub const RELOAD_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

impl SemanticSearcher {
    /// `None` when there is no sidecar or it holds no chunks.
    pub fn open(index_path: &Path) -> Result<Option<SemanticSearcher>> {
        let sidecar = sidecar_path_for_index(index_path);
        let Some(store) = EmbedStore::open_readonly(&sidecar)? else {
            return Ok(None);
        };
        if store.chunk_count()? == 0 {
            return Ok(None);
        }
        let dims = store
            .dims()?
            .ok_or_else(|| anyhow!("embeddings sidecar has no dims"))?;
        // Read the version before loading: a commit in between is then picked up by the next check.
        let data_version = store.data_version()?;
        let index = VectorIndex::load(&store)?;
        Ok(Some(SemanticSearcher {
            index,
            store,
            index_path: index_path.to_path_buf(),
            dims,
            data_version,
            last_check: std::time::Instant::now(),
        }))
    }

    /// Ranks files for `options.query`. Filters apply; the sort option does not (relevance order).
    /// Reloads the vectors if another process committed to the sidecar since the last load.
    /// Checks at most once per `RELOAD_CHECK_INTERVAL`. Returns true when it reloaded.
    pub fn refresh_if_changed(&mut self) -> Result<bool> {
        if self.last_check.elapsed() < RELOAD_CHECK_INTERVAL {
            return Ok(false);
        }
        self.last_check = std::time::Instant::now();
        let version = self.store.data_version()?;
        if version == self.data_version {
            return Ok(false);
        }
        if let Some(dims) = self.store.dims()? {
            self.dims = dims;
        }
        self.index = VectorIndex::load(&self.store)?;
        self.data_version = version;
        Ok(true)
    }

    pub fn search(
        &mut self,
        embedder: &mut dyn Embedder,
        options: &SearchOptions,
    ) -> Result<Vec<SemanticHit>> {
        self.refresh_if_changed()?;
        let query = options.query.trim();
        if query.is_empty() || options.limit == 0 {
            return Ok(Vec::new());
        }
        let raw = embedder
            .embed_batch(&[query_input(query)])
            .context("embed query")?
            .pop()
            .ok_or_else(|| anyhow!("embedder returned no query vector"))?;
        let query_vector = truncate_and_normalize(&raw, self.dims);
        let candidates = options
            .limit
            .saturating_mul(CANDIDATE_FACTOR)
            .max(MIN_CANDIDATES)
            .min(self.index.len());
        let scored = self.index.top_k(&query_vector, candidates);

        // Best chunk per file: `scored` is sorted by score, so the first hit for a path wins.
        let mut seen: HashSet<String> = HashSet::new();
        let mut best: Vec<(String, u32, f32)> = Vec::new();
        for hit in scored {
            if seen.insert(hit.path.clone()) {
                best.push((hit.path, hit.ordinal, hit.score));
            }
        }

        let db = Database::open_readonly(&self.index_path)
            .with_context(|| format!("open index {}", self.index_path.display()))?;
        let mut kept: Vec<(SearchResult, f32, u32, String)> = Vec::new();
        for (path, ordinal, score) in best {
            let Some(result) = db.result_for_path(&path)? else {
                continue;
            };
            if !matches_filters(&result, &options.filters) {
                continue;
            }
            kept.push((result, score, ordinal, path));
        }
        kept.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.3.cmp(&b.3))
        });
        kept.truncate(options.limit);

        let keys: Vec<(String, u32)> = kept
            .iter()
            .map(|(_, _, ordinal, path)| (path.clone(), *ordinal))
            .collect();
        let snippets = self.store.snippets_for(&keys)?;
        Ok(kept
            .into_iter()
            .zip(snippets)
            .map(|((result, score, _, _), snippet)| SemanticHit {
                result,
                score,
                snippet,
            })
            .collect())
    }
}

/// True when the nearest index has a sidecar with at least one chunk.
pub fn semantic_available(index_path: &Path) -> bool {
    let sidecar = sidecar_path_for_index(index_path);
    match EmbedStore::open_readonly(&sidecar) {
        Ok(Some(store)) => store.chunk_count().map(|count| count > 0).unwrap_or(false),
        _ => false,
    }
}

/// One-shot semantic query for `lctr find --mode semantic`. Loads the text model (never
/// downloads it) and runs the search. `LCTR_EMBED_FAKE=1` uses the fake embedder.
pub fn semantic_query(index_path: &Path, options: &SearchOptions) -> Result<Vec<SemanticHit>> {
    let mut searcher = SemanticSearcher::open(index_path)?
        .ok_or_else(|| anyhow!("no embeddings yet; run lctr scan <dir> --em"))?;
    let config = embedder_config_for_search()?;
    with_embedder(&config, |embedder| searcher.search(embedder, options))
}

/// Embedder settings for searching. Fails with the fetch hint when the text model is missing.
pub fn embedder_config_for_search() -> Result<EmbedderConfig> {
    if fake_from_env() {
        return Ok(EmbedderConfig {
            text_model: None,
            vision_model: None,
            fake: true,
        });
    }
    if !model::is_present(&TEXT_MODEL) {
        bail!("embedding model not found; run lctr embed fetch");
    }
    Ok(EmbedderConfig {
        text_model: Some(model::model_path(&TEXT_MODEL)?),
        vision_model: None,
        fake: false,
    })
}

/// Hash of chunk text used by the count-only report (same function the pass uses for dedupe).
pub(crate) fn text_hash_for_count(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
