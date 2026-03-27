use std::fs;
use std::path::PathBuf;

fn format_source(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
}

#[test]
fn basic_indentation() {
    let input = fs::read_to_string("../../tests/fixtures/format/indent/basic_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/indent/basic_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn format_minimal_unit() {
    let source = "unit Test;\ninterface\nimplementation\nend.\n";
    let result = format_source(source);
    assert!(result.contains("unit Test;"), "missing 'unit Test;'");
    assert!(result.contains("interface"), "missing 'interface'");
    assert!(
        result.contains("implementation"),
        "missing 'implementation'"
    );
    assert!(result.contains("end."), "missing 'end.'");
}

#[test]
fn format_idempotent() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/indent/basic_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(
        result, input,
        "formatting already-formatted code should produce identical output"
    );
}
