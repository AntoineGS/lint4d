use std::path::PathBuf;

fn format_old(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config)
        .expect("old formatter failed")
}

fn format_new(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source_ir(source.as_bytes(), &info, &config)
        .expect("new formatter failed")
}

fn assert_pipelines_match(label: &str, source: &str) {
    let old = format_old(source);
    let new = format_new(source);
    assert_eq!(
        old, new,
        "\nPipeline mismatch in: {}\n=== OLD ===\n{}\n=== NEW ===\n{}\n",
        label, old, new
    );
}

#[test]
fn simple_unit() {
    assert_pipelines_match(
        "simple_unit",
        "unit Test;\ninterface\nimplementation\nend.\n",
    );
}

#[test]
fn spacing_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas")
        .expect("fixture not found");
    assert_pipelines_match("spacing", &input);
}

#[test]
fn comments_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas")
        .expect("fixture not found");
    assert_pipelines_match("comments", &input);
}

#[test]
fn indent_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/indent/basic_input.pas")
        .expect("fixture not found");
    assert_pipelines_match("indent", &input);
}

#[test]
fn uses_fixture() {
    let input = std::fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas")
        .expect("fixture not found");
    assert_pipelines_match("uses", &input);
}
