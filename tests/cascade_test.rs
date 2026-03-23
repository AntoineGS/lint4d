use std::path::PathBuf;
use lint4d::discovery::resolve_dcu_dirs;

#[test]
fn cli_paths_take_highest_priority() {
    let cli = vec![PathBuf::from("cli/path")];
    let config = vec!["config/path".to_string()];
    let discovered = vec![PathBuf::from("discovered/path")];
    let result = resolve_dcu_dirs(&cli, &config, discovered);
    assert_eq!(result, vec![PathBuf::from("cli/path")]);
}

#[test]
fn config_paths_override_discovered() {
    let cli: Vec<PathBuf> = vec![];
    let config = vec!["config/path".to_string()];
    let discovered = vec![PathBuf::from("discovered/path")];
    let result = resolve_dcu_dirs(&cli, &config, discovered);
    assert_eq!(result, vec![PathBuf::from("config/path")]);
}

#[test]
fn discovered_paths_used_when_no_overrides() {
    let cli: Vec<PathBuf> = vec![];
    let config: Vec<String> = vec![];
    let discovered = vec![PathBuf::from("discovered/path")];
    let result = resolve_dcu_dirs(&cli, &config, discovered);
    assert_eq!(result, vec![PathBuf::from("discovered/path")]);
}

#[test]
fn empty_when_all_sources_empty() {
    let result = resolve_dcu_dirs(&[], &[], vec![]);
    assert!(result.is_empty());
}
