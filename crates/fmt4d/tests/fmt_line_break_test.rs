//! Tests for AST-aware line breaking (smart line splitting).

#![allow(dead_code)]

use std::path::PathBuf;

fn format_source(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
}

fn format_source_with_max(source: &str, max_line_length: usize) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.max_line_length = max_line_length;
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
}

/// Assert that no line in the output exceeds max_line_length.
fn assert_no_long_lines(output: &str, max_line_length: usize) {
    let long_lines: Vec<(usize, &str)> = output
        .lines()
        .enumerate()
        .filter(|(_, l)| l.len() > max_line_length)
        .collect();
    assert!(
        long_lines.is_empty(),
        "Lines exceed {} chars:\n{}",
        max_line_length,
        long_lines
            .iter()
            .map(|(i, l)| format!("  line {}: ({} chars) {}", i + 1, l.len(), l))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Assert that formatting is idempotent.
fn assert_idempotent(source: &str) {
    let first = format_source(source);
    let second = format_source(&first);
    assert_eq!(
        first, second,
        "Formatting is not idempotent.\nFirst:\n{}\nSecond:\n{}",
        first, second
    );
}

fn assert_idempotent_with_max(source: &str, max_line_length: usize) {
    let first = format_source_with_max(source, max_line_length);
    let second = format_source_with_max(&first, max_line_length);
    assert_eq!(
        first, second,
        "Formatting is not idempotent.\nFirst:\n{}\nSecond:\n{}",
        first, second
    );
}

// ── Column Tracking Validation ──────────────────────────────────

#[test]
fn column_tracking_matches_output() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  WriteLn('hello');
end;
end.
";
    let result = format_source(src);
    assert!(result.contains("procedure P;"), "output:\n{}", result);
    let second = format_source(&result);
    assert_eq!(result, second, "column tracking broke idempotency");
}
