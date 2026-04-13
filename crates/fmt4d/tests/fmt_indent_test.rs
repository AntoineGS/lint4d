use std::fs;

mod common;
use common::format_source;

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
