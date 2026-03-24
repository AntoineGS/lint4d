use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::path::PathBuf;

// ─── field-not-freed ──────────────────────────────────────────────────────────

#[test]
fn field_not_freed_flags_unfreed_field() {
    let source = std::fs::read("tests/fixtures/resource_leak/bad_field_not_freed.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "Should flag FLogger (FChild is freed): {:?}",
        hits
    );
    assert!(
        hits[0].message.contains("FLogger"),
        "Should mention FLogger: {}",
        hits[0].message
    );
}

#[test]
fn field_not_freed_passes_when_all_freed() {
    let source = std::fs::read("tests/fixtures/resource_leak/good_field_freed.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert!(
        hits.is_empty(),
        "No field-not-freed diagnostics expected: {:?}",
        hits
    );
}

#[test]
fn field_not_freed_skips_owner_managed() {
    let source = std::fs::read("tests/fixtures/resource_leak/good_field_owned.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert!(
        hits.is_empty(),
        "Owner-managed fields should not be flagged: {:?}",
        hits
    );
}

#[test]
fn field_not_freed_flags_no_destructor() {
    let source = std::fs::read("tests/fixtures/resource_leak/bad_no_destructor.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "Should flag FChild when no destructor exists: {:?}",
        hits
    );
}

// ─── field-reassign-leak ──────────────────────────────────────────────────────

#[test]
fn field_reassign_leak_flags_reassign_without_free() {
    let source = std::fs::read("tests/fixtures/resource_leak/bad_field_reassign.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "Should flag FChild reassignment in Reset: {:?}",
        hits
    );
}

#[test]
fn field_reassign_leak_passes_when_freed_first() {
    let source =
        std::fs::read("tests/fixtures/resource_leak/good_field_reassign_with_free.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak")
        .collect();
    assert!(
        hits.is_empty(),
        "Should not flag when freed before reassignment: {:?}",
        hits
    );
}

// ─── Helper ──────────────────────────────────────────────────────────────────

fn lint_fixture(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = std::fs::read(&path).unwrap();
    let file = FileInfo::new(std::path::PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    run_lint(&file, &source, &config)
}

// ─── field-not-freed: non-constructor methods ────────────────────────────────

#[test]
fn field_not_freed_flags_field_created_in_method() {
    let diagnostics =
        lint_fixture("tests/fixtures/resource_leak/bad_field_created_in_method_not_freed.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "Should flag FObj created in Initialize but not freed in destructor: {:?}",
        hits
    );
    assert!(
        hits[0].message.contains("FObj"),
        "Should mention FObj: {}",
        hits[0].message
    );
    assert!(
        hits[0].message.contains("Initialize"),
        "Should mention method name 'Initialize': {}",
        hits[0].message
    );
}

#[test]
fn field_not_freed_passes_field_created_in_method_and_freed() {
    let diagnostics =
        lint_fixture("tests/fixtures/resource_leak/good_field_created_in_method_freed.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert!(
        hits.is_empty(),
        "Should not flag FObj when destructor frees it: {:?}",
        hits
    );
}

#[test]
fn field_not_freed_flags_once_for_multiple_methods() {
    let diagnostics =
        lint_fixture("tests/fixtures/resource_leak/bad_field_created_in_multiple_methods.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "Should flag FObj exactly once (constructor priority): {:?}",
        hits
    );
    assert!(
        hits[0].message.contains("constructor"),
        "Should report constructor location: {}",
        hits[0].message
    );
}

#[test]
fn field_not_freed_no_false_positive_on_good_field_in_method() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_field_in_method.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed")
        .collect();
    assert!(
        hits.is_empty(),
        "good_field_in_method should not flag field-not-freed (destructor frees it): {:?}",
        hits
    );
}

// ─── field-reassign-leak: new behavior ───────────────────────────────────────

#[test]
fn field_reassign_leak_no_false_positive_first_assignment_in_method() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/good_field_in_method.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak")
        .collect();
    assert!(
        hits.is_empty(),
        "First-time field assignment in non-constructor should not flag reassign: {:?}",
        hits
    );
}

#[test]
fn field_reassign_leak_flags_same_method_sequential() {
    let diagnostics =
        lint_fixture("tests/fixtures/resource_leak/bad_reassign_same_method_sequential.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak")
        .collect();
    assert!(
        hits.len() >= 2,
        "Should flag both reassignments in DoStuff (cross-method + same-method): {:?}",
        hits
    );
}

#[test]
fn field_reassign_leak_flags_branch_after_sequential() {
    let diagnostics =
        lint_fixture("tests/fixtures/resource_leak/bad_reassign_in_branch_after_sequential.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak")
        .collect();
    assert!(
        hits.len() >= 2,
        "Should flag both: cross-method reassign + reassign inside if block: {:?}",
        hits
    );
}

#[test]
fn field_reassign_leak_skips_mutually_exclusive_branches() {
    let diagnostics =
        lint_fixture("tests/fixtures/resource_leak/good_reassign_mutually_exclusive.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak")
        .collect();
    assert!(
        hits.is_empty(),
        "Mutually exclusive if/else branches should not flag reassign: {:?}",
        hits
    );
}

// ─── Integration test: MainForm.pas ──────────────────────────────────────────

#[test]
fn mainform_fobj_no_reassign_leak() {
    let diagnostics = lint_fixture("tests/fixtures/projects/TestProject1/MainForm.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-reassign-leak" && d.message.contains("FObj"))
        .collect();
    assert!(
        hits.is_empty(),
        "FObj in FormCreate should NOT trigger field-reassign-leak: {:?}",
        hits
    );
}

#[test]
fn mainform_fobj_field_not_freed() {
    let diagnostics = lint_fixture("tests/fixtures/projects/TestProject1/MainForm.pas");
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "field-not-freed" && d.message.contains("FObj"))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "FObj in FormCreate should trigger field-not-freed: {:?}",
        hits
    );
}
