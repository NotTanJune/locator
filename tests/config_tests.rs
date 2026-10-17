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
