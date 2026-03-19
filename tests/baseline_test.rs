use lint4d::config::baseline::Baseline;
use lint4d::engine::{Diagnostic, Severity};

#[test]
fn baseline_suppresses_matching_violation() {
    let diag = Diagnostic {
        rule_id: "empty-except".to_string(),
        severity: Severity::Warning,
        message: "empty except block".to_string(),
        line: 10,
        column: 3,
        end_line: 11,
        end_column: 6,
        help: None,
    };

    let source_line = "  except  ";
    let baseline = Baseline::from_diagnostics("src/MyUnit.pas", &[&diag], &[source_line]);
    assert!(baseline.is_suppressed("src/MyUnit.pas", &diag, source_line));
}

#[test]
fn baseline_does_not_suppress_new_violation() {
    let old_diag = Diagnostic {
        rule_id: "empty-except".to_string(),
        severity: Severity::Warning,
        message: "old".to_string(),
        line: 10,
        column: 3,
        end_line: 10,
        end_column: 6,
        help: None,
    };

    let new_diag = Diagnostic {
        rule_id: "bare-except".to_string(),
        severity: Severity::Warning,
        message: "new".to_string(),
        line: 20,
        column: 3,
        end_line: 20,
        end_column: 6,
        help: None,
    };

    let baseline = Baseline::from_diagnostics("src/MyUnit.pas", &[&old_diag], &["except"]);
    assert!(!baseline.is_suppressed("src/MyUnit.pas", &new_diag, "except"));
}

#[test]
fn baseline_serialization_roundtrip() {
    let diag = Diagnostic {
        rule_id: "with-statement".to_string(),
        severity: Severity::Warning,
        message: "with".to_string(),
        line: 5,
        column: 1,
        end_line: 5,
        end_column: 10,
        help: None,
    };

    let baseline = Baseline::from_diagnostics("src/Test.pas", &[&diag], &["with sl do"]);
    let json = baseline.to_json();
    let loaded = Baseline::from_json(&json).unwrap();
    assert!(loaded.is_suppressed("src/Test.pas", &diag, "with sl do"));
}

#[test]
fn baseline_trims_whitespace_for_hash() {
    let diag = Diagnostic {
        rule_id: "empty-except".to_string(),
        severity: Severity::Warning,
        message: "empty".to_string(),
        line: 10,
        column: 3,
        end_line: 10,
        end_column: 6,
        help: None,
    };

    let baseline = Baseline::from_diagnostics("src/Test.pas", &[&diag], &["  except  "]);
    assert!(baseline.is_suppressed("src/Test.pas", &diag, "    except    "));
}
