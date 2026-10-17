//! Deterministic embedder that needs no model. It hashes words into NATIVE_DIMS
//! buckets, so texts that share words get higher cosine similarity. Used by tests
//! and by `LCTR_EMBED_FAKE=1`.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

use super::runtime::Embedder;
use super::vector::{l2_normalize, NATIVE_DIMS};

const TASK_PREFIXES: [&str; 2] = ["title: none | text: ", "task: search result | query: "];
const IMAGE_HEAD_BYTES: u64 = 4096;

#[derive(Debug, Default, Clone, Copy)]
pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn backend_label(&self) -> &str {
        "fake"
    }

    fn supports_images(&self) -> bool {
        true
    }

    fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|text| hash_text(text)).collect())
    }

    fn embed_image(&mut self, path: &Path) -> Result<Vec<f32>> {
        let mut head = Vec::new();
        File::open(path)
            .with_context(|| format!("open {}", path.display()))?
            .take(IMAGE_HEAD_BYTES)
            .read_to_end(&mut head)
            .with_context(|| format!("read {}", path.display()))?;
        let mut vector = vec![0f32; NATIVE_DIMS];
        for window in head.chunks(8) {
            accumulate(&mut vector, fnv1a(window));
        }
        Ok(l2_normalize(&vector))
    }
}

/// Hashes the words of `text` (task prefix removed) into a normalized vector.
pub fn hash_text(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let body = TASK_PREFIXES
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix))
        .unwrap_or(lower.as_str());
    let mut vector = vec![0f32; NATIVE_DIMS];
    for word in body
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        accumulate(&mut vector, fnv1a(word.as_bytes()));
    }
    l2_normalize(&vector)
}

fn accumulate(vector: &mut [f32], hash: u64) {
    let index = (hash % NATIVE_DIMS as u64) as usize;
    let sign = if (hash >> 32) & 1 == 0 { 1.0 } else { -1.0 };
    vector[index] += sign;
}

/// FNV-1a, stable across releases (unlike `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::vector::dot_f16;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn output_is_deterministic_and_normalized() {
        let first = hash_text("quarterly budget marketing spend");
        let second = hash_text("quarterly budget marketing spend");
        assert_eq!(first, second);
        assert_eq!(first.len(), NATIVE_DIMS);
        let norm: f32 = first.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn prefix_does_not_change_the_vector() {
        let bare = hash_text("tomato soup");
        let prefixed = hash_text("title: none | text: tomato soup");
        assert_eq!(bare, prefixed);
        let query = hash_text("task: search result | query: tomato soup");
        assert_eq!(bare, query);
    }

    #[test]
    fn shared_words_score_higher_than_unrelated_text() {
        let budget = hash_text("quarterly budget marketing spend spreadsheet");
        let shared = hash_text("marketing budget");
        let unrelated = hash_text("tomato soup recipe with basil");
        assert!(cosine(&budget, &shared) > cosine(&budget, &unrelated));
        assert!(cosine(&budget, &shared) > 0.0);
    }

    #[test]
    fn embed_batch_returns_one_vector_per_input_in_order() {
        let mut embedder = HashEmbedder;
        let inputs = vec!["alpha beta".to_string(), "gamma".to_string()];
        let vectors = embedder.embed_batch(&inputs).expect("embed");
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], hash_text("alpha beta"));
        assert_eq!(vectors[1], hash_text("gamma"));
        assert_eq!(embedder.backend_label(), "fake");
        assert!(embedder.supports_images());
    }

    #[test]
    fn images_with_different_bytes_get_different_vectors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("a.png");
        let second = dir.path().join("b.png");
        std::fs::write(&first, vec![1u8; 512]).expect("write a");
        std::fs::write(&second, vec![7u8; 512]).expect("write b");
        let mut embedder = HashEmbedder;
        let a = embedder.embed_image(&first).expect("embed a");
        let b = embedder.embed_image(&second).expect("embed b");
        assert_ne!(a, b);
        let doc = dot_f16(&a, &crate::embed::vector::encode_f16(&a));
        assert!((doc - 1.0).abs() < 1e-3);
    }
}
