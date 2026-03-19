use lint4d::engine::{Diagnostic, Severity};
use lint4d::output::text::format_diagnostics;

#[test]
fn formats_single_diagnostic() {
    let source = "unit Test;\n\nobj := TMyObject.Create;\nobj.SomeFun;\ntry\n  obj.DoWork;\nfinally\n  obj.Free;\nend;\n";
    let diag = Diagnostic {
        rule_id: "resource-leak-unprotected".to_string(),
        severity: Severity::Error,
        message: "code between constructor and try..finally".to_string(),
        line: 4,
        column: 1,
        end_line: 4,
        end_column: 13,
        help: Some("move this call inside the try block".to_string()),
    };

    let output = format_diagnostics("src/MyUnit.pas", source.as_bytes(), &[diag], false);
    assert!(output.contains("error[resource-leak-unprotected]"), "Missing rule header: {}", output);
    assert!(output.contains("src/MyUnit.pas:4:1"), "Missing location: {}", output);
    assert!(output.contains("obj.SomeFun;"), "Missing source line: {}", output);
    assert!(output.contains("move this call inside the try block"), "Missing help: {}", output);
}

#[test]
fn formats_no_diagnostics_as_empty() {
    let output = format_diagnostics("src/Clean.pas", b"unit Clean;", &[], false);
    assert!(output.is_empty());
}
