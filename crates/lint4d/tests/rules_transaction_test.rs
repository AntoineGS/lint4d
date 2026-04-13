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

fn count_rule(diagnostics: &[lint4d::engine::Diagnostic], rule_id: &str) -> usize {
    diagnostics.iter().filter(|d| d.rule_id == rule_id).count()
}

// ── Rule 1: transaction-no-rollback ──

#[test]
fn transaction_no_rollback_flags_missing_rollback() {
    let diagnostics = lint_fixture("tests/fixtures/transaction/bad_no_rollback.pas");
    assert!(
        count_rule(&diagnostics, "transaction-no-rollback") >= 1,
        "Expected transaction-no-rollback diagnostic, got: {:?}",
        diagnostics
    );
}

// ── Rule 2: transaction-ownership-violation ──

#[test]
fn transaction_ownership_violation_flags_unguarded_commit() {
    let diagnostics = lint_fixture("tests/fixtures/transaction/bad_ownership_violation.pas");
    assert!(
        count_rule(&diagnostics, "transaction-ownership-violation") >= 1,
        "Expected transaction-ownership-violation diagnostic, got: {:?}",
        diagnostics
    );
}

// ── Rule 3: transaction-no-commit ──

#[test]
fn transaction_no_commit_flags_missing_commit() {
    let diagnostics = lint_fixture("tests/fixtures/transaction/bad_no_commit.pas");
    assert!(
        count_rule(&diagnostics, "transaction-no-commit") >= 1,
        "Expected transaction-no-commit diagnostic, got: {:?}",
        diagnostics
    );
}

// ── Rule 4: transaction-nested-start ──

#[test]
fn transaction_nested_start_flags_double_start() {
    let diagnostics = lint_fixture("tests/fixtures/transaction/bad_nested_start.pas");
    assert!(
        count_rule(&diagnostics, "transaction-nested-start") >= 1,
        "Expected transaction-nested-start diagnostic, got: {:?}",
        diagnostics
    );
}

// ── Good patterns: no false positives ──

#[test]
fn good_guarded_transaction_produces_no_transaction_diagnostics() {
    let diagnostics = lint_fixture("tests/fixtures/transaction/good_guarded_transaction.pas");
    let trx_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id.starts_with("transaction-"))
        .collect();
    assert!(
        trx_diags.is_empty(),
        "Expected no transaction diagnostics for good pattern, got: {:?}",
        trx_diags
    );
}

#[test]
fn good_no_transaction_produces_no_transaction_diagnostics() {
    let diagnostics = lint_fixture("tests/fixtures/transaction/good_no_transaction.pas");
    let trx_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id.starts_with("transaction-"))
        .collect();
    assert!(
        trx_diags.is_empty(),
        "Expected no transaction diagnostics for non-transaction code, got: {:?}",
        trx_diags
    );
}
