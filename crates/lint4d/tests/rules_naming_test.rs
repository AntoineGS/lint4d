use lint4d::config::Config;
use lint4d::engine::{FileInfo, run_lint};
use lint4d::rules::naming::{to_camel_case, to_pascal_case, to_upper_snake_case};
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
fn type_prefix_flags_missing_t() {
    let diagnostics = lint_fixture("tests/fixtures/naming/bad_type_prefix.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "type-prefix")
        .collect();
    assert_eq!(
        matches.len(),
        2,
        "Expected 2 type-prefix diagnostics for MyClass and BadRecord, got: {:?}",
        matches
    );
}

#[test]
fn interface_prefix_flags_missing_i() {
    let diagnostics = lint_fixture("tests/fixtures/naming/bad_interface_prefix.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "interface-prefix")
        .collect();
    assert_eq!(matches.len(), 1);
}

#[test]
fn naming_passes_with_correct_prefixes() {
    let diagnostics = lint_fixture("tests/fixtures/naming/good_naming.pas");
    let naming: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "type-prefix" || d.rule_id == "interface-prefix")
        .collect();
    assert!(
        naming.is_empty(),
        "Expected no naming diagnostics, got: {:?}",
        naming
    );
}

#[test]
fn constant_naming_flags_lowercase_constants() {
    let diagnostics = lint_fixture("tests/fixtures/naming/bad_constant_naming.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "constant-naming")
        .collect();
    assert_eq!(
        matches.len(),
        2,
        "Expected 2 constant-naming diagnostics, got: {:?}",
        matches
    );
}

#[test]
fn constant_naming_passes_upper_case() {
    let diagnostics = lint_fixture("tests/fixtures/naming/good_constant_naming.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "constant-naming")
        .collect();
    assert!(
        matches.is_empty(),
        "Expected no constant-naming diagnostics, got: {:?}",
        matches
    );
}

#[test]
fn naming_rules_skipped_for_dpr_files() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/naming/bad_type_prefix.pas");
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from("Project.dpr"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let naming: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "type-prefix" || d.rule_id == "interface-prefix")
        .collect();
    assert!(
        naming.is_empty(),
        "Naming rules should be skipped for .dpr files"
    );
}

#[test]
fn to_camel_case_basic() {
    assert_eq!(to_camel_case("MyVar"), "myVar");
    assert_eq!(to_camel_case("AnotherName"), "anotherName");
}

#[test]
fn to_camel_case_preserves_leading_underscores() {
    assert_eq!(to_camel_case("_Count"), "_count");
    assert_eq!(to_camel_case("__Foo"), "__foo");
}

#[test]
fn to_camel_case_already_camel() {
    assert_eq!(to_camel_case("myVar"), "myVar");
}

#[test]
fn to_camel_case_single_char() {
    assert_eq!(to_camel_case("X"), "x");
}

#[test]
fn to_pascal_case_basic() {
    assert_eq!(to_pascal_case("myVar"), "MyVar");
    assert_eq!(to_pascal_case("httpPort"), "HttpPort");
}

#[test]
fn to_pascal_case_underscore_separated() {
    assert_eq!(to_pascal_case("my_const"), "MyConst");
    assert_eq!(to_pascal_case("HTTP_PORT"), "HttpPort");
    assert_eq!(to_pascal_case("foo_bar_baz"), "FooBarBaz");
}

#[test]
fn to_pascal_case_preserves_leading_underscores() {
    assert_eq!(to_pascal_case("_myVar"), "_MyVar");
    assert_eq!(to_pascal_case("_my_const"), "_MyConst");
}

#[test]
fn to_pascal_case_already_pascal() {
    assert_eq!(to_pascal_case("MyVar"), "MyVar");
}

#[test]
fn to_upper_snake_case_camel() {
    assert_eq!(to_upper_snake_case("httpPort"), "HTTP_PORT");
    assert_eq!(to_upper_snake_case("maxSize"), "MAX_SIZE");
}

#[test]
fn to_upper_snake_case_acronym() {
    assert_eq!(to_upper_snake_case("HTTPPort"), "HTTP_PORT");
}

#[test]
fn local_var_naming_inside_ifdef() {
    let diagnostics = lint_fixture("tests/fixtures/naming/local_var_in_ifdef.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "local-variable-naming")
        .collect();
    // bad_var inside {$IFDEF} should still be caught
    assert!(
        !matches.is_empty(),
        "Expected local-variable-naming diagnostic for bad_var inside IFDEF, got none"
    );
}
