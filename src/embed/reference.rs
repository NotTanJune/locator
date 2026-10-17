//! DigUp's reference vectors: fixed inputs embedded by Google's mlx-vlm and by llama.cpp, which
//! agree at cosine 0.999 or more. A check here means our llama.cpp build still makes the same
//! vectors. The inputs are the strings DigUp builds with its `Prompts` (DigUpKit/Model.swift).
//! Source and license: tests/fixtures/digup-reference/NOTICE.md.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use super::runtime::{query_input, Embedder};

/// Each vector's cosine with its reference must be at least this.
pub const AGREEMENT: f32 = 0.999;

const REFERENCE_JSON: &str = include_str!("../../tests/fixtures/digup-reference/vectors.json");
const PHOTO_JPG: &[u8] = include_bytes!("../../tests/fixtures/digup-reference/photo.jpg");
const SCREEN_PNG: &[u8] = include_bytes!("../../tests/fixtures/digup-reference/screen.png");

#[derive(Debug, Deserialize)]
struct Reference {
    vector_version: u32,
    made_with: String,
    vectors: BTreeMap<String, Vec<f32>>,
}

/// One input's cosine with its reference vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub cosine: f32,
}

/// The text inputs DigUp embeds for its reference, under the names in vectors.json.
pub fn text_inputs() -> Vec<(&'static str, String)> {
    let long_document: String = (1..=60)
        .map(|n| {
            format!(
                "Note {n}: the review moved to room {} because the projector in the main hall failed again, \
                 and the team agreed to send the slides the night before.",
                n % 7 + 2
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        ("query", query_input("payment declined error")),
        ("query-bn", query_input("ইলিশ মাছ রান্না")),
        ("query-ar", query_input("متى تغلق المكتبة")),
        (
            "document",
            "title: Tokyo itinerary | text: Flight to Tokyo Haneda on November 3. Then the train to \
             Kamakura to see the Great Buddha, and back for dinner in Shibuya."
                .to_string(),
        ),
        (
            "document-long",
            format!("title: none | text: {long_document}"),
        ),
    ]
}

/// The image inputs, with DigUp's reference budget for each: a screenshot at 280 soft tokens, a
/// photo at 140, and a video frame (the same photo) at 70. Named as in vectors.json.
pub fn image_inputs() -> [(&'static str, &'static [u8], &'static str, u32); 3] {
    [
        ("screen-280", SCREEN_PNG, "png", 280),
        ("photo-140", PHOTO_JPG, "jpg", 140),
        ("frame-70", PHOTO_JPG, "jpg", 70),
    ]
}

/// Embeds the image inputs at their reference budgets and compares each with its reference.
/// Also returns the cosine of the photo and the screenshot (different images must stay apart).
/// Needs the vision model loaded.
pub fn check_images(embedder: &mut dyn Embedder) -> Result<(Vec<Check>, f32)> {
    let reference = load_reference()?;
    let mut embedded: Vec<(&'static str, Vec<f32>)> = Vec::new();
    let checks = image_inputs()
        .into_iter()
        .map(|(name, bytes, extension, budget)| {
            let file = tempfile::Builder::new()
                .suffix(&format!(".{extension}"))
                .tempfile()
                .context("create temp image")?;
            std::fs::write(file.path(), bytes).context("write temp image")?;
            let vector = embedder
                .embed_image_budget(file.path(), budget)
                .with_context(|| format!("embed {name}"))?;
            let expected = reference
                .vectors
                .get(name)
                .ok_or_else(|| anyhow!("no reference vector named {name}"))?;
            let check = Check {
                name,
                cosine: cosine(&vector, expected),
            };
            embedded.push((name, vector));
            Ok(check)
        })
        .collect::<Result<Vec<Check>>>()?;
    let vector_of = |name: &str| {
        embedded
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_slice())
            .ok_or_else(|| anyhow!("no embedded vector named {name}"))
    };
    let photo_vs_screen = cosine(vector_of("photo-140")?, vector_of("screen-280")?);
    Ok((checks, photo_vs_screen))
}

/// Embeds the text inputs with the production settings and compares each with its reference.
pub fn check_text(embedder: &mut dyn Embedder) -> Result<Vec<Check>> {
    let reference = load_reference()?;
    let inputs = text_inputs();
    let texts: Vec<String> = inputs.iter().map(|(_, text)| text.clone()).collect();
    let vectors = embedder.embed_batch(&texts)?;
    inputs
        .iter()
        .zip(vectors)
        .map(|((name, _), vector)| {
            let expected = reference
                .vectors
                .get(*name)
                .ok_or_else(|| anyhow!("no reference vector named {name}"))?;
            Ok(Check {
                name,
                cosine: cosine(&vector, expected),
            })
        })
        .collect()
}

/// Describes the reference file: the vector version and the llama.cpp build that made it.
pub fn reference_label() -> Result<String> {
    let reference = load_reference()?;
    Ok(format!(
        "vectors {} from {}",
        reference.vector_version, reference.made_with
    ))
}

fn load_reference() -> Result<Reference> {
    serde_json::from_str(REFERENCE_JSON).context("parse bundled reference vectors")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|v| v * v).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_reference_parses_and_names_every_text_input() {
        let reference = load_reference().expect("parse");
        assert_eq!(reference.vector_version, 1);
        for (name, _) in text_inputs() {
            let vector = reference.vectors.get(name).expect("named vector");
            assert_eq!(vector.len(), 768, "{name}");
        }
    }

    #[test]
    fn text_inputs_match_digup_prompt_formats() {
        let inputs = text_inputs();
        assert_eq!(
            inputs[0].1,
            "task: search result | query: payment declined error"
        );
        assert!(inputs[3]
            .1
            .starts_with("title: Tokyo itinerary | text: Flight to Tokyo Haneda"));
        assert!(inputs[3].1.contains("Then the train to Kamakura"));
        assert!(inputs[4]
            .1
            .starts_with("title: none | text: Note 1: the review moved to room 3"));
        assert!(inputs[4].1.contains("Note 60: the review moved to room 6"));
    }

    #[test]
    fn image_inputs_use_digup_budgets_and_named_vectors() {
        let reference = load_reference().expect("parse");
        let budgets: Vec<u32> = image_inputs().iter().map(|input| input.3).collect();
        assert_eq!(budgets, vec![280, 140, 70]);
        for (name, _, _, _) in image_inputs() {
            assert!(reference.vectors.contains_key(name), "{name}");
        }
    }

    #[test]
    fn cosine_of_identical_and_orthogonal_vectors() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }
}
