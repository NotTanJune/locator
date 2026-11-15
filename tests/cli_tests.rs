use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

#[test]
fn cli_prints_help() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(contains("lctr"));
}

#[test]
fn cli_prints_version() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(contains(format!("lctr {}", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn cli_exposes_update_command() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.args(["update", "--help"])
        .assert()
        .success()
        .stdout(contains("Usage:"))
        .stdout(contains("update"));
}

#[test]
fn cli_scans_and_finds_file_with_temp_database() {
    let root = tempdir().expect("root");
    let app = tempdir().expect("app support");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("scan")
        .arg(root.path())
        .assert()
        .success()
        .stdout(contains("1 files indexed"));

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("find")
        .arg("invoice")
        .arg("--type")
        .arg("pdf")
        .assert()
        .success()
        .stdout(contains("invoice.pdf"));
}

#[test]
fn scan_creates_directory_local_index_and_find_uses_current_directory() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success()
        .stdout(contains("1 files indexed"));

    assert!(root.path().join(".locator/index.db").exists());

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.current_dir(root.path())
        .arg("find")
        .arg("invoice")
        .assert()
        .success()
        .stdout(contains("invoice.pdf"));
}

#[test]
fn scan_without_root_indexes_current_directory() {
    let root = tempdir().expect("root");
    let root_path = root.path().canonicalize().expect("canonical root");
    let home = tempdir().expect("home");
    let data = tempdir().expect("data dir");
    std::fs::write(root_path.join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.current_dir(&root_path)
        .env("HOME", home.path())
        .env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .assert()
        .success()
        .stdout(contains("scan complete"))
        .stdout(contains("staging index at").not())
        .stdout(contains("staged index copied").not());

    assert!(root_path.join(".locator/index.db").exists());
    assert!(!home.path().join(".locator/index.db").exists());
}

#[test]
fn scan_defaults_to_high_throughput_staged_profile() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success()
        .stdout(contains("staging index at").not())
        .stdout(contains("1 files indexed"))
        .stdout(contains("index write"))
        .stdout(contains("filesystem scan counts"));
}

#[test]
fn scan_can_disable_staged_index_and_profile_detail() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.arg("scan")
        .arg(root.path())
        .arg("--no-stage-index")
        .arg("--no-profile-detail")
        .assert()
        .success()
        .stdout(contains("1 files indexed"))
        .stdout(contains("staging index at").not())
        .stdout(contains("profile detail:").not());
}

#[test]
fn staged_scan_creates_directory_local_index() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .arg("--stage-index")
        .assert()
        .success()
        .stdout(contains("scan complete"))
        .stdout(contains("staged index copied").not());

    assert!(root.path().join(".locator/index.db").exists());

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.current_dir(root.path())
        .arg("find")
        .arg("invoice")
        .assert()
        .success()
        .stdout(contains("invoice.pdf"));
}

#[test]
fn staged_rescan_preserves_open_history() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("report.pdf"), "fake").expect("write file");

    let scan = || {
        let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
        cmd.env("LCTR_DATA_DIR", data.path())
            .arg("scan")
            .arg(root.path())
            .arg("--stage-index")
            .assert()
            .success();
    };
    scan();

    let index_path = root.path().join(".locator/index.db");
    let file_path = root.path().join("report.pdf").to_string_lossy().to_string();
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS access (path TEXT PRIMARY KEY, open_count INTEGER NOT NULL DEFAULT 0, last_opened_at INTEGER);",
        )
        .expect("create access");
        conn.execute(
            "INSERT INTO access(path, open_count, last_opened_at) VALUES (?1, 5, 1700000000)",
            [&file_path],
        )
        .expect("insert access");
    }

    scan();

    let conn = rusqlite::Connection::open(&index_path).expect("reopen index");
    let open_count: i64 = conn
        .query_row(
            "SELECT open_count FROM access WHERE path = ?1",
            [&file_path],
            |row| row.get(0),
        )
        .expect("access row survives rescan");
    assert_eq!(open_count, 5);
}

#[test]
#[cfg(unix)]
fn staged_scan_readonly_root_falls_back_to_app_support_index() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");
    let original_mode = std::fs::metadata(root.path())
        .expect("root metadata")
        .permissions()
        .mode();
    let mut readonly = std::fs::metadata(root.path())
        .expect("root metadata")
        .permissions();
    readonly.set_mode(0o555);
    std::fs::set_permissions(root.path(), readonly).expect("make root readonly");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .arg("--stage-index")
        .assert()
        .success()
        .stdout(contains("using fallback index"))
        .stdout(contains("scan complete"))
        .stdout(contains("staged index copied").not());

    assert!(!root.path().join(".locator/index.db").exists());

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.env("LCTR_DATA_DIR", data.path())
        .current_dir(root.path())
        .arg("find")
        .arg("invoice")
        .assert()
        .success()
        .stdout(contains("invoice.pdf"));

    let mut writable = std::fs::metadata(root.path())
        .expect("root metadata")
        .permissions();
    writable.set_mode(original_mode);
    std::fs::set_permissions(root.path(), writable).expect("restore root permissions");
}

#[test]
#[cfg(unix)]
fn scan_readonly_root_falls_back_to_app_support_index() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");
    let original_mode = std::fs::metadata(root.path())
        .expect("root metadata")
        .permissions()
        .mode();
    let mut readonly = std::fs::metadata(root.path())
        .expect("root metadata")
        .permissions();
    readonly.set_mode(0o555);
    std::fs::set_permissions(root.path(), readonly).expect("make root readonly");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .arg("--no-eta")
        .assert()
        .success()
        .stdout(contains("1 files indexed"));

    assert!(!root.path().join(".locator/index.db").exists());

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.env("LCTR_DATA_DIR", data.path())
        .current_dir(root.path())
        .arg("find")
        .arg("invoice")
        .assert()
        .success()
        .stdout(contains("invoice.pdf"));

    let mut writable = std::fs::metadata(root.path())
        .expect("root metadata")
        .permissions();
    writable.set_mode(original_mode);
    std::fs::set_permissions(root.path(), writable).expect("restore root permissions");
}

#[test]
fn unfiltered_find_prefers_filename_matches_over_parent_path_matches() {
    let root = tempdir().expect("root");
    let app = tempdir().expect("app support");
    let parent = root.path().join("report");
    std::fs::create_dir(&parent).expect("create parent");
    std::fs::write(parent.join("notes.txt"), "fake").expect("write parent path match");
    std::fs::write(root.path().join("report.pdf"), "fake").expect("write filename match");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    let output = find
        .env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("find")
        .arg("report")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).expect("stdout is utf8");

    assert!(output
        .lines()
        .next()
        .is_some_and(|line| line.contains("report.pdf")));
}

#[test]
fn shell_init_zsh_prints_cd_wrapper_without_default_home_cd() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("shell-init")
        .arg("zsh")
        .assert()
        .success()
        .stdout(contains("function lctr()"))
        .stdout(contains("command lctr"))
        .stdout(contains("cd --"))
        .stdout(contains("--native-buffer-mb"))
        .stdout(contains("--native-workers"))
        .stdout(contains("--native-output-batch-size"))
        .stdout(contains("cd -- \"$root\""))
        .stdout(predicates::str::contains("cd -- \"$HOME\"").not())
        .stdout(predicates::str::contains("local status=").not());
}

#[test]
fn shell_init_supports_common_shells() {
    for shell in ["bash", "fish", "powershell"] {
        let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
        cmd.arg("shell-init")
            .arg(shell)
            .assert()
            .success()
            .stdout(contains("lctr"))
            .stdout(contains("scan"));
    }

    let mut powershell = Command::cargo_bin("lctr").expect("binary exists");
    powershell
        .arg("shell-init")
        .arg("powershell")
        .assert()
        .success()
        .stdout(predicates::str::contains("{{").not())
        .stdout(predicates::str::contains("}}").not());
}

#[test]
fn setup_shell_writes_profile_when_confirmed() {
    let root = tempdir().expect("root");
    let profile = root.path().join(".zshrc");

    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("setup-shell")
        .arg("--shell")
        .arg("zsh")
        .arg("--profile")
        .arg(&profile)
        .arg("--yes")
        .assert()
        .success()
        .stdout(contains("Added lctr shell integration"));

    let contents = std::fs::read_to_string(&profile).expect("profile contents");
    assert!(contents.contains("# >>> lctr shell integration >>>"));
    assert!(contents.contains("function lctr()"));
    assert!(contents.contains("cd -- \"$root\""));
}

#[test]
fn setup_shell_can_skip_profile_write() {
    let root = tempdir().expect("root");
    let profile = root.path().join(".bashrc");

    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("setup-shell")
        .arg("--shell")
        .arg("bash")
        .arg("--profile")
        .arg(&profile)
        .arg("--no")
        .assert()
        .success()
        .stdout(contains("Shell integration skipped."));

    assert!(!profile.exists());
}

#[test]
fn search_accepts_optional_root_argument() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("search")
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("[ROOT]"));
}

#[test]
fn scan_exposes_throughput_tuning_options() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.arg("scan")
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("--native-buffer-mb"))
        .stdout(contains("--native-workers"))
        .stdout(contains("--native-output-batch-size"))
        .stdout(contains("--batch-size").not())
        .stdout(contains("--writer-queue-batches").not())
        .stdout(contains("--stage-index").not())
        .stdout(contains("--no-stage-index").not())
        .stdout(contains("--profile-detail"))
        .stdout(contains("--no-profile-detail"))
        .stdout(contains("dirent"));
}

#[test]
fn scan_profile_detail_prints_expanded_timings() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .arg("--profile-detail")
        .assert()
        .success()
        .stdout(contains("index write"))
        .stdout(contains("filesystem scan counts"))
        .stdout(contains("name file"))
        .stdout(contains("metadata reads"))
        .stdout(contains("profile detail:").not())
        .stdout(contains("native detail:").not())
        .stdout(contains("getattr calls").not());
}

#[test]
fn delete_index_removes_current_directory_index() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    let db_path = root.path().join(".locator/index.db");
    assert!(db_path.exists());

    let mut delete = Command::cargo_bin("lctr").expect("binary exists");
    delete
        .current_dir(root.path())
        .arg("delete-index")
        .assert()
        .success()
        .stdout(contains("deleted index"));

    assert!(!db_path.exists());
}

#[test]
fn delete_index_removes_target_directory_index() {
    let root = tempdir().expect("root");
    let other = tempdir().expect("other");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    let db_path = root.path().join(".locator/index.db");
    assert!(db_path.exists());

    let mut delete = Command::cargo_bin("lctr").expect("binary exists");
    delete
        .current_dir(other.path())
        .arg("delete-index")
        .arg(root.path())
        .assert()
        .success()
        .stdout(contains("deleted index"));

    assert!(!db_path.exists());
}

#[test]
fn delete_index_reports_unindexed_target_directory() {
    let root = tempdir().expect("root");

    let mut delete = Command::cargo_bin("lctr").expect("binary exists");
    delete
        .current_dir(root.path())
        .arg("delete-index")
        .arg(root.path())
        .assert()
        .success()
        .stdout(contains("no index found"));
}

#[test]
fn scan_output_mentions_search_and_delete_index_commands() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .arg("--no-eta")
        .assert()
        .success()
        .stdout(contains("lctr search"))
        .stdout(contains("lctr delete-index"));
}

// ── Plan 001: rescan regression ───────────────────────────────────────────────

#[test]
fn rescan_over_existing_index_succeeds_and_no_tmp_residue() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    // First scan — creates .locator/index.db via the staged swap path.
    let mut scan1 = Command::cargo_bin("lctr").expect("binary exists");
    scan1
        .current_dir(root.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    // Second scan — exercises copy_finished_index over an existing index.
    let mut scan2 = Command::cargo_bin("lctr").expect("binary exists");
    scan2
        .current_dir(root.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    // Results are still queryable after the rescan.
    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.current_dir(root.path())
        .arg("find")
        .arg("invoice")
        .assert()
        .success()
        .stdout(contains("invoice.pdf"));

    // No .tmp residue left behind.
    let locator_dir = root.path().join(".locator");
    if locator_dir.exists() {
        for entry in std::fs::read_dir(&locator_dir).expect("read .locator") {
            let entry = entry.expect("entry");
            let name = entry.file_name();
            assert!(
                !name.to_string_lossy().ends_with(".tmp"),
                "unexpected .tmp file left: {:?}",
                entry.path()
            );
        }
    }
}

// ── Plan 010: completions ─────────────────────────────────────────────────────

#[test]
fn completions_generates_zsh_script() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(contains("compdef"))
        .stdout(contains("lctr"));
}

#[test]
fn completions_generates_bash_script() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.args(["completions", "bash"])
        .assert()
        .success()
        .stdout(contains("lctr"));
}

#[test]
fn completions_rejects_unknown_shell() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.args(["completions", "nosuchshell"]).assert().failure();
}

// ── Plan 011: find --output ────────────────────────────────────────────────────

#[test]
fn find_json_output_is_valid_and_complete() {
    let root = tempdir().expect("root");
    let app = tempdir().expect("app");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    Command::cargo_bin("lctr")
        .expect("binary exists")
        .env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    let output = Command::cargo_bin("lctr")
        .expect("binary exists")
        .env("LCTR_DB", app.path().join("index.sqlite"))
        .args(["find", "invoice", "--output", "json"])
        .output()
        .expect("run find");

    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let arr = parsed.as_array().expect("JSON array");
    assert_eq!(arr.len(), 1);
    let obj = &arr[0];
    assert!(obj.get("path").is_some(), "missing 'path'");
    assert!(obj.get("name").is_some(), "missing 'name'");
    assert!(obj.get("kind").is_some(), "missing 'kind'");
    assert!(obj.get("size_bytes").is_some(), "missing 'size_bytes'");
    assert_eq!(obj["name"].as_str().unwrap(), "invoice.pdf");
}

#[test]
fn find_jsonl_output_one_object_per_line() {
    let root = tempdir().expect("root");
    let app = tempdir().expect("app");
    std::fs::write(root.path().join("alpha.pdf"), "fake").expect("write alpha");
    std::fs::write(root.path().join("beta.pdf"), "fake").expect("write beta");

    Command::cargo_bin("lctr")
        .expect("binary exists")
        .env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    let output = Command::cargo_bin("lctr")
        .expect("binary exists")
        .env("LCTR_DB", app.path().join("index.sqlite"))
        .args(["find", "pdf", "--output", "jsonl", "--limit", "10"])
        .output()
        .expect("run find");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 jsonl lines, got {}",
        lines.len()
    );
    for line in lines {
        serde_json::from_str::<serde_json::Value>(line).expect("each line should be valid JSON");
    }
}

#[test]
fn find_tsv_default_unchanged() {
    let root = tempdir().expect("root");
    let app = tempdir().expect("app");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    Command::cargo_bin("lctr")
        .expect("binary exists")
        .env("LCTR_DB", app.path().join("index.sqlite"))
        .arg("scan")
        .arg(root.path())
        .assert()
        .success();

    let mut find = Command::cargo_bin("lctr").expect("binary exists");
    find.env("LCTR_DB", app.path().join("index.sqlite"))
        .args(["find", "invoice"])
        .assert()
        .success()
        .stdout(contains("invoice.pdf"))
        .stdout(contains(" bytes"))
        .stdout(contains("\t"));
}

// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn scan_output_includes_profile_timing_summary() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .arg("scan")
        .arg(root.path())
        .assert()
        .success()
        .stdout(contains("scan complete"))
        .stdout(contains("timing breakdown"))
        .stdout(contains("walk+metadata"))
        .stdout(contains("name file"));
}

#[cfg(unix)]
#[test]
fn shell_wrapper_skips_depth_value_when_finding_scan_root() {
    let bin = tempdir().expect("bin");
    let root = tempdir().expect("root");
    let stub = bin.path().join("lctr");
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let init = Command::cargo_bin("lctr")
        .expect("binary exists")
        .args(["shell-init", "bash"])
        .output()
        .expect("shell-init");
    assert!(init.status.success());
    let script = bin.path().join("init.bash");
    std::fs::write(&script, &init.stdout).expect("write init");

    for depth in ["--depth deep", "--depth=deep"] {
        let program = format!(
            "source '{}'; lctr scan --em {depth} '{}'; pwd -P",
            script.display(),
            root.path().display()
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(program)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .expect("run bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let cwd = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            std::path::PathBuf::from(cwd.trim()),
            root.path().canonicalize().unwrap(),
            "{depth}"
        );
    }
}

#[test]
fn scan_accepts_the_old_tuning_flags_and_ignores_them() {
    let root = tempdir().expect("root");
    let data = tempdir().expect("data dir");
    std::fs::write(root.path().join("invoice.pdf"), "fake").expect("write file");

    let mut scan = Command::cargo_bin("lctr").expect("binary exists");
    scan.env("LCTR_DATA_DIR", data.path())
        .current_dir(data.path())
        .arg("scan")
        .arg(root.path())
        .arg("--batch-size")
        .arg("1000")
        .arg("--writer-queue-batches")
        .arg("4")
        .arg("--stage-index")
        .assert()
        .success()
        .stdout(contains("1 files indexed"));
}
