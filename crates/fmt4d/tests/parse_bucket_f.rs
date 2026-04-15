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

// Ignored until Bucket F lands a fix (scanner-level handling of non-Pascal
// text inside false-branch `{$IF ...}` directive bodies). The test is kept
// green-by-ignore so it flips to a real failure the instant the fix lands
// and starts parsing the body — at which point we remove `#[ignore]`.
#[ignore = "bucket F: non-Pascal directive body — fix not landed yet"]
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
