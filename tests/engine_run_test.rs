use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::path::PathBuf;

#[test]
fn engine_runs_on_valid_file_with_no_issues() {
    let source = b"unit Clean;\ninterface\nimplementation\nend.\n";
    let file = FileInfo::new(PathBuf::from("Clean.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(diagnostics.is_empty());
}

#[test]
fn engine_reports_parse_errors() {
    let source = b"unit Bad;\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Bad.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(diagnostics.iter().any(|d| d.rule_id == "parse-error"));
}

#[test]
fn engine_filters_suppressed_diagnostics() {
    let source = b"unit Test;\n// lint4d:ignore parse-error\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    // Parse error on line 3 should be suppressed by comment on line 2
    let parse_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error" && d.line == 3)
        .collect();
    assert!(
        parse_errors.is_empty(),
        "Parse error on line 3 should be suppressed, got: {:?}",
        parse_errors
    );
}
