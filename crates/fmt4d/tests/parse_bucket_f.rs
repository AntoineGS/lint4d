//! Bucket F regression tests — non-Pascal content wrapped in a false-branch
//! `{$IF ...}`/`{$IFEND}` directive block.
//!
//! Discovered in
//! `C:\Multidev\Projects\WebImportExport\WebImportExportAPI\WebImportExportExecutePOS.pas:19349`
//! — a French developer note (`rappel: developper en 32 bits ...`) was
//! wrapped in `{$IF DEFINED(WIN32) AND NOT DEFINED(UNITTEST)}` to dodge the
//! compiler. Delphi skips the body when the condition evaluates false, but
//! tree-sitter-pascal parses both branches of every directive, so the free-
//! form text blows up the parse and the diagnostic spans the whole file.
//!
//! See `.full-review/parse-error-buckets-summary.md` for the full inventory.

use std::collections::HashSet;
use std::path::PathBuf;

fn format_source(src: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(src.as_bytes(), &info, &config, &HashSet::new())
        .expect("format succeeds")
}

// Bucket F regression test: non-Pascal content wrapped in a false-branch
// `{$IF ...}` directive block must survive formatting verbatim.
#[test]
fn bucket_f1_if_ifend_wrapping_non_pascal_text() {
    // Minimal reproduction bisected from WebImportExportExecutePOS.pas:19349
    // (19,360-line file). Stripping just the three offending lines made the
    // parse succeed; this pins the construct itself.
    let src = "\
unit Minimal;
interface
implementation
{$IF DEFINED(WIN32) AND NOT DEFINED(UNITTEST)}
rappel: developper en 32 bits pour plus de stabilite
{$IFEND}
end.
";
    let result = format_source(src);
    assert!(
        result.contains("rappel: developper"),
        "expected free-form directive body to survive formatting, got:\n{result}"
    );
}

#[test]
fn bucket_f_full_file_round_trips_with_surrounding_code() {
    // A realistic unit with a Bucket F block wedged between two procedure
    // declarations. The Bucket F span must survive verbatim, and the
    // surrounding code must still format normally.
    let src = "\
unit Sample;

interface

procedure Alpha(x: Integer);
procedure Beta(y: Integer);

implementation

procedure Alpha(x: Integer);
begin
  x := x + 1;
end;

{$IF DEFINED(WIN32) AND NOT DEFINED(UNITTEST)}
rappel: developper en 32 bits pour plus de stabilite
{$IFEND}

procedure Beta(y: Integer);
begin
  y := y * 2;
end;

end.
";
    let formatted = format_source(src);

    // Bucket F span survives verbatim.
    assert!(
        formatted.contains("{$IF DEFINED(WIN32) AND NOT DEFINED(UNITTEST)}"),
        "opening directive must survive:\n{formatted}"
    );
    assert!(
        formatted.contains("rappel: developper en 32 bits pour plus de stabilite"),
        "body text must survive:\n{formatted}"
    );
    assert!(
        formatted.contains("{$IFEND}"),
        "closing directive must survive:\n{formatted}"
    );

    // Surrounding procedures are still present.
    assert!(formatted.contains("procedure Alpha(x: Integer);"));
    assert!(formatted.contains("procedure Beta(y: Integer);"));
    assert!(formatted.contains("x := x + 1;"));
    assert!(formatted.contains("y := y * 2;"));

    // Idempotence: formatting the output again produces the same string.
    let second_pass = format_source(&formatted);
    assert_eq!(
        formatted, second_pass,
        "formatter must be idempotent on Bucket F round-trips"
    );
}
