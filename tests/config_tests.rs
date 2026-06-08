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
