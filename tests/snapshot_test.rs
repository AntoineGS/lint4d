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

fn lint_diagnostics(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    run_lint(&file, &source, &config)
}

#[test]
fn snapshot_rule_empty_except() {
    let diagnostics = lint_diagnostics("tests/fixtures/exception/bad_empty_except.pas");
    insta::assert_json_snapshot!("snapshot_rule_empty_except", diagnostics);
}

#[test]
fn snapshot_rule_bare_except() {
    let diagnostics = lint_diagnostics("tests/fixtures/exception/bad_bare_except.pas");
    insta::assert_json_snapshot!("snapshot_rule_bare_except", diagnostics);
}

#[test]
fn snapshot_rule_resource_leak_unprotected() {
    let diagnostics = lint_diagnostics("tests/fixtures/resource_leak/bad_unprotected.pas");
    insta::assert_json_snapshot!("snapshot_rule_resource_leak_unprotected", diagnostics);
}

#[test]
fn snapshot_rule_resource_leak_no_try() {
    let diagnostics = lint_diagnostics("tests/fixtures/resource_leak/bad_no_try.pas");
    insta::assert_json_snapshot!("snapshot_rule_resource_leak_no_try", diagnostics);
}

#[test]
fn snapshot_rule_type_prefix() {
    let diagnostics = lint_diagnostics("tests/fixtures/naming/bad_type_prefix.pas");
    insta::assert_json_snapshot!("snapshot_rule_type_prefix", diagnostics);
}

#[test]
fn snapshot_rule_interface_prefix() {
    let diagnostics = lint_diagnostics("tests/fixtures/naming/bad_interface_prefix.pas");
    insta::assert_json_snapshot!("snapshot_rule_interface_prefix", diagnostics);
}

#[test]
fn snapshot_rule_constant_naming() {
    let diagnostics = lint_diagnostics("tests/fixtures/naming/bad_constant_naming.pas");
    insta::assert_json_snapshot!("snapshot_rule_constant_naming", diagnostics);
}

#[test]
fn snapshot_rule_local_variable_naming() {
    let diagnostics = lint_diagnostics("tests/fixtures/naming/bad_local_variable_camel.pas");
    insta::assert_json_snapshot!("snapshot_rule_local_variable_naming", diagnostics);
}

#[test]
fn snapshot_rule_with_statement() {
    let diagnostics = lint_diagnostics("tests/fixtures/dangerous/bad_with.pas");
    insta::assert_json_snapshot!("snapshot_rule_with_statement", diagnostics);
}

#[test]
fn snapshot_rule_inherited_order() {
    let diagnostics = lint_diagnostics("tests/fixtures/inherited/bad_inherited_order.pas");
    insta::assert_json_snapshot!("snapshot_rule_inherited_order", diagnostics);
}

#[test]
fn snapshot_rule_inherited_missing() {
    let diagnostics = lint_diagnostics("tests/fixtures/inherited/bad_inherited_missing.pas");
    insta::assert_json_snapshot!("snapshot_rule_inherited_missing", diagnostics);
}
