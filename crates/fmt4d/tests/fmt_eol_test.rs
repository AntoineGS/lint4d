use fmt4d::config::{EndOfLine, FmtConfig};
use std::path::PathBuf;

fn format_with_eol(source: &[u8], eol: EndOfLine) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = FmtConfig {
        end_of_line: eol,
        ..FmtConfig::default()
    };
    fmt4d::formatter::format_source(source, &info, &config, &std::collections::HashSet::new())
        .expect("formatting failed")
}

const LF_SOURCE: &str = "unit Test;\ninterface\nimplementation\nend.\n";

fn crlf_source() -> Vec<u8> {
    LF_SOURCE.replace('\n', "\r\n").into_bytes()
}

// ── Detection tests ────────────────────────────────────────────

#[test]
fn detect_lf_source() {
    assert_eq!(EndOfLine::detect(LF_SOURCE.as_bytes()), EndOfLine::Lf);
}

#[test]
fn detect_crlf_source() {
    assert_eq!(EndOfLine::detect(&crlf_source()), EndOfLine::Crlf);
}

#[test]
fn detect_empty_source_defaults_to_lf() {
    assert_eq!(EndOfLine::detect(b""), EndOfLine::Lf);
}

// ── Auto mode preserves original EOL ───────────────────────────

#[test]
fn auto_preserves_lf() {
    let result = format_with_eol(LF_SOURCE.as_bytes(), EndOfLine::Auto);
    assert!(!result.contains("\r\n"), "expected LF-only output");
    assert!(result.contains('\n'), "expected newlines in output");
}

#[test]
fn auto_preserves_crlf() {
    let result = format_with_eol(&crlf_source(), EndOfLine::Auto);
    assert!(result.contains("\r\n"), "expected CRLF in output");
}

// ── Enforced LF mode ──────────────────────────────────────────

#[test]
fn enforce_lf_on_lf_source() {
    let result = format_with_eol(LF_SOURCE.as_bytes(), EndOfLine::Lf);
    assert!(!result.contains("\r\n"), "expected LF-only output");
    assert!(result.contains('\n'));
}

#[test]
fn enforce_lf_on_crlf_source() {
    let result = format_with_eol(&crlf_source(), EndOfLine::Lf);
    assert!(
        !result.contains("\r\n"),
        "expected LF-only output after enforcing LF"
    );
    assert!(result.contains('\n'));
}

// ── Enforced CRLF mode ────────────────────────────────────────

#[test]
fn enforce_crlf_on_lf_source() {
    let result = format_with_eol(LF_SOURCE.as_bytes(), EndOfLine::Crlf);
    assert!(
        result.contains("\r\n"),
        "expected CRLF in output after enforcing CRLF"
    );
    // Verify no bare LF (every \n should be preceded by \r)
    let bytes = result.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            assert!(
                i > 0 && bytes[i - 1] == b'\r',
                "found bare LF at byte {}",
                i
            );
        }
    }
}

#[test]
fn enforce_crlf_on_crlf_source() {
    let result = format_with_eol(&crlf_source(), EndOfLine::Crlf);
    assert!(result.contains("\r\n"), "expected CRLF in output");
    // No double \r\r\n
    assert!(!result.contains("\r\r\n"), "found double CR in output");
}

// ── Idempotency with EOL ──────────────────────────────────────

#[test]
fn idempotent_with_crlf() {
    let first = format_with_eol(&crlf_source(), EndOfLine::Crlf);
    let second = format_with_eol(first.as_bytes(), EndOfLine::Crlf);
    assert_eq!(first, second, "CRLF formatting not idempotent");
}

#[test]
fn idempotent_with_lf() {
    let first = format_with_eol(LF_SOURCE.as_bytes(), EndOfLine::Lf);
    let second = format_with_eol(first.as_bytes(), EndOfLine::Lf);
    assert_eq!(first, second, "LF formatting not idempotent");
}

#[test]
fn idempotent_with_auto_from_crlf() {
    let first = format_with_eol(&crlf_source(), EndOfLine::Auto);
    let second = format_with_eol(first.as_bytes(), EndOfLine::Auto);
    assert_eq!(first, second, "Auto (CRLF) formatting not idempotent");
}

// ── Config parsing ─────────────────────────────────────────────

#[test]
fn config_parses_end_of_line_lf() {
    let toml = r#"
[format]
end_of_line = "lf"
"#;
    let config = FmtConfig::from_toml(toml).unwrap();
    assert_eq!(config.end_of_line, EndOfLine::Lf);
}

#[test]
fn config_parses_end_of_line_crlf() {
    let toml = r#"
[format]
end_of_line = "crlf"
"#;
    let config = FmtConfig::from_toml(toml).unwrap();
    assert_eq!(config.end_of_line, EndOfLine::Crlf);
}

#[test]
fn config_parses_end_of_line_auto() {
    let toml = r#"
[format]
end_of_line = "auto"
"#;
    let config = FmtConfig::from_toml(toml).unwrap();
    assert_eq!(config.end_of_line, EndOfLine::Auto);
}

#[test]
fn config_default_is_auto() {
    let config = FmtConfig::default();
    assert_eq!(config.end_of_line, EndOfLine::Auto);
}

// ── Mixed-EOL normalization ────────────────────────────────────

#[test]
#[ignore = "TST-M4: mixed-EOL normalization pending"]
fn mixed_eol_auto_mode_produces_dominant_only() {
    // Regression guard for TST-M4: a file with mixed EOLs under Auto mode
    // should normalize to the dominant EOL.
    //
    // 3 LF lines + 1 CRLF line → dominant is LF → output must be all LF.
    let mixed = b"unit T;\r\ninterface\nimplementation\nend.\n";
    let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig {
        end_of_line: fmt4d::config::EndOfLine::Auto,
        ..fmt4d::config::FmtConfig::default()
    };
    let result =
        fmt4d::formatter::format_source(mixed, &info, &config, &std::collections::HashSet::new())
            .expect("formatting failed");
    assert!(
        !result.contains("\r\n"),
        "output retained CRLF under Auto/LF-dominant: {result:?}"
    );
}
