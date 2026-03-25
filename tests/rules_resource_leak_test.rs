use lint4d::config::Config;
use lint4d::dcu::{
    DcuPlatform, DcuUnit, DcuVersion, FieldInfo, MethodInfo, MethodKind, ParamInfo, ParamModifier,
    ProjectContext, TypeInfo, TypeKind, TypeRef, Visibility,
};
use lint4d::engine::{run_lint, run_lint_with_context, FileInfo};
use lint4d::rules::RuleRegistry;
use lint4d::source_context::SourceContext;
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
    let project = empty_project();
    let diagnostics =
        lint_fixture_with_context("tests/fixtures/resource_leak/bad_no_try.pas", &project);
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
    let project = empty_project();
    let diagnostics =
        lint_fixture_with_context("tests/fixtures/resource_leak/good_owned.pas", &project);
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
    let project = project_with_class_fields("TMyServer", &["FDatabase", "FAdapter", "FCache"]);
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_constructor_field.pas",
        &project,
    );
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
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_factory_method.pas",
        &project,
    );
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
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_result_return.pas",
        &project,
    );
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
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_try_except_raise.pas",
        &project,
    );
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
    let project = project_with_class_fields("TMyClass", &["FConnection"]);
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_field_in_method.pas",
        &project,
    );
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

#[test]
fn resource_leak_no_try_skips_interface_refcounted() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_interface_refcounted.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Interface-refcounted objects should not flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_no_refcount_object() {
    let system_unit = DcuUnit {
        name: "System".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: "TNoRefCountObject".to_string(),
            kind: TypeKind::Class,
            parent: Some(TypeRef::Resolved("TObject".to_string())),
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    let project = ProjectContext::from_units(vec![system_unit]);
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_no_refcount_object.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "TNoRefCountObject assigned to interface should still flag: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skipped_without_context() {
    let diagnostics = lint_fixture("tests/fixtures/resource_leak/bad_no_try.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "resource-leak-no-try should be skipped without DCU context, got: {:?}",
        matches
    );
}

/// Helper: lint a fixture with a synthetic `ProjectContext` providing DCU type info.
fn lint_fixture_with_context(
    fixture_path: &str,
    project: &ProjectContext,
) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    let registry = RuleRegistry::new();
    run_lint_with_context(&file, &source, &config, Some(project), None, &registry)
}

fn empty_project() -> ProjectContext {
    ProjectContext::from_units(vec![])
}

fn project_with_class_fields(class_name: &str, fields: &[&str]) -> ProjectContext {
    let unit = DcuUnit {
        name: class_name.to_lowercase(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: class_name.to_string(),
            kind: TypeKind::Class,
            parent: Some(TypeRef::Resolved("TObject".to_string())),
            fields: fields
                .iter()
                .map(|f| FieldInfo {
                    name: f.to_string(),
                    type_ref: TypeRef::Unresolved(0),
                    visibility: Visibility::Private,
                })
                .collect(),
            methods: vec![],
            interface_guid: None,
        }],
    };
    ProjectContext::from_units(vec![unit])
}

#[test]
fn resource_leak_no_try_dcu_flags_no_refcount_descendant() {
    // Simulate a DCU where TMyNoRefObj descends from TNoRefCountObject.
    let system_unit = DcuUnit {
        name: "System".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: "TNoRefCountObject".to_string(),
            kind: TypeKind::Class,
            parent: Some(TypeRef::Resolved("TObject".to_string())),
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    let app_unit = DcuUnit {
        name: "bad_no_refcount_object".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec!["System".to_string()],
        types: vec![TypeInfo {
            name: "TMyNoRefObj".to_string(),
            kind: TypeKind::Class,
            parent: Some(TypeRef::Resolved("TNoRefCountObject".to_string())),
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    let project = ProjectContext::from_units(vec![system_unit, app_unit]);

    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_no_refcount_object.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        !matches.is_empty(),
        "TNoRefCountObject descendants should still flag even with DCU context: {:?}",
        diagnostics
    );
}

#[test]
fn resource_leak_no_try_flags_fvar_in_standalone_proc() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_fvar_standalone.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "F-prefixed local var in standalone proc should be flagged: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_dcu_skips_true_interface() {
    // Simulate a DCU confirming IMyService is an interface.
    let unit = DcuUnit {
        name: "MyServiceIntf".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![TypeInfo {
            name: "IMyService".to_string(),
            kind: TypeKind::Interface,
            parent: None,
            fields: vec![],
            methods: vec![],
            interface_guid: None,
        }],
    };
    let project = ProjectContext::from_units(vec![unit]);

    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_interface_refcounted.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "DCU-confirmed interface types should not flag: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_non_owner_constructor_args() {
    let unit = DcuUnit {
        name: "MyParsers".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![
            TypeInfo {
                name: "TParserConfig".to_string(),
                kind: TypeKind::Class,
                parent: Some(TypeRef::Resolved("TObject".to_string())),
                fields: vec![],
                methods: vec![],
                interface_guid: None,
            },
            TypeInfo {
                name: "TMyParser".to_string(),
                kind: TypeKind::Class,
                parent: Some(TypeRef::Resolved("TObject".to_string())),
                fields: vec![],
                methods: vec![MethodInfo {
                    name: "Create".to_string(),
                    kind: MethodKind::Constructor,
                    params: vec![ParamInfo {
                        name: "AConfig".to_string(),
                        type_ref: TypeRef::Resolved("TParserConfig".to_string()),
                        modifier: ParamModifier::ByValue,
                    }],
                    return_type: None,
                }],
                interface_guid: None,
            },
        ],
    };
    let project = ProjectContext::from_units(vec![unit]);

    let source = b"unit test_owner;\nimplementation\nuses MyParsers;\nprocedure Foo;\nvar P: TMyParser;\nbegin\n  P := TMyParser.Create(SomeConfig);\n  P.Parse;\nend;\nend.";
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let registry = RuleRegistry::new();
    let diagnostics = run_lint_with_context(&file, source, &config, Some(&project), None, &registry);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        !matches.is_empty(),
        "TMyParser.Create(SomeConfig) with non-owner param should flag leak"
    );
}

#[test]
fn resource_leak_no_try_skips_immediate_free() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_free_after_create.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Immediate .Free after constructor should not flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_free_in_comment() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_free_in_comment.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Free in a comment should NOT count as cleanup: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_free_in_string() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_free_in_string.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Free in a string literal should NOT count as cleanup: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_result_when_raise_follows() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_result_may_leak.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Result := TFoo.Create followed by raise without try should flag: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_result_with_try_except() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/good_result_with_try_except.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Result with try..except protection should not flag: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_result_raise_after_try() {
    let project = empty_project();
    let diagnostics = lint_fixture_with_context(
        "tests/fixtures/resource_leak/bad_result_raise_after_try.pas",
        &project,
    );
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Result with raise after try..except should flag — protection must extend to end of function: {:?}",
        matches
    );
}

/// Lint a fixture file with a SourceContext built from the given files.
/// Passes empty_project() so ResourceLeakNoTryRule runs (it requires context).
fn lint_fixture_with_source_ctx(
    fixture_paths: &[&str],
    lint_path: &str,
) -> Vec<lint4d::engine::Diagnostic> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_data: Vec<(FileInfo, Vec<u8>)> = fixture_paths
        .iter()
        .map(|p| {
            let path = manifest.join(p);
            let source = fs::read(&path).unwrap();
            (FileInfo::new(PathBuf::from(p)), source)
        })
        .collect();
    let refs: Vec<(&FileInfo, &[u8])> = file_data.iter().map(|(f, s)| (f, s.as_slice())).collect();
    let source_ctx = SourceContext::build(&refs);

    let project = empty_project();
    let path = manifest.join(lint_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(lint_path));
    let config = "version = 1".parse::<Config>().unwrap();
    let registry = RuleRegistry::new();
    run_lint_with_context(&file, &source, &config, Some(&project), Some(&source_ctx), &registry)
}

#[test]
fn resource_leak_no_try_flags_factory_call() {
    let path = "tests/fixtures/resource_leak/bad_factory_no_try.pas";
    let diagnostics = lint_fixture_with_source_ctx(&[path], path);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Factory call without try..finally should flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_passes_factory_with_try() {
    let path = "tests/fixtures/resource_leak/good_factory_protected.pas";
    let diagnostics = lint_fixture_with_source_ctx(&[path], path);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Factory call with try..finally should not flag: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_skips_non_factory_function() {
    let path = "tests/fixtures/resource_leak/good_not_factory.pas";
    let diagnostics = lint_fixture_with_source_ctx(&[path], path);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert!(
        matches.is_empty(),
        "Non-factory function should not flag resource-leak-no-try: {:?}",
        matches
    );
}

#[test]
fn resource_leak_no_try_flags_indirect_factory() {
    let path = "tests/fixtures/resource_leak/bad_factory_indirect.pas";
    let diagnostics = lint_fixture_with_source_ctx(&[path], path);
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "resource-leak-no-try")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Indirect factory (factory calling factory) should flag: {:?}",
        matches
    );
}
