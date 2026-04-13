use lint4d::config::Config;
use lint4d::dcu::{DcuPlatform, DcuUnit, DcuVersion, ProjectContext, TypeInfo, TypeKind};
use lint4d::engine::{FileInfo, run_lint_with_context};
use lint4d::rules::RuleRegistry;
use std::fs;
use std::path::PathBuf;

/// Helper: create a ProjectContext with TObject as a class type in System unit.
fn project_with_tobject() -> ProjectContext {
    let system_unit = DcuUnit {
        name: "System".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: "TObject".to_string(),
            kind: TypeKind::Class,
            parent: None,
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    ProjectContext::from_units(vec![system_unit])
}

/// Helper: lint a fixture with unchecked-nil enabled and a ProjectContext.
fn lint_nil_check(fixture_path: &str, project: &ProjectContext) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1\n[rules]\nunchecked-nil = \"warning\""
        .parse::<Config>()
        .unwrap();
    let registry = RuleRegistry::new();
    run_lint_with_context(&file, &source, &config, Some(project), &registry)
}

#[test]
fn unchecked_nil_flags_param_without_check() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/bad_param_no_check.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 unchecked-nil for parameter used without check, got: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_passes_with_not_nil_check() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/good_comparison.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert!(
        matches.is_empty(),
        "Should not flag when nil check exists: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_passes_with_raiseifnil() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/good_raiseifnil.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert!(
        matches.is_empty(),
        "Should not flag after RaiseIfNil: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_passes_with_early_exit_guard() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/good_early_exit.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert!(
        matches.is_empty(),
        "Should not flag after early exit guard: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_passes_with_constructor_assignment() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/good_constructor.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert!(
        matches.is_empty(),
        "Should not flag after constructor assignment: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_flags_local_without_check() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/bad_local_no_check.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 unchecked-nil for local used without check, got: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_flags_unchecked_branch() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/bad_branch_unsafe.pas", &project);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 unchecked-nil for use on unchecked branch, got: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_skipped_when_not_enabled() {
    let project = project_with_tobject();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/nil_check/bad_param_no_check.pas");
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(
        "tests/fixtures/nil_check/bad_param_no_check.pas",
    ));
    let config = "version = 1".parse::<Config>().unwrap();
    let registry = RuleRegistry::new();
    let diagnostics = run_lint_with_context(&file, &source, &config, Some(&project), &registry);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert!(
        matches.is_empty(),
        "Rule should not fire when not explicitly enabled: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_passes_safe_function_return() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check(
        "tests/fixtures/nil_check/good_function_return_safe.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert!(
        matches.is_empty(),
        "Function that always returns non-nil should not flag: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_flags_nil_function_return() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check(
        "tests/fixtures/nil_check/bad_function_return_nil.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Function that can return nil should flag: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_flags_use_after_freeandnil() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check(
        "tests/fixtures/nil_check/bad_use_after_freeandnil.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Should flag use after FreeAndNil resets state: {:?}",
        matches
    );
}

#[test]
fn unchecked_nil_diagnostic_format() {
    let project = project_with_tobject();
    let diagnostics = lint_nil_check("tests/fixtures/nil_check/bad_param_no_check.pas", &project);
    let diag = diagnostics
        .iter()
        .find(|d| d.rule_id == "unchecked-nil")
        .expect("Should have an unchecked-nil diagnostic");

    assert!(
        diag.message.contains("Parameter"),
        "Message should mention 'Parameter': {}",
        diag.message
    );
    assert!(
        diag.message.contains("AObj"),
        "Message should mention variable name: {}",
        diag.message
    );
    assert!(diag.help.is_some(), "Should have help text");
    assert!(
        diag.help.as_ref().unwrap().contains("RaiseIfNil"),
        "Help should suggest RaiseIfNil: {}",
        diag.help.as_ref().unwrap()
    );
    assert!(diag.scope.is_some(), "Should have enclosing scope");
}

fn project_with_interface() -> ProjectContext {
    let system_unit = DcuUnit {
        name: "System".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: "TObject".to_string(),
            kind: TypeKind::Class,
            parent: None,
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    let intf_unit = DcuUnit {
        name: "MyIntf".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: "IMyInterface".to_string(),
            kind: TypeKind::Interface,
            parent: None,
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    ProjectContext::from_units(vec![system_unit, intf_unit])
}

#[test]
fn unchecked_nil_flags_interface_without_check() {
    let project = project_with_interface();
    let diagnostics = lint_nil_check(
        "tests/fixtures/nil_check/bad_interface_no_check.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "unchecked-nil")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 unchecked-nil for interface used without check, got: {:?}",
        matches
    );
}
