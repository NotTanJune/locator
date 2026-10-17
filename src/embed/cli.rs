//! Command-level entry points for `lctr scan --em` and `lctr embed ...`. Terminal
//! prompts and plain progress output live here; the pass itself is in `pipeline`.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use super::model::{self, DownloadProgress, TEXT_MODEL, VISION_MODEL};
use super::pipeline::{
    depth_text, pass_done_text, run_pass, stage_label, Depth, PassEvent, PassRequest, PassSummary,
    PassTiming, MODEL_ID,
};
use super::runtime::fake_from_env;
use super::store::{sidecar_path_for_index, EmbedStore};
use crate::config::Config;
use crate::db::Database;

const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// Runs the embedding pass after `scan --em` has installed the filename index.
/// Runs the embedding pass for `scan --em`. `depth` is the `--depth` flag when given; otherwise the
/// `embed_depth` setting applies.
pub fn run_scan_embedding(index_path: &Path, root: &Path, depth: Option<&str>) -> Result<()> {
    let config = Config::load();
    let dims = usize::from(config.embed_dims);
    let depth_text = depth.unwrap_or(&config.embed_depth);
    let depth = Depth::parse(depth_text)
        .ok_or_else(|| anyhow!("unknown depth {depth_text:?}: use quick, balanced or deep"))?;
    let fake = fake_from_env();
    let root_canonical = root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", root.display()))?;
    let image_count = count_images(index_path, &root_canonical)?;

    let (text_model, vision_model) = if fake {
        (None, None)
    } else {
        let text = download_with_bar("text model", model::ensure_text_model)?;
        let vision = if image_count > 0 {
            choose_vision_model(image_count)?
        } else {
            None
        };
        (Some(text), vision)
    };

    let request = PassRequest {
        index_path: index_path.to_path_buf(),
        root: root.to_path_buf(),
        dims,
        text_model,
        vision_model,
        fake,
        depth,
    };
    let summary = if use_galaxy() {
        crate::embed_ui::run_embedding_screen(request)?
    } else {
        run_with_plain_progress(request)?
    };
    if io::stdout().is_terminal() {
        let width = crate::embed_ui::card_width_for_columns(
            crossterm::terminal::size()
                .map(|(columns, _)| columns)
                .unwrap_or(100),
        );
        println!(
            "{}",
            crate::embed_ui::render_embedding_card(&crate::embed_ui::CardInput {
                summary: &summary,
                root,
                backend: backend_label(),
                width,
            })
        );
    } else {
        println!("{}", summary_line(&summary));
    }
    if std::env::var_os("LCTR_TIMING").is_some() {
        eprintln!("{}", timing_report(&summary));
    }
    Ok(())
}

fn count_images(index_path: &Path, root: &Path) -> Result<usize> {
    let db = Database::open_readonly(index_path)
        .with_context(|| format!("open index {}", index_path.display()))?;
    let root_string = root.to_string_lossy().to_string();
    Ok(db
        .embeddable_files(&root_string)?
        .iter()
        .filter(|file| file.kind == "image")
        .count())
}

fn choose_vision_model(image_count: usize) -> Result<Option<PathBuf>> {
    if model::is_present(&VISION_MODEL) {
        let path = model::ensure_vision_model(&mut |_: DownloadProgress| {})?;
        return Ok(Some(path));
    }
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    if interactive {
        print!(
            "Found {image_count} images. Download the vision model ({} MB) to include them? [Y/n] ",
            VISION_MODEL.bytes / 1_000_000
        );
        io::stdout().flush().context("flush prompt")?;
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .context("read answer")?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            let path = download_with_bar("vision model", model::ensure_vision_model)?;
            return Ok(Some(path));
        }
    }
    eprintln!("images skipped; run `lctr embed fetch --vision` then rescan");
    Ok(None)
}

/// Set by the SIGINT handler while the plain progress sink is running.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_sigint(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Installs the SIGINT handler and returns the one it replaces. Unix only: elsewhere Ctrl-C keeps the
/// default behaviour and the plain progress output is not interrupted gracefully.
#[cfg(unix)]
fn install_sigint() -> libc::sighandler_t {
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) }
}

#[cfg(unix)]
fn restore_sigint(previous: libc::sighandler_t) {
    unsafe { libc::signal(libc::SIGINT, previous) };
}

/// The galaxy needs an interactive terminal of at least 40x10. `LCTR_EM_PLAIN=1` forces plain output.
fn use_galaxy() -> bool {
    if std::env::var_os("LCTR_EM_PLAIN").is_some_and(|value| value == "1") {
        return false;
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return false;
    }
    matches!(crossterm::terminal::size(), Ok((columns, rows)) if columns >= 40 && rows >= 10)
}

/// Runs the pass on a worker thread and prints a progress line to stderr every two seconds.
/// Ctrl-C stops the pass and keeps what was embedded. The previous SIGINT handler is restored.
fn run_with_plain_progress(request: PassRequest) -> Result<PassSummary> {
    let (tx, rx) = mpsc::channel::<PassEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    INTERRUPTED.store(false, Ordering::SeqCst);
    #[cfg(unix)]
    let previous = install_sigint();
    let worker = thread::spawn(move || run_pass(&request, tx, worker_cancel));

    let started = Instant::now();
    let mut last_status = Instant::now();
    let mut chunks = 0usize;
    let mut files_done = 0usize;
    let mut total = 0usize;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            cancel.store(true, Ordering::SeqCst);
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                match event {
                    PassEvent::Started {
                        total_files,
                        backend,
                        resumed,
                    } => {
                        total = total_files;
                        let resume = if resumed {
                            " (resuming an earlier pass)"
                        } else {
                            ""
                        };
                        eprintln!("embedding {total_files} files with {backend}{resume}");
                    }
                    PassEvent::ChunksEmbedded { vectors, .. } => chunks += vectors.len(),
                    PassEvent::FileFinished { .. } => files_done += 1,
                    PassEvent::StageStarted {
                        stage,
                        stages,
                        files,
                        ..
                    } => {
                        total = files;
                        files_done = 0;
                        eprintln!("{} ({files} files)", stage_label(stage, stages));
                    }
                    PassEvent::StageFinished { stage, stages } => {
                        eprintln!("{}", pass_done_text(stage, stages));
                    }
                    PassEvent::FileStarted { .. } | PassEvent::Finished(_) => {}
                }
                if total > 0 && last_status.elapsed() >= STATUS_INTERVAL {
                    let rate = chunks as f64 / started.elapsed().as_secs_f64().max(0.001);
                    eprintln!(
                        "embedded {chunks} chunks, {files_done}/{total} files, {rate:.1} chunks/s"
                    );
                    last_status = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let joined = worker
        .join()
        .map_err(|_| anyhow!("embedding pass panicked"));
    #[cfg(unix)]
    restore_sigint(previous);
    joined?
}

/// Backend name the pass uses on this platform.
fn backend_label() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "Metal"
    } else {
        "CPU"
    }
}

/// One line printed before the embedding pass, in place of the filename-scan dashboard.
pub fn compact_scan_line(stats: &crate::scanner::ScanStats, color: bool) -> String {
    let head = if color {
        "\x1b[1;32mIndexed\x1b[0m"
    } else {
        "Indexed"
    };
    format!(
        "{head} {} files in {:.1}s ({} skipped, {} warnings). Embedding contents next.",
        stats.indexed_files,
        stats.profile.total.as_secs_f64(),
        stats.skipped_entries,
        stats.error_entries
    )
}

/// Timing block for `LCTR_TIMING=1`: where the pass spent its time.
fn timing_report(summary: &PassSummary) -> String {
    let timing = &summary.timing;
    let wall = summary.elapsed.as_secs_f64().max(1e-9);
    let gpu = timing.gpu.as_secs_f64();
    let wait = timing.embed_wait.as_secs_f64();
    let stage_lines: String = timing
        .stages
        .iter()
        .map(|stage| {
            format!(
                "\n  stage {}          wall {:.2} s, {} chunks, {} tokens",
                stage.stage,
                stage.wall.as_secs_f64(),
                stage.chunks,
                stage.tokens
            )
        })
        .collect();
    let first_stage = match timing.first_stage {
        Some(time) => format!(
            "\n  end of stage 1  {:.2} s after the pass started",
            time.as_secs_f64()
        ),
        None => "\n  end of stage 1  not reached".to_string(),
    };
    format!(
        "LCTR_TIMING embedding pass\n  model load      {:.2} s\n  wall            {:.2} s\n  gpu busy        {:.2} s ({:.1}% of wall, {} calls)\n  embed waiting   {:.2} s ({:.1}% of wall)\n  tokens          {} ({:.0} tokens/s over wall, {:.0} tokens/s GPU)\n  chunks          {} ({:.2} chunks/s over wall)\n  extraction      {:.2} s summed over workers\n  writer          {:.2} s{first_stage}{stage_lines}",
        timing.model_load.as_secs_f64(),
        wall,
        gpu,
        100.0 * gpu / wall,
        timing.calls,
        wait,
        100.0 * wait / wall,
        timing.tokens,
        timing.tokens as f64 / wall,
        timing.tokens as f64 / gpu.max(1e-9),
        summary.chunks,
        summary.chunks as f64 / wall,
        timing.extract.as_secs_f64(),
        timing.writer.as_secs_f64(),
    ) + &extra_timing_lines(timing)
}

/// Dedupe and low-value counts for the timing block.
fn extra_timing_lines(timing: &PassTiming) -> String {
    let mut out = format!(
        "\n  deduplicated    {} chunks reused from this pass (not embedded)",
        timing.deduplicated
    );
    for (rule, count) in &timing.low_value {
        out.push_str(&format!("\n  low-value skip  {count} files: {rule}"));
    }
    out
}

/// The last line of plain output. Its depth sentence is the one the summary card prints.
fn summary_line(summary: &PassSummary) -> String {
    let elapsed = format_duration(summary.elapsed);
    let depth = depth_text(summary.depth, summary.stages_done, summary.cancelled);
    if summary.cancelled {
        format!(
            "Embedded {} chunks from {} files so far ({} skipped, {} failed) in {elapsed}. {depth}",
            summary.chunks, summary.files_embedded, summary.files_skipped, summary.files_failed
        )
    } else {
        format!(
            "Embedded {} chunks from {} files ({} skipped, {} failed) in {elapsed}. {depth} Press m in lctr search until the mode reads \"semantic\".",
            summary.chunks, summary.files_embedded, summary.files_skipped, summary.files_failed
        )
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Downloads a model with a progress bar and returns its path.
pub(crate) fn download_with_bar(
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
    .with_context(|| format!("download {label}"))?;
    bar.finish_and_clear();
    Ok(path)
}

/// `lctr embed fetch [--vision]`
pub fn fetch_models(vision: bool) -> Result<()> {
    let text = download_with_bar("text model", model::ensure_text_model)?;
    println!("text model: {}", text.display());
    if vision {
        let path = download_with_bar("vision model", model::ensure_vision_model)?;
        println!("vision model: {}", path.display());
    }
    Ok(())
}

/// `lctr embed remove-model [--yes]`. Never touches the sidecar vectors.
pub fn remove_models_command(yes: bool) -> Result<()> {
    if !yes {
        if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
            bail!(
                "refusing to delete embedding models without --yes on a non-interactive terminal"
            );
        }
        let on_disk = models_bytes_on_disk()?;
        print!(
            "Delete downloaded embedding models (frees {} MB)? [y/N] ",
            on_disk / 1_000_000
        );
        io::stdout().flush().context("flush prompt")?;
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .context("read answer")?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("kept the embedding models");
            return Ok(());
        }
    }
    let freed = model::remove_models()?;
    println!(
        "removed {} MB of embedding models. Vectors in the embeddings sidecar were not touched.",
        freed / 1_000_000
    );
    Ok(())
}

fn models_bytes_on_disk() -> Result<u64> {
    let mut total = 0u64;
    for file in [&TEXT_MODEL, &VISION_MODEL] {
        let path = model::model_path(file)?;
        if let Ok(metadata) = std::fs::metadata(&path) {
            total += metadata.len();
        }
    }
    Ok(total)
}

/// `lctr embed status`: model presence, the configured width, and the nearest index's sidecar.
pub fn status_report(index_path: Option<&Path>) -> Result<String> {
    let mut out = String::new();
    let config = Config::load();
    for file in [&TEXT_MODEL, &VISION_MODEL] {
        let path = model::model_path(file)?;
        let state = match std::fs::metadata(&path) {
            Ok(metadata) if model::is_present(file) => {
                format!(
                    "present ({} MB) at {}",
                    metadata.len() / 1_000_000,
                    path.display()
                )
            }
            _ => "not downloaded".to_string(),
        };
        out.push_str(&format!("{}: {state}\n", file.name));
    }
    out.push_str(&format!(
        "text model bundled in this binary: {}\n",
        if cfg!(feature = "bundled-model") {
            "yes"
        } else {
            "no"
        }
    ));
    out.push_str(&format!("embed_dims: {}\n", config.embed_dims));
    let backend = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "Metal"
    } else {
        "CPU"
    };
    out.push_str(&format!("backend that would be used: {backend}\n"));
    out.push_str(&format!("model id: {MODEL_ID}\n"));

    let Some(index) = index_path else {
        out.push_str("no index found here; run lctr scan <dir> first\n");
        return Ok(out);
    };
    let sidecar = sidecar_path_for_index(index);
    out.push_str(&format!("embeddings sidecar: {}\n", sidecar.display()));
    let store = match EmbedStore::open_readonly(&sidecar)? {
        Some(store) => store,
        None => {
            out.push_str("no embeddings yet; run lctr scan <dir> --em\n");
            return Ok(out);
        }
    };
    let stats = store.stats()?;
    if stats.chunks == 0 && stats.files == 0 {
        out.push_str("no embeddings yet; run lctr scan <dir> --em\n");
        return Ok(out);
    }
    out.push_str(&format!(
        "files: {}, chunks: {}, skipped: {}, failed: {}, sidecar size: {:.1} MB\n",
        stats.files,
        stats.chunks,
        stats.skipped,
        stats.failed,
        stats.sidecar_bytes as f64 / 1_000_000.0
    ));
    for root in store.roots()? {
        let state = match store.is_complete(&root)? {
            Some(true) => "complete",
            _ => "incomplete (run lctr scan <dir> --em to resume)",
        };
        out.push_str(&format!("root {root}: {state}\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use crate::embed::pipeline::{Depth, StageTiming};

    #[test]
    fn timing_block_lists_each_stage_and_the_end_of_stage_one() {
        let summary = PassSummary {
            files_embedded: 2,
            files_skipped: 0,
            files_failed: 0,
            files_unchanged: 0,
            chunks: 300,
            cancelled: false,
            elapsed: Duration::from_secs(30),
            sidecar_bytes: 1,
            timing: PassTiming {
                first_stage: Some(Duration::from_secs(12)),
                stages: vec![
                    StageTiming {
                        stage: 1,
                        wall: Duration::from_secs(12),
                        chunks: 100,
                        tokens: 9000,
                    },
                    StageTiming {
                        stage: 2,
                        wall: Duration::from_secs(18),
                        chunks: 200,
                        tokens: 18000,
                    },
                ],
                ..PassTiming::default()
            },
            depth: Depth::Balanced,
            stages_done: 2,
        };
        let text = timing_report(&summary);
        assert!(text.contains("end of stage 1  12.00 s"), "{text}");
        assert!(
            text.contains("stage 1          wall 12.00 s, 100 chunks, 9000 tokens"),
            "{text}"
        );
        assert!(
            text.contains("stage 2          wall 18.00 s, 200 chunks, 18000 tokens"),
            "{text}"
        );
    }
}
