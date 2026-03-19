use lint4d::engine::{Diagnostic, FileInfo, FileType, Severity};
use std::path::PathBuf;

#[test]
fn severity_ordering() {
    assert!(Severity::Error > Severity::Warning);
    assert!(Severity::Warning > Severity::Hint);
}

#[test]
fn severity_from_str() {
    assert_eq!("error".parse::<Severity>().unwrap(), Severity::Error);
    assert_eq!("warning".parse::<Severity>().unwrap(), Severity::Warning);
    assert_eq!("hint".parse::<Severity>().unwrap(), Severity::Hint);
    assert!("invalid".parse::<Severity>().is_err());
}

#[test]
fn file_type_from_extension() {
    assert_eq!(FileType::from_extension("pas"), Some(FileType::Pas));
    assert_eq!(FileType::from_extension("dpr"), Some(FileType::Dpr));
    assert_eq!(FileType::from_extension("dpk"), Some(FileType::Dpk));
    assert_eq!(FileType::from_extension("txt"), None);
    assert_eq!(FileType::from_extension("PAS"), Some(FileType::Pas));
}

#[test]
fn file_info_construction() {
    let info = FileInfo::new(PathBuf::from("src/MyUnit.pas"));
    assert_eq!(info.file_type, FileType::Pas);
    assert_eq!(info.path, PathBuf::from("src/MyUnit.pas"));
}

#[test]
fn diagnostic_display() {
    let diag = Diagnostic {
        rule_id: "empty-except".to_string(),
        severity: Severity::Warning,
        message: "empty except block".to_string(),
        line: 10,
        column: 3,
        end_line: 10,
        end_column: 6,
        help: Some("add error handling".to_string()),
    };
    assert_eq!(diag.rule_id, "empty-except");
    assert_eq!(diag.line, 10);
}
