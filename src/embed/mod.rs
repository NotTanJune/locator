//! Local embedding support: model files, the llama.cpp runtime, chunking,
//! the sidecar vector store, and semantic search.

pub mod bench;
pub mod chunk;
pub mod cli;
pub mod fake;
pub mod image_sizing;
pub mod mixed_batch;
pub mod model;
pub mod pipeline;
pub mod projection;
pub mod reference;
pub mod runtime;
pub mod search;
pub mod selftest;
pub mod skip;
pub mod store;
pub mod vector;

pub use cli::{
    compact_scan_line, fetch_models, remove_models_command, run_scan_embedding, status_report,
};
pub use runtime::{document_input, query_input, with_embedder, Embedder, EmbedderConfig};
pub use search::{semantic_available, semantic_query, SemanticHit, SemanticSearcher};
