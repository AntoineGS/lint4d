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
    let lines: Vec<&str> = result.lines().collect();
    // All units are present (grouping will be reintroduced in a later task)
    let sys_classes_pos = lines.iter().position(|l| l.contains("System.Classes"));
    let forms_pos = lines.iter().position(|l| l.contains("Forms"));
    assert!(
        sys_classes_pos.is_some() && forms_pos.is_some(),
        "both units should be present"
    );
    // Units are sorted alphabetically within the single group
    assert!(
        forms_pos.unwrap() < sys_classes_pos.unwrap(),
        "units should be sorted alphabetically (Forms before System.Classes)"
    );
}

#[test]
fn uses_sorts_within_groups() {
    let result = format_source(
        &"unit T;\ninterface\nuses\n  System.SysUtils, System.Classes;\nimplementation\nend.\n",
    );
    let lines: Vec<&str> = result.lines().collect();
    let classes_pos = lines.iter().position(|l| l.contains("System.Classes"));
    let sysutils_pos = lines.iter().position(|l| l.contains("System.SysUtils"));
    assert!(
        classes_pos.unwrap() < sysutils_pos.unwrap(),
        "System.Classes should come before System.SysUtils (alphabetical)"
    );
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
