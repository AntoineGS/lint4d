use lint4d::config::Config;
use lint4d::dcu::ProjectContext;
use lint4d::engine::{FileInfo, run_lint, run_lint_with_context};
use lint4d::output::json::format_json_output;
use lint4d::output::text::format_diagnostics;
use lint4d::rules::RuleRegistry;
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

fn lint_rule_diagnostics(
    fixture_path: &str,
    expected_rule: &str,
) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let registry = RuleRegistry::new();
    let overrides = registry
        .all_rules()
        .iter()
        .map(|rule| {
            let severity = if rule.meta().id == expected_rule {
                rule.meta().default_severity.to_string()
            } else {
                "off".to_string()
            };
            format!("\"{}\" = \"{severity}\"", rule.meta().id)
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        registry.get(expected_rule).is_some(),
        "unknown snapshot rule {expected_rule}"
    );
    let config = format!("version = 1\n[rules]\n{overrides}")
        .parse::<Config>()
        .unwrap();
    let project = ProjectContext::from_units(vec![]);
    run_lint_with_context(&file, &source, &config, Some(&project), &registry)
}

fn assert_rule_isolated(diagnostics: &[lint4d::engine::Diagnostic], expected_rule: &str) {
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.rule_id == expected_rule),
        "expected only {expected_rule} diagnostics, got {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.rule_id.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn snapshot_rule_empty_except() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/exception/bad_empty_except.pas",
        "empty-except",
    );
    assert_rule_isolated(&diagnostics, "empty-except");
    insta::assert_json_snapshot!("snapshot_rule_empty_except", diagnostics);
}

#[test]
fn snapshot_rule_bare_except() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/exception/bad_bare_except.pas",
        "bare-except",
    );
    assert_rule_isolated(&diagnostics, "bare-except");
    insta::assert_json_snapshot!("snapshot_rule_bare_except", diagnostics);
}

#[test]
fn snapshot_rule_resource_leak_unprotected() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/resource_leak/bad_unprotected.pas",
        "resource-leak-unprotected",
    );
    assert_rule_isolated(&diagnostics, "resource-leak-unprotected");
    insta::assert_json_snapshot!("snapshot_rule_resource_leak_unprotected", diagnostics);
}

#[test]
fn snapshot_rule_resource_leak_no_try() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/resource_leak/bad_no_try.pas",
        "resource-leak-no-try",
    );
    assert_rule_isolated(&diagnostics, "resource-leak-no-try");
    assert!(
        !diagnostics.is_empty(),
        "resource-leak-no-try must run with its required DCU context"
    );
    insta::assert_json_snapshot!("snapshot_rule_resource_leak_no_try", diagnostics);
}

#[test]
fn snapshot_rule_type_prefix() {
    let diagnostics =
        lint_rule_diagnostics("tests/fixtures/naming/bad_type_prefix.pas", "type-prefix");
    assert_rule_isolated(&diagnostics, "type-prefix");
    insta::assert_json_snapshot!("snapshot_rule_type_prefix", diagnostics);
}

#[test]
fn snapshot_rule_interface_prefix() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/naming/bad_interface_prefix.pas",
        "interface-prefix",
    );
    assert_rule_isolated(&diagnostics, "interface-prefix");
    insta::assert_json_snapshot!("snapshot_rule_interface_prefix", diagnostics);
}

#[test]
fn snapshot_rule_constant_naming() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/naming/bad_constant_naming.pas",
        "constant-naming",
    );
    assert_rule_isolated(&diagnostics, "constant-naming");
    insta::assert_json_snapshot!("snapshot_rule_constant_naming", diagnostics);
}

#[test]
fn snapshot_rule_local_variable_naming() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/naming/bad_local_variable_pascal.pas",
        "local-variable-naming",
    );
    assert_rule_isolated(&diagnostics, "local-variable-naming");
    insta::assert_json_snapshot!("snapshot_rule_local_variable_naming", diagnostics);
}

#[test]
fn snapshot_rule_with_statement() {
    let diagnostics =
        lint_rule_diagnostics("tests/fixtures/dangerous/bad_with.pas", "with-statement");
    assert_rule_isolated(&diagnostics, "with-statement");
    insta::assert_json_snapshot!("snapshot_rule_with_statement", diagnostics);
}

#[test]
fn snapshot_rule_inherited_order() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/inherited/bad_inherited_order.pas",
        "inherited-order",
    );
    assert_rule_isolated(&diagnostics, "inherited-order");
    insta::assert_json_snapshot!("snapshot_rule_inherited_order", diagnostics);
}

#[test]
fn snapshot_rule_inherited_missing() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/inherited/bad_inherited_missing.pas",
        "inherited-missing",
    );
    assert_rule_isolated(&diagnostics, "inherited-missing");
    insta::assert_json_snapshot!("snapshot_rule_inherited_missing", diagnostics);
}

#[test]
fn snapshot_rule_raise_in_destructor() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/exception/bad_raise_in_destructor.pas",
        "raise-in-destructor",
    );
    assert_rule_isolated(&diagnostics, "raise-in-destructor");
    insta::assert_json_snapshot!("snapshot_rule_raise_in_destructor", diagnostics);
}

#[test]
fn snapshot_rule_raise_in_destructor_clean() {
    let diagnostics = lint_rule_diagnostics(
        "tests/fixtures/exception/good_raise_in_destructor.pas",
        "raise-in-destructor",
    );
    assert_rule_isolated(&diagnostics, "raise-in-destructor");
    insta::assert_json_snapshot!("snapshot_rule_raise_in_destructor_clean", diagnostics);
}

// Factory snapshot tests removed — factory detection has been migrated to
// cfg-pascal and will be re-wired in a future task.
