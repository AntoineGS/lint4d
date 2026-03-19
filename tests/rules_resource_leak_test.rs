use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::fs;
use std::path::PathBuf;

fn lint_fixture(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = Config::from_str("version = 1").unwrap();
    run_lint(&file, &source, &config)
}

#[test]
fn resource_leak_unprotected_flags_code_before_try() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/bad_unprotected.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-unprotected")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 resource-leak-unprotected, got: {:?}",
        matches
    );
}

#[test]
fn resource_leak_unprotected_passes_when_protected() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_protected.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-unprotected")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no resource-leak-unprotected, got: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_missing_try_finally() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/bad_no_try.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 resource-leak-no-try, got: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_owned_objects() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_owned.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no resource-leak-no-try for owned objects, got: {:?}",
        matches
    );
}

#[test]
fn resource_leak_unprotected_flags_multi_constructor() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/bad_multi_constructor.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-unprotected")
        .collect();
    assert!(
        !matches.is_empty(),
        "Should flag code between constructors and try"
    );
}

#[test]
fn resource_leak_accepts_free_and_nil() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_free_and_nil.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id.starts_with("resource-leak"))
        .collect();
    assert!(
        matches.is_empty(),
        "FreeAndNil should be recognized as cleanup: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_constructor_field_assignments() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_constructor_field.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Field assignments inside constructors should not flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_factory_methods() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_factory_method.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Factory methods like CreateRunner should not flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_result_assignments() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_result_return.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Result assignments (function return values) should not flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_try_except_with_free() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_try_except_raise.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "try..except with Free+raise should be recognized as cleanup: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_field_in_any_method() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_field_in_method.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Field assignments in any method should not flag resource-leak-no-try: {:?}",
        matches
    );
}
