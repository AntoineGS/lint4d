use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use lint4d::output::json::format_json_output;
use lint4d::output::text::format_diagnostics;
use std::fs;
use std::path::PathBuf;

fn lint_and_format_text(fixture_path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    format_diagnostics(fixture_path, &source, &diagnostics, false)
}

fn lint_and_format_json(fixture_path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    format_json_output(&[(fixture_path.to_string(), diagnostics)])
}

#[test]
fn snapshot_text_output() {
    let output = lint_and_format_text("tests/fixtures/exception/bad_empty_except.pas");
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_json_output() {
    let output = lint_and_format_json("tests/fixtures/exception/bad_empty_except.pas");
    insta::assert_snapshot!(output);
}
