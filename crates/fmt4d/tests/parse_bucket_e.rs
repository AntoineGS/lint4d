//! Bucket E regression tests — parse errors caused by pre-existing grammar
//! gaps in tree-sitter-pascal, discovered via
//! `fmt4d --project WebImportExportStandAloneServer.dproj`.
//!
//! See `.full-review/parse-error-buckets-summary.md` for the full inventory.
//!
//! Each test formats a minimal reproduction and asserts that (a) formatting
//! succeeds without a parse error, and (b) the `deprecated` hint / comparison
//! operator survives the round-trip.

use std::collections::HashSet;
use std::path::PathBuf;

fn format_source(src: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(src.as_bytes(), &info, &config, &HashSet::new())
        .expect("format succeeds")
}

// ── E2: `deprecated` hint on `string` field ────────────────────────────
// Grammar gap: `declString` rule did not accept the `deprecated` suffix,
// even though `typeref` did. Fixed by mirroring typeref's hint suffix on
// `declString` and making the hint message optional on both rules.

#[test]
fn bucket_e2_string_field_with_deprecated_bare() {
    let src = "\
unit T;
interface
type
  TFoo = class
    fEDS: string deprecated;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("string deprecated"),
        "expected `string deprecated` to survive formatting, got:\n{result}"
    );
}

#[test]
fn bucket_e2_string_field_with_deprecated_message() {
    let src = "\
unit T;
interface
type
  TFoo = class
    fEDS: string deprecated 'use something else';
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("deprecated 'use something else'"),
        "expected deprecation message to survive formatting, got:\n{result}"
    );
}

// ── E1: comparison inside call argument parsed as binary, not generic ──
// Grammar gap: `exprTpl`'s template args were `$._expr`, so `IfThen(qty<0,
// -1, 1)` parsed greedily as a generic call with three literal type args,
// failing to find the closing `>` and recovering with `(MISSING kGt)`.
// Fixed by restricting tpl args to `$._typeref` (literals no longer match)
// and adding `prec.dynamic` to keep real generic calls preferred when both
// interpretations are valid.

#[test]
fn bucket_e1_comparison_inside_call_argument() {
    let src = "\
unit T;
interface
implementation
function Test(qty: Integer): Integer;
begin
  Result := IfThen(qty < 0, -1, 1);
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("IfThen(qty < 0, -1, 1)") || result.contains("IfThen(qty<0, -1, 1)"),
        "expected comparison-in-call to survive formatting, got:\n{result}"
    );
}

#[test]
fn bucket_e1_real_generic_call_still_works() {
    // Regression guard: the E1 fix tightens exprTpl's type args but must
    // not break real generic calls like `TList<Integer>.Create` or
    // `Bar<string>('hello')`.
    let src = "\
unit T;
interface
implementation
procedure P;
var
  list: TList<Integer>;
begin
  list := TList<Integer>.Create;
  x := Bar<string>('hello');
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("TList<Integer>.Create"),
        "expected generic method call to survive formatting, got:\n{result}"
    );
    assert!(
        result.contains("Bar<string>('hello')"),
        "expected generic call to survive formatting, got:\n{result}"
    );
}

#[test]
fn bucket_e2_typeref_field_with_deprecated_bare() {
    // Same pattern on a non-string typeref. The typeref rule previously
    // required a non-empty message after `deprecated`; this test pins the
    // bare form.
    let src = "\
unit T;
interface
type
  TFoo = class
    fA: Integer deprecated;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("Integer deprecated"),
        "expected `Integer deprecated` to survive formatting, got:\n{result}"
    );
}
