use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use locator::db::{
    delete_index_files, existing_db_path_for_working_dir, fallback_db_path_for_root,
    local_db_path_for_root, staging_db_path_for_root, Database,
};
use locator::query::{QueryMode, SearchFilters, SearchOptions, SortField};
use locator::scan_ui::{render_scan_frame_with_eta, ScanAnimation};
use locator::scanner::{scan_root_with_progress, ScanBackend, ScanOptions};

#[derive(Debug, Parser)]
#[command(name = "lctr", version, about = "Fast local file metadata search")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    Scan {
        #[arg(default_value_os_t = home_dir())]
        root: PathBuf,
        #[arg(long, value_enum, default_value_t = ScanBackend::Dirent)]
        backend: ScanBackend,
        #[arg(long, default_value_t = 500_000)]
        batch_size: usize,
        #[arg(long, default_value_t = 32)]
        writer_queue_batches: usize,
        #[arg(long, default_value_t = 16)]
        native_buffer_mb: usize,
        #[arg(long, default_value_t = 8)]
        native_workers: usize,
        #[arg(long, default_value_t = 4096)]
        native_output_batch_size: usize,
        #[arg(long)]
        stage_index: bool,
        #[arg(long)]
        no_stage_index: bool,
        #[arg(long)]
        profile_detail: bool,
        #[arg(long)]
        no_profile_detail: bool,
        #[arg(long)]
        eta: bool,
        #[arg(long)]
        no_eta: bool,
    },
    ShellInit {
        shell: String,
    },
    Status,
    Search {
        #[arg(default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))]
        root: PathBuf,
    },
    Find(FindArgs),
    Watch {
        #[arg(default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))]
        root: PathBuf,
    },
    Roots,
    RemoveRoot {
        root: String,
    },
    DeleteIndex {
        #[arg(default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))]
        root: PathBuf,
    },
    Vacuum,
}

#[derive(Debug, Args)]
struct FindArgs {
    query: String,
    #[arg(long, value_enum, default_value_t = QueryMode::Contains)]
    mode: QueryMode,
    #[arg(long, value_enum, default_value_t = SortField::Relevance)]
    sort: SortField,
    #[arg(long)]
    reverse: bool,
    #[arg(long = "type")]
    kind: Option<String>,
    #[arg(long)]
    ext: Option<String>,
    #[arg(long)]
    min_size: Option<String>,
    #[arg(long)]
    max_size: Option<String>,
    #[arg(long)]
    created_after: Option<String>,
    #[arg(long)]
    created_before: Option<String>,
    #[arg(long)]
    modified_after: Option<String>,
    #[arg(long)]
    modified_before: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan {
            root,
            backend,
            batch_size,
            writer_queue_batches,
            native_buffer_mb,
            native_workers,
            native_output_batch_size,
            stage_index,
            no_stage_index,
            profile_detail,
            no_profile_detail,
            eta,
            no_eta,
        } => {
            let use_stage_index =
                !no_stage_index && (stage_index || std::env::var_os("LCTR_DB").is_none());
            let show_profile_detail = profile_detail || !no_profile_detail;
            let staged_target = if use_stage_index {
                Some(staged_target_path_for_root(&root)?)
            } else {
                None
            };
            let (db, db_path) = if let Some(target) = &staged_target {
                let staging_path = staging_db_path_for_root(&root)?;
                delete_index_files(&staging_path)?;
                println!(
                    "staging index at {} before copying to {}",
                    staging_path.display(),
                    target.display()
                );
                (
                    Database::open_fresh_staged_scan(&staging_path)?,
                    staging_path,
                )
            } else {
                Database::open_for_scan_root(&root)?
            };
            let spinner = scan_spinner();
            spinner.set_message(format!(
                "preparing scan for {} using {}",
                root.display(),
                db_path.display()
            ));
            let show_eta = eta && !no_eta;
            let options = ScanOptions {
                backend,
                batch_size,
                writer_queue_batches,
                native_buffer_bytes: native_buffer_mb.saturating_mul(1024 * 1024),
                native_workers,
                native_output_batch_size,
                fresh_index: use_stage_index,
                estimate_totals: show_eta,
                ..Default::default()
            };
            let mut frame_index = 0usize;
            let mut last_draw = Instant::now() - Duration::from_secs(1);
            let stats = scan_root_with_progress(&db, root, options, |progress| {
                if last_draw.elapsed() >= Duration::from_millis(90)
                    || matches!(
                        progress.phase,
                        locator::scanner::ScanPhase::Discovering
                            | locator::scanner::ScanPhase::Optimizing
                            | locator::scanner::ScanPhase::Done
                    )
                {
                    spinner.set_message(render_scan_frame_with_eta(
                        progress,
                        ScanAnimation::frame(frame_index),
                        show_eta,
                    ));
                    frame_index = frame_index.wrapping_add(1);
                    last_draw = Instant::now();
                }
            })?;
            if let Some(target) = staged_target {
                db.checkpoint()?;
                drop(db);
                copy_finished_index(&db_path, &target)?;
                println!("staged index copied to {}", target.display());
            }
            spinner.finish_and_clear();
            println!(
                "indexed {} files, skipped {} entries, errors {}",
                stats.indexed_files, stats.skipped_entries, stats.error_entries
            );
            print_scan_profile(&stats, show_profile_detail);
            println!("next: run `lctr search` or `lctr find <query>` from this directory");
            println!("cleanup: run `lctr delete-index` from this directory to remove this index");
            if !stats.error_summaries.is_empty() {
                println!("error summary:");
                for (kind, summary) in &stats.error_summaries {
                    println!("  {}: {}", kind.label(), summary.count);
                    for sample in &summary.samples {
                        println!("    {}", sample.display());
                    }
                }
            }
        }
        Commands::ShellInit { shell } => {
            print_shell_init(&shell)?;
        }
        Commands::Status => {
            let db = Database::open_default_for_search()?;
            println!("{} indexed files", db.count_active()?);
        }
        Commands::Search { root } => {
            locator::tui::run_for_directory(root)?;
        }
        Commands::Find(args) => {
            let db = Database::open_default_for_search()?;
            let options = build_search_options(&args)?;
            let results = db.search_with_options(&options)?;
            for result in results {
                println!(
                    "{}\t{}\t{} bytes",
                    result.kind, result.path, result.size_bytes
                );
            }
        }
        Commands::Watch { root } => {
            locator::watch::watch_root(root)?;
        }
        Commands::Roots => {
            let db = Database::open_default()?;
            for root in db.roots()? {
                println!("{root}");
            }
        }
        Commands::RemoveRoot { root } => {
            let db = Database::open_default()?;
            let changed = db.remove_root(&root)?;
            println!("removed {changed} indexed files from {root}");
        }
        Commands::DeleteIndex { root } => match existing_db_path_for_working_dir(&root)? {
            Some(db_path) => {
                let removed = delete_index_files(&db_path)?;
                println!(
                    "deleted index {} (removed {} files)",
                    db_path.display(),
                    removed
                );
            }
            None => {
                println!("no index found for {}", root.display());
            }
        },
        Commands::Vacuum => {
            let db = Database::open_default()?;
            db.vacuum()?;
            println!("vacuum complete");
        }
    }

    Ok(())
}

fn print_scan_profile(stats: &locator::scanner::ScanStats, detail: bool) {
    let total = stats.profile.total.as_secs_f64();
    let walk = stats.profile.walk.as_secs_f64();
    let sqlite = stats.profile.sqlite_writes.as_secs_f64();
    let cleanup = stats.profile.cleanup.as_secs_f64();
    let discovery = stats.profile.discovery.as_secs_f64();
    let files_per_second = if total > 0.0 {
        stats.profile.indexed_files as f64 / total
    } else {
        0.0
    };
    let mb_per_second = if total > 0.0 {
        (stats.profile.indexed_bytes as f64 / 1_000_000.0) / total
    } else {
        0.0
    };

    println!(
        "scan profile: total {:.2}s, {:.1} files/s, {:.1} MB/s",
        total, files_per_second, mb_per_second
    );
    println!(
        "  discovery {:.2}s, walk+metadata {:.2}s, sqlite writes {:.2}s over {} batches, cleanup {:.2}s",
        discovery, walk, sqlite, stats.profile.batches, cleanup
    );
    if detail {
        println!("profile detail:");
        println!(
            "  record handling {:.2}s, writer wait {:.2}s",
            stats.profile.record_handling.as_secs_f64(),
            stats.profile.writer_wait.as_secs_f64()
        );
        println!(
            "  stale mark {:.2}s, fts rebuild {:.2}s, index rebuild {:.2}s, trigger recreate {:.2}s",
            stats.profile.stale_mark.as_secs_f64(),
            stats.profile.fts_rebuild.as_secs_f64(),
            stats.profile.index_rebuild.as_secs_f64(),
            stats.profile.trigger_recreate.as_secs_f64()
        );
        println!("native detail:");
        println!(
            "  dirs opened {}, dirs seen {}, files seen {}, entries seen {}, getattr calls {}, unknown type {}",
            stats.profile.native_dirs_opened,
            stats.profile.native_dirs_seen,
            stats.profile.native_files_seen,
            stats.profile.native_entries_seen,
            stats.profile.native_getattr_calls,
            stats.profile.native_unknown_type
        );
        println!(
            "  open dir {:.2}s, getattr {:.2}s, native parse {:.2}s, native emit {:.2}s, native queue wait {:.2}s",
            stats.profile.native_open_dir.as_secs_f64(),
            stats.profile.native_getattr.as_secs_f64(),
            stats.profile.native_parse.as_secs_f64(),
            stats.profile.native_emit.as_secs_f64(),
            stats.profile.native_queue_wait.as_secs_f64()
        );
    }
}

fn copy_finished_index(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    delete_index_files(target)?;
    fs::copy(source, target)?;
    delete_index_files(source)?;
    Ok(())
}

fn staged_target_path_for_root(root: &Path) -> Result<PathBuf> {
    let local_target = local_db_path_for_root(root)?;
    if let Some(parent) = local_target.parent() {
        match fs::create_dir_all(parent) {
            Ok(()) => return Ok(local_target),
            Err(error) if is_readonly_or_permission_error(&error) => {
                let fallback = fallback_db_path_for_root(root)?;
                println!(
                    "using fallback staged target {} because {} is not writable",
                    fallback.display(),
                    parent.display()
                );
                return Ok(fallback);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(local_target)
}

fn is_readonly_or_permission_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    )
}

fn print_shell_init(shell: &str) -> Result<()> {
    if shell != "zsh" {
        anyhow::bail!("unsupported shell '{shell}', expected zsh");
    }

    println!(
        r#"function lctr() {{
  if [[ "$1" == "scan" ]]; then
    local root=""
    local i=2
    while (( i <= $# )); do
      local arg="${{argv[$i]}}"
      case "$arg" in
        --backend)
          (( i += 2 ))
          ;;
        --backend=*)
          (( i += 1 ))
          ;;
        --no-eta)
          (( i += 1 ))
          ;;
        --batch-size|--writer-queue-batches|--native-buffer-mb|--native-workers|--native-output-batch-size)
          (( i += 2 ))
          ;;
        --batch-size=*|--writer-queue-batches=*|--native-buffer-mb=*|--native-workers=*|--native-output-batch-size=*)
          (( i += 1 ))
          ;;
        --stage-index|--profile-detail)
          (( i += 1 ))
          ;;
        --)
          root="${{argv[$(( i + 1 ))]}}"
          break
          ;;
        -*)
          (( i += 1 ))
          ;;
        *)
          root="$arg"
          break
          ;;
      esac
    done

    command lctr "$@"
    local exit_code=$?
    if [[ $exit_code -eq 0 ]]; then
      if [[ -n "$root" ]]; then
        cd -- "$root"
      else
        cd -- "$HOME"
      fi
    fi
    return $exit_code
  fi

  command lctr "$@"
}}"#
    );
    Ok(())
}

fn build_search_options(args: &FindArgs) -> Result<SearchOptions> {
    let mut filters = SearchFilters::new();
    if let Some(value) = &args.kind {
        filters = filters.with_kind(value)?;
    }
    if let Some(value) = &args.ext {
        filters = filters.with_exts(value)?;
    }
    if let Some(value) = &args.min_size {
        filters = filters.with_min_size(value)?;
    }
    if let Some(value) = &args.max_size {
        filters = filters.with_max_size(value)?;
    }
    if let Some(value) = &args.created_after {
        filters = filters.with_created_after(value)?;
    }
    if let Some(value) = &args.created_before {
        filters = filters.with_created_before(value)?;
    }
    if let Some(value) = &args.modified_after {
        filters = filters.with_modified_after(value)?;
    }
    if let Some(value) = &args.modified_before {
        filters = filters.with_modified_before(value)?;
    }
    if let Some(value) = &args.name {
        filters = filters.with_name(value)?;
    }
    Ok(SearchOptions::new(&args.query)
        .with_mode(args.mode)
        .with_sort(args.sort)
        .with_reverse(args.reverse)
        .with_limit(args.limit)
        .with_filters(filters))
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn scan_spinner() -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(80));
    spinner.set_style(
        ProgressStyle::with_template("{msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_strings(&[".", "o", "O", "o"]),
    );
    spinner
}
