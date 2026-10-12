use lint4d::config::Config;
use lint4d::engine::{FileInfo, run_lint};
use std::fs;
use std::path::PathBuf;

fn lint_fixture(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    run_lint(&file, &source, &config)
}

#[test]
fn empty_except_flags_empty_handler() {
    let diagnostics = lint_fixture("tests/fixtures/exception/bad_empty_except.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "empty-except")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 empty-except diagnostic, got {}: {:?}",
        matches.len(),
        matches
    );
}

#[test]
fn empty_except_passes_with_handler() {
    let diagnostics = lint_fixture("tests/fixtures/exception/good_except_handler.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "empty-except")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no empty-except diagnostics, got: {:?}",
        matches
    );
}

#[test]
fn bare_except_flags_handler_without_on_clause() {
    let diagnostics = lint_fixture("tests/fixtures/exception/bad_bare_except.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "bare-except")
        .collect();
    assert_eq!(matches.len(), 1);
}

#[test]
fn bare_except_skips_handler_with_raise() {
    let diagnostics = lint_fixture("tests/fixtures/exception/good_bare_except_with_raise.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "bare-except")
        .collect();
    assert!(matches.is_empty());
}

#[test]
fn bare_except_passes_with_on_clause() {
    let diagnostics = lint_fixture("tests/fixtures/exception/good_except_handler.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "bare-except")
        .collect();
    assert!(matches.is_empty());
}

fn lint_source(source: &str) -> Vec<lint4d::engine::Diagnostic> {
    let file = FileInfo::new(PathBuf::from("x.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    run_lint(&file, source.as_bytes(), &config)
}

#[test]
fn raise_in_destructor_reports_bare_reraise_outside_try_except() {
    let source = "unit X;\ninterface\ntype\n  TFoo = class\n    destructor Destroy; override;\n  end;\nimplementation\ndestructor TFoo.Destroy;\nbegin\n  try\n    Cleanup;\n  finally\n    Release;\n  end;\n  raise;\nend;\nend.\n";
    let lines: Vec<_> = lint_source(source)
        .iter()
        .filter(|d| d.rule_id == "raise-in-destructor")
        .map(|d| (d.line, d.column))
        .collect();
    assert_eq!(lines, [(15, 3)]);
}

#[test]
fn raise_in_destructor_skips_bare_reraise_inside_except() {
    let source = "unit X;\ninterface\ntype\n  TFoo = class\n    destructor Destroy; override;\n  end;\nimplementation\ndestructor TFoo.Destroy;\nbegin\n  try\n    Cleanup;\n  except\n    Log;\n    raise;\n  end;\nend;\nend.\n";
    let matches: Vec<_> = lint_source(source)
        .into_iter()
        .filter(|d| d.rule_id == "raise-in-destructor")
        .collect();
    assert!(matches.is_empty(), "{matches:?}");
}
