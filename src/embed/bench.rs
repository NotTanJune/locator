//! `lctr embed bench <dir>`: measures throughput settings on a fixed sample of real chunks.
//! Vectors from every setting are compared with single-sequence baseline vectors. A setting whose
//! minimum cosine falls below `MIN_COSINE` is rejected whatever its speed.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use super::chunk::{chunk_text, classify_and_extract, Extracted, SkipReason};
use super::runtime::{document_input, with_embedder_tuned, Embedder, EmbedderConfig, Tuning};
use super::search::embedder_config_for_search;

const SAMPLE: usize = 96;
const MIN_COSINE: f32 = 0.999;
const SEQS: [usize; 5] = [1, 2, 4, 8, 16];
const UBATCH: [u32; 5] = [512, 1024, 2048, 4096, 8192];
const BASELINE: Tuning = Tuning {
    max_seqs: 1,
    n_ubatch: 8192,
    flash: None,
};

struct Chunk {
    text: String,
}

#[derive(Debug)]
struct Row {
    tuning: Tuning,
    ordered: bool,
    outcome: std::result::Result<Measured, String>,
}

#[derive(Debug, Clone, Copy)]
struct Measured {
    tokens_per_second: f64,
    chunks_per_second: f64,
    min_cosine: f32,
}

/// Runs the grid over the chunks found under `dir` and prints one table row per setting.
/// `dump_vectors`: embed the sample with the production settings (`DEFAULT_TUNING`), write the
/// vectors as a JSON array of arrays to that path, and stop without running the grid. Used to
/// compare two builds (for example before and after a llama.cpp bump).
pub fn run_bench(dir: &Path, dump_vectors: Option<&Path>) -> Result<()> {
    let config = embedder_config_for_search()?;
    if config.fake {
        bail!("bench needs the real model; unset LCTR_EMBED_FAKE");
    }
    let chunks = collect_chunks(dir)?;
    if chunks.is_empty() {
        bail!("no embeddable text found under {}", dir.display());
    }
    let sample = sample_indices(chunks.len());
    let inputs: Vec<String> = sample
        .iter()
        .map(|&index| document_input(&chunks[index].text))
        .collect();
    println!(
        "bench: {} chunks found under {}, sample of {} (deterministic, evenly spaced)",
        chunks.len(),
        dir.display(),
        inputs.len()
    );
    if let Some(path) = dump_vectors {
        let vectors = embed_all(&config, super::runtime::DEFAULT_TUNING, &inputs, false)
            .context("embedding the sample with the production settings")?
            .vectors;
        let dims = vectors.first().map_or(0, Vec::len);
        std::fs::write(path, serde_json::to_string(&vectors)?)
            .with_context(|| format!("write {}", path.display()))?;
        println!(
            "dumped {} vectors of {} dims (production settings) to {}",
            vectors.len(),
            dims,
            path.display()
        );
        return Ok(());
    }

    let baseline = embed_all(&config, BASELINE, &inputs, false)
        .context("baseline single-sequence embedding")?
        .vectors;
    println!(
        "baseline: single sequence per decode, n_ubatch 8192, flash auto (vectors for cosine checks)"
    );

    let mut rows: Vec<Row> = Vec::new();
    for &max_seqs in &SEQS {
        for &n_ubatch in &UBATCH {
            for ordered in [false, true] {
                let tuning = Tuning {
                    max_seqs,
                    n_ubatch,
                    flash: None,
                };
                rows.push(measure(&config, tuning, ordered, &inputs, &baseline));
            }
        }
    }
    print_table(&rows);

    // Flash attention on and off, for the fastest accepted grid setting.
    if let Some(best) = best_row(&rows).map(|row| (row.tuning, row.ordered)) {
        let mut flash_rows = Vec::new();
        for flash in [Some(true), Some(false)] {
            let tuning = Tuning { flash, ..best.0 };
            flash_rows.push(measure(&config, tuning, best.1, &inputs, &baseline));
        }
        println!("flash attention check on the best grid setting:");
        print_table(&flash_rows);
        rows.extend(flash_rows);
    }

    match best_row(&rows) {
        Some(row) => {
            let measured = row.outcome.as_ref().expect("best row has a measurement");
            println!(
                "best accepted: seqs={} n_ubatch={} flash={} order={} at {:.0} tokens/s ({:.2} chunks/s, min cosine {:.6})",
                row.tuning.max_seqs,
                row.tuning.n_ubatch,
                flash_label(row.tuning.flash),
                if row.ordered { "sorted" } else { "input" },
                measured.tokens_per_second,
                measured.chunks_per_second,
                measured.min_cosine
            );
        }
        None => println!("no setting was accepted"),
    }
    Ok(())
}

/// Count-only label for files whose content matched the private key or access key guard.
const PRIVATE_KEY_RULE: &str = "private key";

fn measure(
    config: &EmbedderConfig,
    tuning: Tuning,
    ordered: bool,
    inputs: &[String],
    baseline: &[Vec<f32>],
) -> Row {
    let outcome = embed_all(config, tuning, inputs, ordered).map(|run| {
        let min_cosine = run
            .vectors
            .iter()
            .zip(baseline)
            .map(|(vector, reference)| dot(vector, reference))
            .fold(f32::INFINITY, f32::min);
        Measured {
            tokens_per_second: run.tokens as f64 / run.seconds,
            chunks_per_second: inputs.len() as f64 / run.seconds,
            min_cosine,
        }
    });
    Row {
        tuning,
        ordered,
        outcome: outcome.map_err(|error| format!("{error:#}")),
    }
}

struct Run {
    /// Vectors in input order.
    vectors: Vec<Vec<f32>>,
    tokens: u64,
    seconds: f64,
}

/// Embeds all inputs with one setting, timing only the embedding calls (not the model load).
fn embed_all(
    config: &EmbedderConfig,
    tuning: Tuning,
    inputs: &[String],
    ordered: bool,
) -> Result<Run> {
    let mut order: Vec<usize> = (0..inputs.len()).collect();
    if ordered {
        order.sort_by_key(|&index| inputs[index].len());
    }
    let sorted_inputs: Vec<String> = order.iter().map(|&index| inputs[index].clone()).collect();
    with_embedder_tuned(config, tuning, |embedder: &mut dyn Embedder| {
        let tokens_before = embedder.tokens_embedded();
        let started = Instant::now();
        let produced = embedder.embed_batch(&sorted_inputs)?;
        let seconds = started.elapsed().as_secs_f64();
        let tokens = embedder.tokens_embedded().saturating_sub(tokens_before);
        let mut vectors = vec![Vec::new(); inputs.len()];
        for (position, &index) in order.iter().enumerate() {
            vectors[index] = produced[position].clone();
        }
        Ok(Run {
            vectors,
            tokens,
            seconds,
        })
    })
}

fn best_row(rows: &[Row]) -> Option<&Row> {
    rows.iter()
        .filter_map(|row| match &row.outcome {
            Ok(measured) if measured.min_cosine >= MIN_COSINE => Some((row, measured)),
            _ => None,
        })
        .max_by(|a, b| {
            a.1.tokens_per_second
                .partial_cmp(&b.1.tokens_per_second)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(row, _)| row)
}

fn print_table(rows: &[Row]) {
    println!(
        "{:>5} {:>7} {:>6} {:>7} {:>10} {:>10} {:>11}  status",
        "seqs", "ubatch", "flash", "order", "tokens/s", "chunks/s", "min cosine"
    );
    for row in rows {
        match &row.outcome {
            Ok(measured) => {
                let status = if measured.min_cosine >= MIN_COSINE {
                    "ok".to_string()
                } else {
                    format!("rejected (cosine below {MIN_COSINE})")
                };
                println!(
                    "{:>5} {:>7} {:>6} {:>7} {:>10.0} {:>10.2} {:>11.6}  {}",
                    row.tuning.max_seqs,
                    row.tuning.n_ubatch,
                    flash_label(row.tuning.flash),
                    if row.ordered { "sorted" } else { "input" },
                    measured.tokens_per_second,
                    measured.chunks_per_second,
                    measured.min_cosine,
                    status
                );
            }
            Err(error) => println!(
                "{:>5} {:>7} {:>6} {:>7} {:>10} {:>10} {:>11}  error: {}",
                row.tuning.max_seqs,
                row.tuning.n_ubatch,
                flash_label(row.tuning.flash),
                if row.ordered { "sorted" } else { "input" },
                "-",
                "-",
                "-",
                error.lines().next().unwrap_or("")
            ),
        }
    }
}

fn flash_label(flash: Option<bool>) -> &'static str {
    match flash {
        Some(true) => "on",
        Some(false) => "off",
        None => "auto",
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Evenly spaced indices into `len` chunks, so the sample covers every file.
fn sample_indices(len: usize) -> Vec<usize> {
    if len <= SAMPLE {
        return (0..len).collect();
    }
    (0..SAMPLE).map(|k| k * len / SAMPLE).collect()
}

/// Walks `dir`, extracts and chunks every text-bearing file (symlinks skipped), in path order.
fn collect_chunks(dir: &Path) -> Result<Vec<Chunk>> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    files.sort();
    let mut chunks = Vec::new();
    for path in files {
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase());
        let kind = match extension.as_deref() {
            Some("pdf") => "pdf",
            Some("png" | "jpg" | "jpeg" | "gif" | "bmp") => "image",
            _ => "text",
        };
        if let Extracted::Text(text) =
            classify_and_extract(&path, kind, extension.as_deref(), metadata.len())
        {
            for chunk in chunk_text(&text) {
                chunks.push(Chunk { text: chunk.text });
            }
        }
    }
    Ok(chunks)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk(&entry.path(), out)?;
        } else if file_type.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_deterministic_and_evenly_spaced() {
        assert_eq!(sample_indices(5), vec![0, 1, 2, 3, 4]);
        let sample = sample_indices(960);
        assert_eq!(sample.len(), SAMPLE);
        assert_eq!(sample[0], 0);
        assert_eq!(sample[1], 10);
        assert_eq!(sample, sample_indices(960));
        assert!(sample.iter().all(|&index| index < 960));
    }

    #[test]
    fn rejects_settings_below_the_cosine_floor() {
        let fast_but_wrong = Row {
            tuning: BASELINE,
            ordered: false,
            outcome: Ok(Measured {
                tokens_per_second: 9999.0,
                chunks_per_second: 9.0,
                min_cosine: 0.9,
            }),
        };
        let slow_but_right = Row {
            tuning: Tuning {
                max_seqs: 4,
                ..BASELINE
            },
            ordered: false,
            outcome: Ok(Measured {
                tokens_per_second: 100.0,
                chunks_per_second: 1.0,
                min_cosine: 0.9999,
            }),
        };
        let rows = vec![fast_but_wrong, slow_but_right];
        let best = best_row(&rows).expect("one accepted row");
        assert_eq!(best.tuning.max_seqs, 4);
    }
}

/// Seconds `--count-only` may walk before it reports what it has.
const COUNT_DEADLINE_SECS: u64 = 60;

#[derive(Default)]
struct CountColumn {
    files: u64,
    embeddable_files: u64,
    chunks: u64,
    chars: u64,
    unique_chunk_texts: std::collections::HashSet<u64>,
}

impl CountColumn {
    fn add_chunk(&mut self, text: &str) {
        self.chunks += 1;
        self.chars += text.chars().count() as u64;
        self.unique_chunk_texts
            .insert(super::search::text_hash_for_count(text));
    }
}

/// Extracts and chunks every file under `dir` without loading the model. Reports the work the
/// embedding pass would do, with and without the low-value rules.
pub fn run_count_only(dir: &Path) -> Result<()> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("canonicalize {}", dir.display()))?;
    let home = dirs::home_dir();
    let started = std::time::Instant::now();
    let mut files = Vec::new();
    walk(&root, &mut files)?;
    files.sort();

    let mut before = CountColumn::default();
    let mut after = CountColumn::default();
    let mut by_rule: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut walked = 0u64;
    for path in &files {
        if started.elapsed().as_secs() >= COUNT_DEADLINE_SECS {
            break;
        }
        walked += 1;
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase());
        let kind = match extension.as_deref() {
            Some("pdf") => "pdf",
            Some("png" | "jpg" | "jpeg" | "gif" | "bmp") => "image",
            _ => "text",
        };
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let rule = super::skip::never_read_rule(
            relative,
            path,
            home.as_deref(),
            &super::skip::dir_has_vendor_manifest,
        );
        before.files += 1;
        if let Some(rule) = rule {
            *by_rule.entry(rule).or_default() += 1;
            // Secret and hidden files are never opened, not even to count them.
            if rule == super::skip::SECRET_RULE || rule == super::skip::HIDDEN_RULE {
                continue;
            }
        }
        let extracted = classify_and_extract(path, kind, extension.as_deref(), metadata.len());
        // The content guard is part of extraction. Its hits count as a rule and are not embeddable.
        let content_hit = matches!(extracted, Extracted::Skipped(SkipReason::PrivateKey));
        if content_hit {
            *by_rule.entry(PRIVATE_KEY_RULE).or_default() += 1;
        } else if rule.is_none() {
            after.files += 1;
        }
        let Extracted::Text(text) = extracted else {
            continue;
        };
        let chunks = chunk_text(&text);
        if chunks.is_empty() {
            continue;
        }
        before.embeddable_files += 1;
        for chunk in &chunks {
            before.add_chunk(&chunk.text);
        }
        if rule.is_none() {
            after.embeddable_files += 1;
            for chunk in &chunks {
                after.add_chunk(&chunk.text);
            }
        }
    }

    println!(
        "count-only over {} (walked {} of {} files in {:.1} s{})",
        root.display(),
        walked,
        files.len(),
        started.elapsed().as_secs_f64(),
        if walked < files.len() as u64 {
            format!("; stopped at the {COUNT_DEADLINE_SECS} s limit")
        } else {
            String::new()
        }
    );
    println!(
        "{:<20} {:>9} {:>11} {:>9} {:>14} {:>14}",
        "", "files", "text files", "chunks", "approx tokens", "unique chunks"
    );
    for (label, column) in [("before skip rules", &before), ("after skip rules", &after)] {
        println!(
            "{:<20} {:>9} {:>11} {:>9} {:>14} {:>14}",
            label,
            column.files,
            column.embeddable_files,
            column.chunks,
            (column.chars as f64 / 3.5).round() as u64,
            column.unique_chunk_texts.len()
        );
    }
    if by_rule.is_empty() {
        println!("files skipped by rule: none");
    } else {
        println!("files skipped by rule:");
        for (rule, count) in &by_rule {
            println!("  {count:>9}  {rule}");
        }
    }
    Ok(())
}
