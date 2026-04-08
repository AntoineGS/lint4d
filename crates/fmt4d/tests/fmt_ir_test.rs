use std::path::PathBuf;

fn format(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed")
}

fn format_grouped(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.uses.group = true;
    fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed")
}

#[test]
fn ir_simple_unit() {
    let result = format("unit Test;\ninterface\nimplementation\nend.\n");
    assert!(!result.is_empty());
}

#[test]
fn ir_spacing_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas")
        .expect("fixture not found");
    let expected =
        std::fs::read_to_string("../../tests/fixtures/format/spacing/spacing_expected.pas")
            .expect("fixture not found");
    assert_eq!(format(&input), expected);
}

#[test]
fn ir_comments_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas")
        .expect("fixture not found");
    let expected =
        std::fs::read_to_string("../../tests/fixtures/format/comments/comments_expected.pas")
            .expect("fixture not found");
    assert_eq!(format(&input), expected);
}

#[test]
fn ir_indent_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/indent/basic_input.pas")
        .expect("fixture not found");
    let expected = std::fs::read_to_string("../../tests/fixtures/format/indent/basic_expected.pas")
        .expect("fixture not found");
    assert_eq!(format(&input), expected);
}

#[test]
fn ir_uses_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas")
        .expect("fixture not found");
    let expected = std::fs::read_to_string("../../tests/fixtures/format/uses/uses_expected.pas")
        .expect("fixture not found");
    assert_eq!(format_grouped(&input), expected);
}
