use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::fs;
use std::path::PathBuf;

fn lint_fixture(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    run_lint(&file, &source, &config)
}

// ─── inherited-order ────────────────────────────────────────────────────────

#[test]
fn inherited_order_bad_ctor_bottom() {
    let diagnostics = lint_fixture("tests/fixtures/inherited/bad_inherited_order.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "inherited-order")
        .collect();
    // TOrderBadCtor.Create (inherited at bottom), TOrderBadDtor.Destroy (inherited at top),
    // TOrderBadCtorMiddle.Create (inherited in middle)
    assert_eq!(
        matches.len(),
        3,
        "Expected 3 inherited-order diagnostics, got {}: {:?}",
        matches.len(),
        matches
    );
}

#[test]
fn inherited_order_good_passes() {
    let diagnostics = lint_fixture("tests/fixtures/inherited/good_inherited.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "inherited-order")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no inherited-order diagnostics, got {:?}",
        matches
    );
}

#[test]
fn inherited_order_nested_triggers_order_not_missing() {
    let diagnostics = lint_fixture("tests/fixtures/inherited/bad_inherited_order_nested.pas");
    let order_matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "inherited-order")
        .collect();
    // TCtorInheritedInIf.Create (inherited inside if), TCtorInheritedInTry.Create (inherited inside try)
    assert_eq!(
        order_matches.len(),
        2,
        "Expected 2 inherited-order diagnostics for nested inherited, got {}: {:?}",
        order_matches.len(),
        order_matches
    );

    let missing_matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "inherited-missing")
        .collect();
    assert!(
        missing_matches.is_empty(),
        "Expected no inherited-missing diagnostics (inherited exists, just nested), got {:?}",
        missing_matches
    );
}
