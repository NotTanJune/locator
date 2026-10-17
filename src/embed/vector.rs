//! Pure vector helpers: Matryoshka truncation, f16 storage, and dot products.

use half::f16;

/// Native output width of EmbeddingGemma 2.
pub const NATIVE_DIMS: usize = 768;
/// Matryoshka truncation widths accepted by the `embed_dims` setting.
pub const ALLOWED_DIMS: [usize; 4] = [128, 256, 512, 768];

/// Matryoshka: keep the first `dims` values, then L2-normalize. Zero vectors stay zero.
pub fn truncate_and_normalize(vector: &[f32], dims: usize) -> Vec<f32> {
    let take = dims.min(vector.len());
    let mut out = vector[..take].to_vec();
    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut out {
            *value /= norm;
        }
    }
    out
}

/// L2-normalize a copy of `vector`. Zero vectors stay zero.
pub fn l2_normalize(vector: &[f32]) -> Vec<f32> {
    truncate_and_normalize(vector, vector.len())
}

/// Little-endian f16 per component.
pub fn encode_f16(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 2);
    for value in vector {
        out.extend_from_slice(&f16::from_f32(*value).to_bits().to_le_bytes());
    }
    out
}

/// Decodes little-endian f16 values. A trailing odd byte is ignored.
pub fn decode_f16(bytes: &[u8]) -> Vec<f32> {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|pair| f16::from_bits(u16::from_le_bytes(*pair)).to_f32())
        .collect()
}

/// Dot product of a query (f32) with an f16-encoded document vector. Both are
/// expected to be L2-normalized, so this is cosine similarity. A trailing odd
/// byte is ignored.
pub fn dot_f16(query: &[f32], document: &[u8]) -> f32 {
    let (pairs, _) = document.as_chunks::<2>();
    query
        .iter()
        .zip(pairs.iter())
        .map(|(q, pair)| q * f16::from_bits(u16::from_le_bytes(*pair)).to_f32())
        .sum()
}
