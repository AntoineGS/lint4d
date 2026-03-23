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
