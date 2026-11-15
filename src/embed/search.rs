//! Semantic search: embed the query, score every stored chunk, keep the best chunk per file,
//! and hydrate the surviving files from the main index so the results are ordinary `SearchResult`s.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use super::model::{self, TEXT_MODEL};
use super::runtime::{fake_from_env, query_input, with_embedder, Embedder, EmbedderConfig};
use super::store::{sidecar_path_for_index, EmbedStore, ScoredFile, VectorIndex};
use super::vector::truncate_and_normalize;
use crate::db::{Database, SearchResult};
use crate::live_search::matches_filters;
use crate::query::SearchOptions;

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
    /// The last query text and the vector the embedder gave for it, before truncation. The
    /// search screen runs the same query again after every watcher batch, and this saves
    /// the model call each time. A searcher is used with one embedder for its whole life.
    last_query: Option<(String, Vec<f32>)>,
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
            last_query: None,
        }))
    }

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

    /// The query's vector at the sidecar's dims. The embedder runs only when the text differs
    /// from the last query; a failed embed leaves nothing cached.
    fn query_vector(&mut self, embedder: &mut dyn Embedder, query: &str) -> Result<Vec<f32>> {
        if !matches!(&self.last_query, Some((text, _)) if text == query) {
            let raw = embedder
                .embed_batch(&[query_input(query)])
                .context("embed query")?
                .pop()
                .ok_or_else(|| anyhow!("embedder returned no query vector"))?;
            self.last_query = Some((query.to_string(), raw));
        }
        let raw = self
            .last_query
            .as_ref()
            .map(|(_, raw)| raw.as_slice())
            .unwrap_or_default();
        // The dims can change when the sidecar is reloaded, so truncate on every search.
        Ok(truncate_and_normalize(raw, self.dims))
    }

    /// Ranks files for `options.query`. Filters apply; the sort option does not (relevance order).
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
        let query_vector = self.query_vector(embedder, query)?;

        // Files in score order; hydration and filters drop some, so walk until `limit` are kept.
        let db = Database::open_readonly(&self.index_path)
            .with_context(|| format!("open index {}", self.index_path.display()))?;
        let mut lookup = db.path_lookup()?;
        let mut kept: Vec<(SearchResult, ScoredFile<'_>)> = Vec::new();
        for file in self.index.best_per_file(&query_vector) {
            if kept.len() == options.limit {
                break;
            }
            let Some(result) = lookup.result_for(file.path) else {
                continue;
            };
            if matches_filters(&result, &options.filters) {
                kept.push((result, file));
            }
        }

        let keys: Vec<(String, u32)> = kept
            .iter()
            .map(|(_, file)| (file.path.to_string(), file.ordinal))
            .collect();
        let snippets = self.store.snippets_for(&keys)?;
        Ok(kept
            .into_iter()
            .zip(snippets)
            .map(|((result, file), snippet)| SemanticHit {
                result,
                score: file.score,
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
