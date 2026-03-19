use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::path::PathBuf;

#[test]
fn engine_runs_on_valid_file_with_no_issues() {
    let source = b"unit Clean;\ninterface\nimplementation\nend.\n";
    let file = FileInfo::new(PathBuf::from("Clean.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(diagnostics.is_empty());
}

#[test]
fn engine_reports_parse_errors() {
    let source = b"unit Bad;\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Bad.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(diagnostics.iter().any(|d| d.rule_id == "parse-error"));
}

#[test]
fn engine_skips_parse_errors_in_dpr_files() {
    // The `in 'path'` syntax in uses clauses is valid Delphi but unsupported
    // by tree-sitter-pascal, producing spurious parse errors. These should be
    // suppressed for .dpr/.dpk files.
    let source = b"program Test;\nuses\n  MyUnit in 'path\\MyUnit.pas';\nbegin\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.dpr"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    let parse_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error")
        .collect();
    assert!(
        parse_errors.is_empty(),
        "Parse errors should be suppressed for .dpr files, got: {:?}",
        parse_errors
    );
}

#[test]
fn engine_skips_bare_raise_parse_errors() {
    // Bare `raise;` (re-raise) is valid Delphi but produces ERROR nodes in
    // tree-sitter-pascal. These should be suppressed.
    let source = b"unit Test;\ninterface\nimplementation\nprocedure Foo;\nbegin\n  try\n    DoWork;\n  except\n    raise;\n  end;\nend;\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    let raise_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error" && d.message.contains("raise"))
        .collect();
    assert!(
        raise_errors.is_empty(),
        "Bare raise; should not produce parse errors, got: {:?}",
        raise_errors
    );
}

#[test]
fn engine_keeps_parse_errors_in_pas_files() {
    // Regular .pas files should still report parse errors.
    let source = b"unit Bad;\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Bad.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(
        diagnostics.iter().any(|d| d.rule_id == "parse-error"),
        "Parse errors should still be reported for .pas files"
    );
}

#[test]
fn engine_filters_suppressed_diagnostics() {
    let source = b"unit Test;\n// lint4d:ignore parse-error\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = Config::from_str("version = 1").unwrap();

    let diagnostics = run_lint(&file, source, &config);
    // Parse error on line 3 should be suppressed by comment on line 2
    let parse_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error" && d.line == 3)
        .collect();
    assert!(
        parse_errors.is_empty(),
        "Parse error on line 3 should be suppressed, got: {:?}",
        parse_errors
    );
}
