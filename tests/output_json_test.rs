use lint4d::engine::{Diagnostic, Severity};
use lint4d::output::json::format_json_output;
use serde_json::Value;

#[test]
fn formats_json_output_structure() {
    let file_diagnostics = vec![(
        "src/MyUnit.pas".to_string(),
        vec![Diagnostic {
            rule_id: "empty-except".to_string(),
            severity: Severity::Warning,
            message: "empty except block".to_string(),
            line: 10,
            column: 3,
            end_line: 11,
            end_column: 6,
            help: Some("add error handling".to_string()),
            scope: None,
        }],
    )];

    let json_str = format_json_output(&file_diagnostics);
    let value: Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(value["version"], 1);
    assert_eq!(value["files"][0]["file"], "src/MyUnit.pas");
    assert_eq!(
        value["files"][0]["diagnostics"][0]["rule_id"],
        "empty-except"
    );
    assert_eq!(value["files"][0]["diagnostics"][0]["severity"], "warning");
    assert_eq!(value["files"][0]["diagnostics"][0]["line"], 10);
    assert_eq!(
        value["files"][0]["diagnostics"][0]["help"],
        "add error handling"
    );
}

#[test]
fn empty_diagnostics_produces_empty_files_array() {
    let json_str = format_json_output(&[]);
    let value: Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(value["version"], 1);
    assert!(value["files"].as_array().unwrap().is_empty());
}
