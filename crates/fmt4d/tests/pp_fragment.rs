//! Integration tests for mid-expression {$ifdef}...{$endif} fragments.
//!
//! Each fixture exercises one of the five directive-fragment patterns the
//! C external scanner in tree-sitter-pascal is expected to handle. Each
//! test verifies:
//!  1. fmt4d formats the fixture without returning an error.
//!  2. Every `{$` sequence in the input appears in the output (byte-count
//!     equality — the fragment span is opaque to fmt4d so it should emit
//!     the directives verbatim).
//!  3. Formatting is idempotent: re-formatting the output yields the same
//!     bytes.
//!
//! Entry point: `fmt4d::formatter::format_source(bytes, &info, &config, &units)`
//! (matching the idiom used throughout the existing fmt4d integration suite).

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ppFragment")
        .join(name)
}

fn format_bytes(source: &[u8]) -> Vec<u8> {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source, &info, &config, &HashSet::new())
        .expect("format succeeds")
        .into_bytes()
}

fn count_directive_opens(bytes: &[u8]) -> usize {
    bytes.windows(2).filter(|w| *w == b"{$").count()
}

fn assert_fragment_preserving(fixture: &str) {
    let path = fixture_path(fixture);
    let source = fs::read(&path).expect("fixture exists");

    // 1. Format succeeds (panics via .expect above if not).
    let formatted = format_bytes(&source);

    // 2. Directive byte-count preserved.
    let source_opens = count_directive_opens(&source);
    let formatted_opens = count_directive_opens(&formatted);
    assert_eq!(
        source_opens, formatted_opens,
        "fixture {fixture}: input has {source_opens} directive opens, output has {formatted_opens}",
    );

    // 3. Idempotency: re-format the output and expect byte equality.
    let second = format_bytes(&formatted);
    assert_eq!(
        formatted, second,
        "fixture {fixture}: formatter is not idempotent",
    );
}

#[test]
fn pattern_a_type_substitution() {
    assert_fragment_preserving("pattern_a_type_substitution.pas");
}

#[test]
fn pattern_b_namespace_prefix() {
    assert_fragment_preserving("pattern_b_namespace_prefix.pas");
}

#[test]
fn pattern_c_full_statement() {
    assert_fragment_preserving("pattern_c_full_statement.pas");
}

#[test]
fn pattern_d_cast_wrapping() {
    assert_fragment_preserving("pattern_d_cast_wrapping.pas");
}

#[test]
fn pattern_e_exception_type() {
    assert_fragment_preserving("pattern_e_exception_type.pas");
}

#[test]
fn bucket_a_const_expression() {
    assert_fragment_preserving("bucket_a_const_expression.pas");
}

#[test]
fn bucket_a_assignment_rhs() {
    assert_fragment_preserving("bucket_a_assignment_rhs.pas");
}

#[test]
fn bucket_a_call_argument() {
    assert_fragment_preserving("bucket_a_call_argument.pas");
}

#[test]
fn bucket_b_statement_mid_block() {
    assert_fragment_preserving("bucket_b_statement_mid_block.pas");
}

#[test]
fn bucket_c_uses_semi_parses_and_is_idempotent() {
    assert_fragment_preserving("bucket_c_uses_semi.pas");
}

#[test]
fn bucket_c_visibility_directive() {
    assert_fragment_preserving("bucket_c_visibility_directive.pas");
}

#[test]
fn bucket_c_visibility_strict() {
    assert_fragment_preserving("bucket_c_visibility_strict.pas");

    // `strict private` (and `strict protected`) must stay joined on a
    // single line inside a ppDeclSection. Regression guard for the bug
    // where build_pp_decl_section emitted a Hardline after `strict`.
    let path = fixture_path("bucket_c_visibility_strict.pas");
    let source = fs::read(&path).expect("fixture exists");
    let formatted = format_bytes(&source);
    let formatted_str = std::str::from_utf8(&formatted).expect("utf8 output");
    assert!(
        formatted_str.contains("strict private"),
        "formatter split `strict private` across lines; got:\n{formatted_str}",
    );
}

#[test]
fn bucket_c_partial_if() {
    assert_fragment_preserving("bucket_c_partial_if.pas");
}

#[test]
fn bucket_c_partial_if_else() {
    assert_fragment_preserving("bucket_c_partial_if_else.pas");
}
