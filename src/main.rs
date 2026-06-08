use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use locator::db::{
    delete_index_files, existing_db_path_for_working_dir, fallback_db_path_for_root,
    local_db_path_for_root, staging_db_path_for_root, Database,
};
use locator::query::{QueryMode, SearchFilters, SearchOptions, SortField};
use locator::scan_ui::{
    render_scan_frame_for_terminal, render_scan_frame_with_eta_and_root, render_scan_summary,
    render_scan_summary_for_terminal, ScanAnimation, ScanSummary,
};
use locator::scanner::{scan_root_with_progress, ScanBackend, ScanOptions, ScanProgress};

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
        #[arg(default_value_os_t = current_dir())]
        root: PathBuf,
        // ParallelWalk benchmarks fastest on large trees (jwalk's parallel
        // walker); Native/Dirent are available via --backend for comparison.
        // Unset: use the configured default (`lctr config set backend ...`).
        #[arg(long, value_enum)]
        backend: Option<ScanBackend>,
        #[arg(long, default_value_t = 500_000)]
        batch_size: usize,
        #[arg(long, default_value_t = 64)]
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
        #[arg(long)]
        no_update_check: bool,
    },
    ShellInit {
        shell: String,
    },
    SetupShell {
        #[arg(long, help = "Shell to configure: zsh, bash, fish, or powershell")]
        shell: Option<String>,
        #[arg(long, help = "Shell profile file to edit")]
        profile: Option<PathBuf>,
        #[arg(long, help = "Enable shell integration without prompting")]
        yes: bool,
        #[arg(long, help = "Skip shell integration without prompting")]
        no: bool,
    },
    Status,
    Search {
        #[arg(default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))]
        root: PathBuf,
        #[arg(long)]
        no_update_check: bool,
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
    /// View or change persistent settings (icons, theme, backend, preview)
    Config {
        #[command(subcommand)]
        action: Option<ConfigCommand>,
    },
}

#[derive(Debug, clap::Subcommand)]
enum ConfigCommand {
    /// Print all settings as plain text (for scripts)
    List,
    /// Print a single setting's value
    Get { key: String },
    /// Change a setting and save it
    Set { key: String, value: String },
    /// Print the config file path
    Path,
    /// Reset all settings to defaults
    Reset,
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
            no_update_check,
        } => {
            let use_stage_index =
                !no_stage_index && (stage_index || std::env::var_os("LCTR_DB").is_none());
            let show_profile_detail = profile_detail || !no_profile_detail;
            let staged_target = if use_stage_index {
                Some(staged_target_path_for_root(&root)?)
            } else {
                None
            };
            let (db, db_path) = if staged_target.is_some() {
                let staging_path = staging_db_path_for_root(&root)?;
                delete_index_files(&staging_path)?;
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
            let live_color = io::stderr().is_terminal();
            let options = ScanOptions {
                backend: backend.unwrap_or_else(config_default_backend),
                batch_size,
                writer_queue_batches,
                native_buffer_bytes: native_buffer_mb.saturating_mul(1024 * 1024),
                native_workers,
                native_output_batch_size,
                fresh_index: use_stage_index,
                estimate_totals: show_eta,
                ..Default::default()
            };
            let latest_progress = Arc::new(Mutex::new(None::<ScanProgress>));
            let renderer_running = Arc::new(AtomicBool::new(true));
            let renderer = spawn_scan_renderer(
                spinner.clone(),
                Arc::clone(&latest_progress),
                Arc::clone(&renderer_running),
                show_eta,
                root.clone(),
                live_color,
            );
            let scan_result = scan_root_with_progress(&db, &root, options, |progress| {
                if let Ok(mut latest) = latest_progress.lock() {
                    *latest = Some(progress.clone());
                }
            });
            renderer_running.store(false, Ordering::Relaxed);
            let _ = renderer.join();
            let stats = scan_result?;
            let final_index_path = staged_target.clone().unwrap_or_else(|| db_path.clone());
            let used_stage_index = staged_target.is_some();
            if let Some(target) = staged_target {
                db.checkpoint()?;
                drop(db);
                copy_finished_index(&db_path, &target)?;
            }
            spinner.finish_and_clear();
            let summary = ScanSummary {
                stats: &stats,
                root: &root,
                index_path: &final_index_path,
                staged: used_stage_index,
                detail: show_profile_detail,
            };
            if io::stdout().is_terminal() {
                println!("{}", render_scan_summary_for_terminal(&summary));
            } else {
                println!("{}", render_scan_summary(&summary));
            }
            if no_update_check {
                locator::update_check::persist_disable();
            }
            if let Some(s) = locator::update_check::check(no_update_check) {
                let line = format!(
                    "\u{2728} lctr {} available, run `{}`",
                    s.latest, s.update_cmd
                );
                println!("{}", line);
            }
        }
        Commands::ShellInit { shell } => {
            print_shell_init(&shell)?;
        }
        Commands::SetupShell {
            shell,
            profile,
            yes,
            no,
        } => {
            setup_shell_integration(shell.as_deref(), profile.as_deref(), yes, no)?;
        }
        Commands::Status => {
            let db = Database::open_default_for_search()?;
            println!("{} indexed files", db.count_active()?);
        }
        Commands::Search {
            root,
            no_update_check,
        } => {
            if no_update_check {
                locator::update_check::persist_disable();
            }
            locator::tui::run_for_directory(root, no_update_check)?;
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
        Commands::Config { action } => handle_config(action)?,
    }

    Ok(())
}

fn config_default_backend() -> ScanBackend {
    match locator::config::Config::load().backend.as_str() {
        "native" => ScanBackend::Native,
        "dirent" => ScanBackend::Dirent,
        "auto" => ScanBackend::Auto,
        _ => ScanBackend::ParallelWalk,
    }
}

fn handle_config(action: Option<ConfigCommand>) -> Result<()> {
    use locator::config::Config;
    match action {
        None => {
            // No subcommand: open the interactive settings editor.
            locator::config_ui::run()?;
        }
        Some(ConfigCommand::List) => {
            let config = Config::load();
            println!("# {}", Config::path()?.display());
            for (key, value) in config.entries() {
                println!("{key} = {value}");
            }
        }
        Some(ConfigCommand::Get { key }) => {
            println!("{}", Config::load().get(&key)?);
        }
        Some(ConfigCommand::Set { key, value }) => {
            let mut config = Config::load();
            config.set(&key, &value)?;
            config.save()?;
            println!("set {key} = {}", config.get(&key)?);
        }
        Some(ConfigCommand::Path) => {
            println!("{}", Config::path()?.display());
        }
        Some(ConfigCommand::Reset) => {
            let config = Config::default();
            config.save()?;
            println!("config reset to defaults");
        }
    }
    Ok(())
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
    println!("{}", shell_init_script(shell)?);
    Ok(())
}

fn shell_init_script(shell: &str) -> Result<String> {
    match shell {
        "zsh" | "bash" => posix_shell_init(shell),
        "fish" => fish_shell_init(),
        "powershell" | "pwsh" | "power-shell" => powershell_init(),
        _ => anyhow::bail!("unsupported shell '{shell}', expected zsh, bash, fish, or powershell"),
    }
}

fn posix_shell_init(shell: &str) -> Result<String> {
    let arg_count = if shell == "zsh" { "$#" } else { "${#args[@]}" };
    let arg_at = if shell == "zsh" {
        "${argv[$i]}"
    } else {
        "${args[$((i - 1))]}"
    };
    Ok(format!(
        r#"function lctr() {{
  if [[ "$1" == "scan" ]]; then
    local args=("$@")
    local root=""
    local i=2
    while (( i <= {arg_count} )); do
      local arg="{arg_at}"
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
        --eta|--no-eta|--stage-index|--no-stage-index|--profile-detail|--no-profile-detail)
          (( i += 1 ))
          ;;
        --)
          root={next_arg}
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
    if [[ $exit_code -eq 0 && -n "$root" ]]; then
      cd -- "$root"
    fi
    return $exit_code
  fi

  command lctr "$@"
}}"#,
        arg_count = arg_count,
        arg_at = arg_at,
        next_arg = if shell == "zsh" {
            "\"${argv[$(( i + 1 ))]}\""
        } else {
            "\"${args[$i]}\""
        },
    ))
}

fn fish_shell_init() -> Result<String> {
    Ok(r#"function lctr
    if test (count $argv) -gt 0; and test "$argv[1]" = "scan"
        set -l root ""
        set -l i 2
        while test $i -le (count $argv)
            set -l arg $argv[$i]
            switch $arg
                case --backend --batch-size --writer-queue-batches --native-buffer-mb --native-workers --native-output-batch-size
                    set i (math $i + 2)
                case '--backend=*' '--batch-size=*' '--writer-queue-batches=*' '--native-buffer-mb=*' '--native-workers=*' '--native-output-batch-size=*' --no-eta --eta --stage-index --no-stage-index --profile-detail --no-profile-detail
                    set i (math $i + 1)
                case --
                    set root $argv[(math $i + 1)]
                    break
                case '-*'
                    set i (math $i + 1)
                case '*'
                    set root $arg
                    break
            end
        end

        command lctr $argv
        set -l exit_code $status
        if test $exit_code -eq 0; and test -n "$root"
            cd -- "$root"
        end
        return $exit_code
    end

    command lctr $argv
end"#
    .to_string())
}

fn powershell_init() -> Result<String> {
    Ok(r#"function lctr {
    if ($args.Count -gt 0 -and $args[0] -eq "scan") {
        $root = $null
        $i = 1
        while ($i -lt $args.Count) {
            $arg = $args[$i]
            switch -Regex ($arg) {
                '^(--backend|--batch-size|--writer-queue-batches|--native-buffer-mb|--native-workers|--native-output-batch-size)$' {
                    $i += 2
                    continue
                }
                '^(--backend|--batch-size|--writer-queue-batches|--native-buffer-mb|--native-workers|--native-output-batch-size)=' {
                    $i += 1
                    continue
                }
                '^(--no-eta|--eta|--stage-index|--no-stage-index|--profile-detail|--no-profile-detail)$' {
                    $i += 1
                    continue
                }
                '^--$' {
                    if ($i + 1 -lt $args.Count) { $root = $args[$i + 1] }
                    break
                }
                '^-' {
                    $i += 1
                    continue
                }
                default {
                    $root = $arg
                    break
                }
            }
        }

        & lctr @args
        $exitCode = $LASTEXITCODE
        if ($exitCode -eq 0 -and $root) {
            Set-Location -LiteralPath $root
        }
        $global:LASTEXITCODE = $exitCode
        return
    }

    & lctr @args
}"#
    .to_string())
}

fn setup_shell_integration(
    shell_override: Option<&str>,
    profile_override: Option<&Path>,
    yes: bool,
    no: bool,
) -> Result<()> {
    let shell = shell_override
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("LCTR_SHELL").ok())
        .or_else(detect_current_shell)
        .ok_or_else(|| {
            anyhow::anyhow!("could not detect shell, pass --shell zsh|bash|fish|powershell")
        })?;
    let canonical_shell = normalize_shell_name(&shell)?;
    let profile = match profile_override {
        Some(path) => path.to_path_buf(),
        None => default_shell_profile(&canonical_shell)?,
    };
    let block = format!(
        "# >>> lctr shell integration >>>\n{}\n# <<< lctr shell integration <<<\n",
        shell_init_script(&canonical_shell)?
    );

    let choice = shell_setup_choice(yes, no)?;
    if !choice {
        println!("Shell integration skipped.");
        return Ok(());
    }

    if let Ok(existing) = fs::read_to_string(&profile) {
        if existing.contains("lctr shell-init") || existing.contains("lctr shell integration") {
            println!("Shell integration already present in {}", profile.display());
            return Ok(());
        }
    }

    if let Some(parent) = profile.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&profile)?;
    writeln!(file)?;
    write!(file, "{block}")?;

    println!("Added lctr shell integration to {}", profile.display());
    match canonical_shell.as_str() {
        "fish" => println!("Restart your shell or run: source {}", profile.display()),
        "powershell" => println!("Restart PowerShell or run: . {}", profile.display()),
        _ => println!("Restart your shell or run: . {}", profile.display()),
    }
    Ok(())
}

fn shell_setup_choice(yes: bool, no: bool) -> Result<bool> {
    if yes && no {
        anyhow::bail!("--yes and --no cannot be used together");
    }
    if yes {
        return Ok(true);
    }
    if no {
        return Ok(false);
    }

    match std::env::var("LCTR_INSTALL_SHELL_INTEGRATION")
        .ok()
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("1" | "yes" | "true") => return Ok(true),
        Some("0" | "no" | "false") => return Ok(false),
        Some(value) => anyhow::bail!(
            "invalid LCTR_INSTALL_SHELL_INTEGRATION value '{value}', expected yes or no"
        ),
        None => {}
    }

    if !io::stdin().is_terminal() {
        println!("Shell integration skipped in non-interactive mode.");
        println!("Run `lctr setup-shell` later to enable scan auto-cd.");
        return Ok(false);
    }

    print!("Add shell integration so `lctr scan <dir>` moves your shell into <dir>? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn detect_current_shell() -> Option<String> {
    if cfg!(windows) {
        return Some("powershell".to_string());
    }

    std::env::var("SHELL")
        .ok()
        .and_then(|shell| Path::new(&shell).file_name().map(|name| name.to_owned()))
        .and_then(|name| name.to_str().map(ToOwned::to_owned))
}

fn normalize_shell_name(shell: &str) -> Result<String> {
    match shell {
        "zsh" | "bash" | "fish" => Ok(shell.to_string()),
        "powershell" | "pwsh" | "power-shell" => Ok("powershell".to_string()),
        _ => anyhow::bail!("unsupported shell '{shell}', expected zsh, bash, fish, or powershell"),
    }
}

fn default_shell_profile(shell: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory, pass --profile"))?;

    match shell {
        "zsh" => Ok(std::env::var_os("ZDOTDIR")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join(".zshrc")),
        "bash" => Ok(home.join(".bashrc")),
        "fish" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("fish")
            .join("config.fish")),
        "powershell" => {
            if let Some(profile) = std::env::var_os("LCTR_POWERSHELL_PROFILE") {
                return Ok(PathBuf::from(profile));
            }
            if cfg!(windows) {
                Ok(home
                    .join("Documents")
                    .join("PowerShell")
                    .join("Microsoft.PowerShell_profile.ps1"))
            } else {
                Ok(home
                    .join(".config")
                    .join("powershell")
                    .join("Microsoft.PowerShell_profile.ps1"))
            }
        }
        _ => anyhow::bail!("unsupported shell '{shell}'"),
    }
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

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn spawn_scan_renderer(
    spinner: ProgressBar,
    latest_progress: Arc<Mutex<Option<ScanProgress>>>,
    running: Arc<AtomicBool>,
    show_eta: bool,
    root: PathBuf,
    color: bool,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut frame_index = 0usize;
        while running.load(Ordering::Relaxed) {
            draw_latest_scan_progress(
                &spinner,
                &latest_progress,
                frame_index,
                show_eta,
                &root,
                color,
            );
            frame_index = frame_index.wrapping_add(1);
            thread::sleep(Duration::from_millis(90));
        }
        draw_latest_scan_progress(
            &spinner,
            &latest_progress,
            frame_index,
            show_eta,
            &root,
            color,
        );
    })
}

fn draw_latest_scan_progress(
    spinner: &ProgressBar,
    latest_progress: &Arc<Mutex<Option<ScanProgress>>>,
    frame_index: usize,
    show_eta: bool,
    root: &Path,
    color: bool,
) {
    let progress = latest_progress
        .lock()
        .ok()
        .and_then(|latest| latest.clone());
    if let Some(progress) = progress {
        let frame = if color {
            render_scan_frame_for_terminal(
                &progress,
                ScanAnimation::frame(frame_index),
                show_eta,
                root,
            )
        } else {
            render_scan_frame_with_eta_and_root(
                &progress,
                ScanAnimation::frame(frame_index),
                show_eta,
                Some(root),
            )
        };
        spinner.set_message(frame);
    }
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
