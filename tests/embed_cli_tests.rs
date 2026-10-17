use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

struct Env {
    root: TempDir,
    app: TempDir,
    data: TempDir,
    config: TempDir,
}

fn env_with_corpus() -> Env {
    let env = Env {
        root: tempfile::tempdir().expect("root"),
        app: tempfile::tempdir().expect("app"),
        data: tempfile::tempdir().expect("data"),
        config: tempfile::tempdir().expect("config"),
    };
    std::fs::write(
        env.root.path().join("budget.txt"),
        "quarterly budget marketing spend spreadsheet",
    )
    .expect("budget");
    std::fs::write(
        env.root.path().join("soup.md"),
        "tomato soup recipe with basil and garlic",
    )
    .expect("soup");
    env
}

impl Env {
    fn index(&self) -> PathBuf {
        self.app.path().join("index.sqlite")
    }

    fn sidecar(&self) -> PathBuf {
        self.app.path().join("index.embeddings.sqlite")
    }

    /// `lctr` with the fake embedder, temp data and config, and a temp index.
    fn lctr(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
        cmd.env("LCTR_DB", self.index())
            .env("LCTR_DATA_DIR", self.data.path())
            .env("LCTR_CONFIG_DIR", self.config.path())
            .env("LCTR_EMBED_FAKE", "1")
            .env("LCTR_NO_UPDATE_CHECK", "1")
            .args(args);
        cmd
    }

    fn scan_em(&self) -> Command {
        let root = self.root.path().to_string_lossy().to_string();
        self.lctr(&["scan", &root, "--em", "--no-update-check"])
    }
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[test]
fn scan_em_embeds_and_creates_the_sidecar_next_to_the_index() {
    let env = env_with_corpus();
    env.scan_em().assert().success().stdout(contains("chunks"));
    assert!(env.index().exists());
    assert!(
        env.sidecar().exists(),
        "missing {}",
        path_str(&env.sidecar())
    );
}

#[test]
fn embed_status_reports_no_embeddings_before_and_counts_after() {
    let env = env_with_corpus();
    env.lctr(&["scan", &path_str(env.root.path())])
        .assert()
        .success();
    env.lctr(&["embed", "status"])
        .assert()
        .success()
        .stdout(contains("no embeddings yet"));

    env.scan_em().assert().success();
    let output = env
        .lctr(&["embed", "status"])
        .output()
        .expect("run embed status");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(text.contains("chunks: "), "{text}");
    assert!(!text.contains("chunks: 0,"), "{text}");
    assert!(text.contains("embed_dims: 768"), "{text}");
}

#[test]
fn remove_model_without_yes_refuses_on_a_non_terminal() {
    let env = env_with_corpus();
    env.lctr(&["embed", "remove-model"])
        .assert()
        .failure()
        .stderr(contains("--yes"));
}

#[test]
fn scan_help_mentions_em() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    cmd.args(["scan", "--help"])
        .assert()
        .success()
        .stdout(contains("--em"));
}

#[test]
fn delete_index_also_removes_the_sidecar() {
    let env = env_with_corpus();
    env.scan_em().assert().success();
    assert!(env.sidecar().exists());

    env.lctr(&["delete-index"])
        .assert()
        .success()
        .stdout(contains("embeddings sidecar removed"));
    assert!(!env.index().exists());
    assert!(!env.sidecar().exists());
}

#[test]
fn config_embed_dims_round_trips_and_rejects_other_widths() {
    let env = env_with_corpus();
    env.lctr(&["config", "set", "embed_dims", "256"])
        .assert()
        .success();
    env.lctr(&["config", "get", "embed_dims"])
        .assert()
        .success()
        .stdout(contains("256"));
    env.lctr(&["config", "set", "embed_dims", "300"])
        .assert()
        .failure();
}

#[test]
fn find_semantic_jsonl_has_score_and_snippet() {
    let env = env_with_corpus();
    env.scan_em().assert().success();
    let output = env
        .lctr(&[
            "find",
            "tomato basil",
            "--mode",
            "semantic",
            "--output",
            "jsonl",
        ])
        .output()
        .expect("run find");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let first = stdout.lines().next().expect("at least one result");
    let value: serde_json::Value = serde_json::from_str(first).expect("json line");
    assert!(
        value["path"].as_str().unwrap_or("").ends_with("soup.md"),
        "{first}"
    );
    assert!(value["score"].is_number(), "{first}");
    assert!(value["snippet"].is_string(), "{first}");
}

#[test]
fn find_semantic_without_embeddings_fails_with_a_hint() {
    let env = env_with_corpus();
    env.lctr(&["scan", &path_str(env.root.path())])
        .assert()
        .success();
    env.lctr(&["find", "tomato basil", "--mode", "semantic"])
        .assert()
        .failure()
        .stderr(contains("--em"));
}

#[test]
fn scan_help_gives_the_depth_numbers_from_stage_ends() {
    let mut cmd = Command::cargo_bin("lctr").expect("binary exists");
    let output = cmd.args(["scan", "--help"]).output().expect("run help");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(text.contains("--depth"), "{text}");
    assert!(
        text.contains("quick (the first 8 chunks of each file, the default)"),
        "{text}"
    );
    assert!(
        text.contains("balanced (up to 32 chunks per file)"),
        "{text}"
    );
    assert!(text.contains("deep (up to 128)"), "{text}");
}

#[test]
fn default_scan_em_runs_quick_and_says_how_to_go_deeper() {
    let env = env_with_corpus();
    let output = env.scan_em().output().expect("run scan");
    assert!(output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(
            "depth: quick (first 8 chunks of each file). --depth balanced adds chunks 9 to 32 and --depth deep adds chunks up to 128."
        ),
        "{text}"
    );
}
