use std::fs;
use std::path::PathBuf;

fn format_source(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
}

fn idempotency_check(source: &str) {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let first =
        fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("first failed");
    let second =
        fmt4d::formatter::format_source(first.as_bytes(), &info, &config).expect("second failed");
    assert_eq!(first, second, "formatting is not idempotent");
}

fn roundtrip_ast_check(source: &str) {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let formatted =
        fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("format failed");

    let (tree_before, _) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (tree_after, _) = pascal_core::parser::parse_file(&info, formatted.as_bytes())
        .expect("parse formatted failed");

    assert_eq!(
        tree_before.root_node().kind(),
        tree_after.root_node().kind(),
        "AST root kind changed"
    );
}

// ── Spacing fixture tests ────────────────────────────────────────

#[test]
fn spacing_fixture_matches_expected() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn spacing_is_idempotent() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas").unwrap();
    let formatted = format_source(&input);
    idempotency_check(&formatted);
}

#[test]
fn spacing_roundtrip_safe() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas").unwrap();
    roundtrip_ast_check(&input);
}

#[test]
fn spacing_adds_space_around_assign() {
    let result = format_source(&"unit T;\ninterface\nimplementation\nprocedure P;\nvar\n  x:Integer;\nbegin\n  x:=1;\nend;\nend.\n");
    assert!(result.contains("x := 1"), "should add spaces around :=");
}

#[test]
fn spacing_adds_space_after_colon() {
    let result = format_source(
        &"unit T;\ninterface\nimplementation\nprocedure P;\nvar\n  x:Integer;\nbegin\nend;\nend.\n",
    );
    assert!(
        result.contains("x: Integer"),
        "should add space after colon in var declaration"
    );
}

#[test]
fn spacing_adds_space_after_comma() {
    let result = format_source(
        &"unit T;\ninterface\nimplementation\nprocedure P;\nbegin\n  Foo(a,b,c);\nend;\nend.\n",
    );
    assert!(
        result.contains("Foo(a, b, c)"),
        "should add spaces after commas"
    );
}

// ── Uses fixture tests ───────────────────────────────────────────

#[test]
fn uses_fixture_matches_expected() {
    let input = fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/uses/uses_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn uses_is_idempotent() {
    let input = fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas").unwrap();
    let formatted = format_source(&input);
    idempotency_check(&formatted);
}

#[test]
fn uses_roundtrip_safe() {
    let input = fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas").unwrap();
    roundtrip_ast_check(&input);
}

#[test]
fn uses_groups_system_units() {
    let result = format_source(&"unit T;\ninterface\nuses\n  Forms, System.SysUtils, System.Classes;\nimplementation\nend.\n");
    // All are core units — should be in one group, alphabetically sorted
    assert!(result.contains("  Forms,\n  System.Classes,\n  System.SysUtils;\n"));
}

#[test]
fn uses_sorts_within_groups() {
    let result = format_source(
        &"unit T;\ninterface\nuses\n  System.SysUtils, System.Classes;\nimplementation\nend.\n",
    );
    // Both are core units, sorted alphabetically
    assert!(result.contains("  System.Classes,\n  System.SysUtils;\n"));
}

// ── Comments fixture tests ───────────────────────────────────────

#[test]
fn comments_fixture_matches_expected() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn comments_is_idempotent() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let formatted = format_source(&input);
    idempotency_check(&formatted);
}

#[test]
fn comments_roundtrip_safe() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    roundtrip_ast_check(&input);
}

#[test]
fn comments_preserves_inline_comment() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let result = format_source(&input);
    assert!(
        result.contains("// assign x"),
        "inline comment should be preserved"
    );
}

#[test]
fn comments_preserves_leading_comment() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let result = format_source(&input);
    assert!(
        result.contains("// This is a header comment"),
        "header comment should be preserved"
    );
    assert!(
        result.contains("// before y"),
        "comment before y should be preserved"
    );
}

// ── External config integration tests ────────────────────────────

#[test]
fn uses_groups_with_external_prefixes() {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.uses.external_prefixes = vec!["Spring".to_string()];

    let source = "unit T;\ninterface\nuses\n  MyUnit, Spring.Container, System.SysUtils, Spring.Collections;\nimplementation\nend.\n";
    let result = fmt4d::formatter::format_source(source.as_bytes(), &info, &config).unwrap();

    // Core section
    assert!(
        result.contains("  System.SysUtils,\n"),
        "Core unit should be present. Got:\n{}",
        result
    );
    // External section (blank line before)
    assert!(
        result.contains("\n\n  Spring.Collections,\n  Spring.Container,\n"),
        "External section should be present. Got:\n{}",
        result
    );
    // Project section (blank line before)
    assert!(
        result.contains("\n\n  MyUnit;\n"),
        "Project section should be present. Got:\n{}",
        result
    );
}

#[test]
fn uses_groups_with_external_paths() {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let vendor = dir.path().join("vendor");
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::write(vendor.join("SuperObject.pas"), "unit SuperObject;").unwrap();

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.uses.external_paths = vec!["vendor".to_string()];
    config.project_root = Some(dir.path().to_path_buf());

    let source =
        "unit T;\ninterface\nuses\n  MyUnit, SuperObject, System.SysUtils;\nimplementation\nend.\n";
    let result = fmt4d::formatter::format_source(source.as_bytes(), &info, &config).unwrap();

    // Core
    assert!(
        result.contains("  System.SysUtils,\n"),
        "Core unit missing. Got:\n{}",
        result
    );
    // External (blank line before)
    assert!(
        result.contains("\n\n  SuperObject,\n"),
        "External unit missing. Got:\n{}",
        result
    );
    // Project (blank line before)
    assert!(
        result.contains("\n\n  MyUnit;\n"),
        "Project unit missing. Got:\n{}",
        result
    );
}

// ── Cross-cutting integration tests ──────────────────────────────

#[test]
fn output_is_non_empty_and_valid() {
    let inputs = [
        "../../tests/fixtures/format/spacing/spacing_input.pas",
        "../../tests/fixtures/format/uses/uses_input.pas",
        "../../tests/fixtures/format/comments/comments_input.pas",
    ];
    for path in &inputs {
        let input = fs::read_to_string(path).unwrap();
        let result = format_source(&input);
        assert!(
            !result.is_empty(),
            "output should not be empty for {}",
            path
        );
        assert!(
            result.contains("unit"),
            "output should contain 'unit' for {}",
            path
        );
        assert!(
            result.contains("end."),
            "output should contain 'end.' for {}",
            path
        );
    }
}

#[test]
fn all_fixtures_are_idempotent() {
    let inputs = [
        "../../tests/fixtures/format/spacing/spacing_input.pas",
        "../../tests/fixtures/format/uses/uses_input.pas",
        "../../tests/fixtures/format/comments/comments_input.pas",
        "../../tests/fixtures/format/indent/basic_input.pas",
    ];
    for path in &inputs {
        let input = fs::read_to_string(path).unwrap();
        let formatted = format_source(&input);
        idempotency_check(&formatted);
    }
}
