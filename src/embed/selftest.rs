//! Hidden `lctr embed selftest`: real-model checks for the Metal backend,
//! throughput, batch correctness, semantic ranking, and image embedding.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use super::model::{self, DownloadProgress};
use super::runtime::{document_input, query_input, with_embedder, Embedder, EmbedderConfig};

const PARAGRAPH: &str = "Our quarterly planning notes cover the marketing budget, the staffing plan for the support team, and the timeline for the new billing system. Each section lists owners, open questions, and the decisions still waiting for sign off from finance. Please review the figures before Friday so the final spreadsheet can be shared with the leadership group. ";
const BATCH_INPUTS: usize = 16;
const BATCH_REPEATS: usize = 5;
const MIN_CHUNKS_PER_SECOND: f64 = 5.0;
const MIN_BATCH_COSINE: f32 = 0.999;

/// Compares the text vectors with DigUp's reference (`reference.rs`), one line per input. Fails
/// when any cosine is below `reference::AGREEMENT`.
pub fn run_reference() -> Result<()> {
    let text_model = with_download_bar("text model", model::ensure_text_model)?;
    let vision_model = with_download_bar("vision model", model::ensure_vision_model)?;
    let config = EmbedderConfig {
        text_model: Some(text_model),
        vision_model: Some(vision_model),
        fake: false,
    };
    with_embedder(&config, |embedder| {
        ensure!(
            embedder.backend_label() != "fake",
            "reference check needs the real model; unset LCTR_EMBED_FAKE"
        );
        println!("backend: {}", embedder.backend_label());
        println!("reference: {}", super::reference::reference_label()?);
        let mut checks = super::reference::check_text(embedder)?;
        let (image_checks, photo_vs_screen) = super::reference::check_images(embedder)?;
        checks.extend(image_checks);
        let mut failures = Vec::new();
        if photo_vs_screen > MAX_IMAGE_SIMILARITY {
            failures.push(format!(
                "photo and screenshot cosine {photo_vs_screen:.6} (above {MAX_IMAGE_SIMILARITY})"
            ));
        }
        println!("photo-140 vs screen-280 cosine {photo_vs_screen:.6} (must stay below {MAX_IMAGE_SIMILARITY})");
        for check in &checks {
            println!("{:<14} cosine {:.6}", check.name, check.cosine);
            if check.cosine < super::reference::AGREEMENT {
                failures.push(format!("{} {:.6}", check.name, check.cosine));
            }
        }
        if failures.is_empty() {
            println!(
                "reference: PASS (every cosine >= {})",
                super::reference::AGREEMENT
            );
            Ok(())
        } else {
            bail!("reference: FAIL ({})", failures.join("; "))
        }
    })
}

/// Runs every check, printing one line per result. Returns an error when a gate fails.
pub fn run(image: bool) -> Result<()> {
    let text_model = with_download_bar("text model", model::ensure_text_model)?;
    let vision_model = if image {
        Some(with_download_bar(
            "vision model",
            model::ensure_vision_model,
        )?)
    } else {
        None
    };
    let config = EmbedderConfig {
        text_model: Some(text_model),
        vision_model,
        fake: false,
    };

    with_embedder(&config, |embedder| {
        ensure!(
            embedder.backend_label() != "fake",
            "selftest needs the real model; unset LCTR_EMBED_FAKE"
        );
        println!("backend: {}", embedder.backend_label());
        let mut failures: Vec<String> = Vec::new();

        check_batch_and_throughput(embedder, &mut failures)?;
        check_semantic_ranking(embedder, &mut failures)?;
        if image {
            check_image(embedder, &mut failures)?;
        }

        if failures.is_empty() {
            println!("selftest: PASS");
            Ok(())
        } else {
            bail!("selftest: FAIL ({})", failures.join("; "))
        }
    })
}

fn check_batch_and_throughput(
    embedder: &mut dyn Embedder,
    failures: &mut Vec<String>,
) -> Result<()> {
    let paragraph = PARAGRAPH.repeat(BATCH_REPEATS);
    let inputs: Vec<String> = (0..BATCH_INPUTS)
        .map(|index| document_input(&format!("Document {index}. {paragraph}")))
        .collect();
    println!(
        "inputs: {} documents of {} characters each (about 400 tokens)",
        inputs.len(),
        inputs[0].len()
    );

    let cold_started = Instant::now();
    let first = embedder.embed_batch(&inputs).context("first batch")?;
    let cold = cold_started.elapsed().as_secs_f64();

    let warm_started = Instant::now();
    let batched = embedder.embed_batch(&inputs).context("second batch")?;
    let warm = warm_started.elapsed().as_secs_f64();

    ensure!(
        batched.len() == BATCH_INPUTS,
        "batch returned {} vectors",
        batched.len()
    );
    ensure!(
        batched
            .iter()
            .all(|v| v.len() == 768 && v.iter().all(|x| x.is_finite())),
        "batch vectors are not 768 finite floats"
    );
    ensure!(
        first.len() == batched.len(),
        "repeated batch changed its length"
    );

    let cold_rate = BATCH_INPUTS as f64 / cold;
    let warm_rate = BATCH_INPUTS as f64 / warm;
    println!("throughput cold: {cold_rate:.2} chunks/s ({cold:.2} s for {BATCH_INPUTS})");
    println!("throughput warm: {warm_rate:.2} chunks/s ({warm:.2} s for {BATCH_INPUTS})");
    if warm_rate < MIN_CHUNKS_PER_SECOND {
        failures.push(format!(
            "throughput {warm_rate:.2} below {MIN_CHUNKS_PER_SECOND}"
        ));
    }

    let mut min_cosine = f32::MAX;
    for index in [0usize, 5, 10, 15] {
        let single = embedder
            .embed_batch(std::slice::from_ref(&inputs[index]))
            .context("single embedding")?;
        min_cosine = min_cosine.min(cosine(&batched[index], &single[0]));
    }
    println!("batch vs single min cosine (4 inputs): {min_cosine:.6}");
    if min_cosine < MIN_BATCH_COSINE {
        failures.push(format!(
            "batch-vs-single cosine {min_cosine:.6} below {MIN_BATCH_COSINE}"
        ));
    }
    Ok(())
}

fn check_semantic_ranking(embedder: &mut dyn Embedder, failures: &mut Vec<String>) -> Result<()> {
    let query = embed_one(embedder, &query_input("invoice for office chairs"))?;
    let chairs = embed_one(
        embedder,
        &document_input("Order confirmation for 12 ergonomic office chairs, total 3,400 dollars"),
    )?;
    let soup = embed_one(
        embedder,
        &document_input("Recipe for tomato soup with basil"),
    )?;
    let chairs_score = cosine(&query, &chairs);
    let soup_score = cosine(&query, &soup);
    println!("cosine(chairs query, chairs document) = {chairs_score:.6}");
    println!("cosine(chairs query, soup document)   = {soup_score:.6}");
    if chairs_score <= soup_score {
        failures.push("chairs document did not outrank soup document".to_string());
    }
    Ok(())
}

/// Two different images must not get vectors this close: a constant-vector bug (every image
/// gets the same vector) fails here.
const MAX_IMAGE_SIMILARITY: f32 = 0.99;

fn check_image(embedder: &mut dyn Embedder, failures: &mut Vec<String>) -> Result<()> {
    ensure!(embedder.supports_images(), "vision model is not loaded");
    let colours: [(&str, [u8; 3]); 3] = [
        ("red", [255, 0, 0]),
        ("green", [0, 255, 0]),
        ("blue", [0, 0, 255]),
    ];
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    for (name, rgb) in colours {
        let file = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .context("create temp image")?;
        image::RgbImage::from_pixel(224, 224, image::Rgb(rgb))
            .save(file.path())
            .with_context(|| format!("write {name} test image"))?;
        let started = Instant::now();
        let vector = embedder
            .embed_image(file.path())
            .with_context(|| format!("embed {name} image"))?;
        println!(
            "{name} image embedding: {:.3} s",
            started.elapsed().as_secs_f64()
        );
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        let finite = vector.iter().all(|v| v.is_finite());
        if !finite || (norm - 1.0).abs() > 1e-3 {
            failures.push(format!(
                "{name} image vector invalid (norm {norm:.6}, finite {finite})"
            ));
        }
        vectors.push(vector);
    }

    for i in 0..colours.len() {
        for j in i + 1..colours.len() {
            let similarity = cosine(&vectors[i], &vectors[j]);
            println!(
                "cosine({} image, {} image) = {similarity:.6}",
                colours[i].0, colours[j].0
            );
            if similarity > MAX_IMAGE_SIMILARITY {
                failures.push(format!(
                    "{} and {} images have cosine {similarity:.6} (above {MAX_IMAGE_SIMILARITY}): \
                     the image vectors do not depend on the image",
                    colours[i].0, colours[j].0
                ));
            }
        }
    }

    // Rows are images, columns are the texts "a solid <colour> image". Each colour's image must
    // be closest to its own text, which is the diagonal of this matrix.
    let texts: Vec<Vec<f32>> = colours
        .iter()
        .map(|(name, _)| embed_one(embedder, &query_input(&format!("a solid {name} image"))))
        .collect::<Result<_>>()?;
    println!("cosine matrix (rows: images red, green, blue; columns: texts red, green, blue):");
    let mut matrix = [[0f32; 3]; 3];
    for (i, (name, _)) in colours.iter().enumerate() {
        for (j, text) in texts.iter().enumerate() {
            matrix[i][j] = cosine(&vectors[i], text);
        }
        println!(
            "  {name:<6} {:.6} {:.6} {:.6}",
            matrix[i][0], matrix[i][1], matrix[i][2]
        );
    }
    for j in 0..colours.len() {
        for i in 0..colours.len() {
            if i != j && matrix[i][j] >= matrix[j][j] {
                failures.push(format!(
                    "the {} image is not closer to \"a solid {} image\" than the {} image is",
                    colours[j].0, colours[j].0, colours[i].0
                ));
            }
        }
    }
    Ok(())
}

fn embed_one(embedder: &mut dyn Embedder, text: &str) -> Result<Vec<f32>> {
    let mut vectors = embedder.embed_batch(&[text.to_string()])?;
    vectors.pop().context("embedder returned no vector")
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

fn with_download_bar(
    label: &str,
    ensure: fn(&mut dyn FnMut(DownloadProgress)) -> Result<PathBuf>,
) -> Result<PathBuf> {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes}")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    bar.set_message(label.to_string());
    let path = ensure(&mut |progress: DownloadProgress| {
        bar.set_length(progress.total);
        bar.set_position(progress.downloaded);
    })
    .with_context(|| format!("ensure {label}"))?;
    bar.finish_and_clear();
    println!("{label}: {}", path.display());
    Ok(path)
}

/// Embeds each image `runs` times at vision budget `budget`. Prints the soft tokens and seconds
/// per run (with `uptime` for load context), and writes the first run's vectors as JSON
/// (file name to vector) when `dump` is given. Used to compare image embeddings across builds.
pub fn image_probe(files: &[PathBuf], budget: u32, runs: usize, dump: Option<&Path>) -> Result<()> {
    ensure!(runs >= 1, "runs must be at least 1");
    let text_model = with_download_bar("text model", model::ensure_text_model)?;
    let vision_model = with_download_bar("vision model", model::ensure_vision_model)?;
    let config = EmbedderConfig {
        text_model: Some(text_model),
        vision_model: Some(vision_model),
        fake: false,
    };
    with_embedder(&config, |embedder| {
        ensure!(embedder.supports_images(), "vision model is not loaded");
        println!("uptime: {}", uptime_line());
        println!(
            "budget {budget}, runs {runs}, backend {}",
            embedder.backend_label()
        );
        let mut dumped: std::collections::BTreeMap<String, Vec<f32>> =
            std::collections::BTreeMap::new();
        for file in files {
            let mut seconds = Vec::with_capacity(runs);
            let mut first: Option<Vec<f32>> = None;
            for run in 0..runs {
                let started = Instant::now();
                let vector = embedder
                    .embed_image_budget(file, budget)
                    .with_context(|| format!("embed {}", file.display()))?;
                seconds.push(started.elapsed().as_secs_f64());
                if run == 0 {
                    first = Some(vector);
                }
            }
            let tokens = embedder.last_image_tokens();
            let mut sorted = seconds.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let median = sorted[sorted.len() / 2];
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            println!(
                "{name}: tokens {}, seconds per run {}, median {median:.3}",
                tokens.map_or("?".to_string(), |t| t.to_string()),
                seconds
                    .iter()
                    .map(|s| format!("{s:.3}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if let Some(vector) = first {
                dumped.insert(name, vector);
            }
        }
        if let Some(path) = dump {
            std::fs::write(path, serde_json::to_string(&dumped)?)
                .with_context(|| format!("write {}", path.display()))?;
            println!("dumped {} vectors to {}", dumped.len(), path.display());
        }
        Ok(())
    })
}

fn uptime_line() -> String {
    std::process::Command::new("uptime")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the same vector for every image: the bug the image check must catch.
    struct ConstantImageEmbedder;

    impl Embedder for ConstantImageEmbedder {
        fn backend_label(&self) -> &str {
            "constant"
        }
        fn supports_images(&self) -> bool {
            true
        }
        fn embed_batch(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(inputs.iter().map(|_| unit_vector(0)).collect())
        }
        fn embed_image(&mut self, _path: &Path) -> Result<Vec<f32>> {
            Ok(unit_vector(0))
        }
    }

    /// A unit vector that depends on `seed`, for text inputs.
    fn unit_vector(seed: usize) -> Vec<f32> {
        let mut v = vec![0f32; 768];
        v[seed % 768] = 1.0;
        v[(seed * 7 + 3) % 768] += 0.5;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    #[test]
    fn constant_image_vectors_fail_the_distinctness_check() {
        let mut failures = Vec::new();
        check_image(&mut ConstantImageEmbedder, &mut failures).expect("check runs");
        assert!(
            failures
                .iter()
                .any(|f| f.contains("do not depend on the image")),
            "{failures:?}"
        );
    }
}
