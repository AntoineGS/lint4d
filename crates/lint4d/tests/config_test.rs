use lint4d::config::{Config, RuleSeverityOverride};
use lint4d::engine::Severity;
use std::fs;
use tempfile::TempDir;

#[test]
fn parses_minimal_config() {
    let toml = "version = 1\n";
    let config = toml.parse::<Config>().unwrap();
    assert_eq!(config.version, 1);
    assert!(config.paths.is_empty());
    assert!(config.exclude.is_empty());
}

#[test]
fn parses_full_config() {
    let toml = r#"
version = 1

[lint4d]
paths = ["src/", "lib/"]
exclude = ["src/generated/**"]

[rules]
resource-leak-unprotected = "error"
with-statement = "off"

[rules.naming]
constant_style = "UPPER_CASE"
"#;
    let config = toml.parse::<Config>().unwrap();
    assert_eq!(config.paths, vec!["src/", "lib/"]);
    assert_eq!(config.exclude, vec!["src/generated/**"]);

    let rl = config.rule_severity("resource-leak-unprotected");
    assert_eq!(rl, Some(RuleSeverityOverride::Severity(Severity::Error)));

    let ws = config.rule_severity("with-statement");
    assert_eq!(ws, Some(RuleSeverityOverride::Off));

    assert_eq!(config.constant_style(), "UPPER_CASE");
}

#[test]
fn default_constant_style_is_upper_case() {
    let config = "version = 1".parse::<Config>().unwrap();
    assert_eq!(config.constant_style(), "UPPER_CASE");
}

#[test]
fn unconfigured_rule_returns_none() {
    let config = "version = 1".parse::<Config>().unwrap();
    assert_eq!(config.rule_severity("empty-except"), None);
}

#[test]
fn discover_config_walks_up() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("src").join("units");
    fs::create_dir_all(&sub).unwrap();
    fs::write(dir.path().join(".lint4d.toml"), "version = 1\n").unwrap();

    let (config, root) = Config::discover(&sub).unwrap();
    assert_eq!(config.version, 1);
    assert_eq!(root, dir.path().to_path_buf());
}

#[test]
fn discover_config_uses_cwd_when_missing() {
    let dir = TempDir::new().unwrap();
    let (config, root) = Config::discover(dir.path()).unwrap();
    assert_eq!(config.version, 1);
    assert_eq!(root, dir.path().to_path_buf());
}

#[test]
fn config_parses_local_variable_style() {
    let toml = r#"
version = 1
[rules.naming]
local_variable_style = "PascalCase"
"#;
    let config = toml.parse::<Config>().unwrap();
    assert_eq!(config.local_variable_style(), "PascalCase");
}

#[test]
fn config_defaults_local_variable_style_to_camel_case() {
    let config = "version = 1".parse::<Config>().unwrap();
    assert_eq!(config.local_variable_style(), "PascalCase");
}

#[test]
fn config_parses_dcu_paths() {
    let config: Config = r#"
        version = 1
        [lint4d]
        dcu_paths = ["lib/Win64/Debug", "C:/Delphi/lib"]
    "#
    .parse()
    .unwrap();
    assert_eq!(config.dcu_paths(), &["lib/Win64/Debug", "C:/Delphi/lib"]);
}

#[test]
fn config_dcu_paths_default_empty() {
    let config = "version = 1".parse::<Config>().unwrap();
    assert!(config.dcu_paths().is_empty());
}

#[test]
fn config_parses_platform() {
    let config: Config = r#"
        version = 1
        [lint4d]
        platform = "Win64"
    "#
    .parse()
    .unwrap();
    assert_eq!(config.platform(), Some("Win64"));
}

#[test]
fn config_platform_defaults_to_none() {
    let config = "version = 1".parse::<Config>().unwrap();
    assert_eq!(config.platform(), None);
}

#[test]
fn config_parses_build_config() {
    let config: Config = r#"
        version = 1
        [lint4d]
        build_config = "Release"
    "#
    .parse()
    .unwrap();
    assert_eq!(config.build_config(), Some("Release"));
}

#[test]
fn config_parses_bds_path() {
    let config: Config = r#"
        version = 1
        [lint4d]
        bds_path = "C:/Embarcadero/Studio/23.0"
    "#
    .parse()
    .unwrap();
    assert_eq!(config.bds_path(), Some("C:/Embarcadero/Studio/23.0"));
}
