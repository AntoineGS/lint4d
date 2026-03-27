use lint4d::engine::{parse_file, FileInfo, Severity};
use std::path::PathBuf;

#[test]
fn parse_valid_file_produces_no_parse_errors() {
    let info = FileInfo::new(PathBuf::from("test.pas"));
    let source = b"unit Test;\ninterface\nimplementation\nend.\n";
    let result = parse_file(&info, source);
    assert!(result.is_ok());
    let (tree, diagnostics) = result.unwrap();
    assert!(!tree.root_node().has_error());
    assert!(diagnostics.is_empty());
}

#[test]
fn parse_invalid_file_produces_parse_error_diagnostic() {
    let info = FileInfo::new(PathBuf::from("test.pas"));
    let source = b"unit Test;\n@@@invalid syntax@@@\nend.\n";
    let result = parse_file(&info, source);
    assert!(result.is_ok());
    let (_tree, diagnostics) = result.unwrap();
    assert!(!diagnostics.is_empty());
    assert_eq!(diagnostics[0].rule_id, "parse-error");
    assert_eq!(diagnostics[0].severity, Severity::Warning);
}
