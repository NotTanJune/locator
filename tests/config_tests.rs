use locator::config::Config;

#[test]
fn set_get_and_validation() {
    let mut config = Config::default();

    config.set("icons", "true").expect("set icons");
    assert_eq!(config.get("icons").unwrap(), "true");

    config.set("theme", "nord").expect("set theme");
    assert_eq!(config.get("theme").unwrap(), "nord");

    config.set("backend", "parallel").expect("set backend");
    config.set("preview", "off").expect("set preview");
    assert_eq!(config.get("preview").unwrap(), "false");

    // Invalid values and keys are rejected.
    assert!(config.set("theme", "bogus").is_err());
    assert!(config.set("backend", "rocket").is_err());
    assert!(config.set("icons", "maybe").is_err());
    assert!(config.set("nonexistent", "x").is_err());
    assert!(config.get("nonexistent").is_err());

    // entries() lists every key.
    assert_eq!(config.entries().len(), locator::config::KEYS.len());
}

#[test]
fn embed_dims_set_get_and_validation() {
    let mut config = Config::default();
    assert_eq!(config.get("embed_dims").unwrap(), "768");

    config.set("embed_dims", "256").expect("set 256");
    assert_eq!(config.get("embed_dims").unwrap(), "256");
    assert_eq!(
        locator::config::key_choices("embed_dims"),
        vec!["128", "256", "512", "768"]
    );
    assert_eq!(
        locator::config::key_label("embed_dims"),
        "Embedding dimensions"
    );

    assert!(config.set("embed_dims", "300").is_err());
    assert!(config.set("embed_dims", "abc").is_err());
    assert_eq!(config.get("embed_dims").unwrap(), "256");
}

#[test]
fn embed_depth_set_get_validation_and_round_trip() {
    let mut config = Config::default();
    assert_eq!(config.get("embed_depth").unwrap(), "quick");
    config.set("embed_depth", "balanced").expect("set balanced");
    assert_eq!(config.get("embed_depth").unwrap(), "balanced");
    config
        .set("embed_depth", "QUICK")
        .expect("case-insensitive");
    assert_eq!(config.get("embed_depth").unwrap(), "quick");
    assert_eq!(
        locator::config::key_choices("embed_depth"),
        vec!["quick", "balanced", "deep"]
    );
    assert_eq!(locator::config::key_label("embed_depth"), "Embedding depth");
    assert!(!locator::config::key_description("embed_depth").is_empty());
    assert!(config.set("embed_depth", "shallow").is_err());
    assert_eq!(config.get("embed_depth").unwrap(), "quick");

    let text = toml::to_string(&config).expect("serialize");
    let back: Config = toml::from_str(&text).expect("parse");
    assert_eq!(back.embed_depth, "quick");

    // A config file written before this key existed keeps working and gets the default.
    let old: Config = toml::from_str("embed_dims = 256\n").expect("old file");
    assert_eq!(old.embed_depth, "quick");
    assert_eq!(old.embed_dims, 256);
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_config_dir<T>(toml: Option<&str>, f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    if let Some(text) = toml {
        std::fs::write(dir.path().join("config.toml"), text).expect("write config");
    }
    std::env::set_var("LCTR_CONFIG_DIR", dir.path());
    std::env::remove_var("LCTR_NO_UPDATE_CHECK");
    let out = f(dir.path());
    std::env::remove_var("LCTR_CONFIG_DIR");
    out
}

#[test]
fn invalid_value_only_resets_that_key() {
    with_config_dir(
        Some("icons = \"yes\"\ntheme = \"nord\"\nembed_dims = 256\n"),
        |_| {
            let (config, ignored) = Config::load_reporting().expect("load");
            assert_eq!(ignored, vec!["icons".to_string()]);
            assert_eq!(config.theme, "nord");
            assert_eq!(config.embed_dims, 256);
            assert!(config.icons); // default
            assert_eq!(Config::load().theme, "nord");
        },
    );
}

#[test]
fn invalid_toml_is_an_error_and_save_is_atomic() {
    with_config_dir(Some("theme = [unterminated"), |dir| {
        assert!(Config::load_reporting().is_err());
        assert_eq!(Config::load(), Config::default());
        // The file is untouched by loading.
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            "theme = [unterminated"
        );
        Config::default().save().expect("save");
        assert!(Config::load_reporting().is_ok());
        let leftovers: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file must be renamed away");
    });
}

#[test]
fn config_update_check_false_disables_update_check() {
    with_config_dir(Some("update_check = false\n"), |_| {
        assert!(locator::update_check::checks_disabled());
    });
    with_config_dir(None, |_| {
        assert!(!locator::update_check::checks_disabled());
    });
}

#[test]
fn update_check_marker_lives_in_config_dir() {
    with_config_dir(None, |dir| {
        locator::update_check::persist_disable();
        assert!(dir.join("update_check_disabled").exists());
    });
}
