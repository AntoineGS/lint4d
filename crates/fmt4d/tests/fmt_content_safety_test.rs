//! Content-preservation safety net (l4d-um8g): `format_source` must refuse
//! to return output that drops or adds source content, so no caller (CLI,
//! format-on-save through pascal-lsp) can write a lossy result.

use pascal_core::FileInfo;
use std::collections::HashSet;
use std::path::PathBuf;

fn format(source: &str) -> fmt4d::Result<String> {
    fmt4d::format_source(
        source.as_bytes(),
        &FileInfo::new(PathBuf::from("T.pas")),
        &fmt4d::FmtConfig::default(),
        &HashSet::new(),
    )
}

#[test]
fn content_preserving_formatting_still_succeeds() {
    let source = "unit T;\ninterface\nUSES SysUtils, Classes; // units\nimplementation\nprocedure P;\nbegin\n  if A then B;  { done }\nend;\nend.\n";
    let output = format(source).expect("content-preserving formatting must succeed");
    assert_ne!(output, source, "the fixture is expected to be reformatted");
    assert!(output.contains("uses"));
    assert!(output.contains("// units"));
    assert!(output.contains("{ done }"));
}

/// Code moved onto the end of a `//` comment line becomes part of the
/// comment (review of l4d-um8g step 1): the output must be refused.
fn assert_not_swallowed_into_line_comment(body: &str) {
    let source =
        format!("unit T;\ninterface\nimplementation\nprocedure P;\nbegin\n  {body}\nend;\nend.\n");
    match format(&source) {
        Ok(output) => {
            // Accepted output must keep the comment as it was.
            let comment = body.lines().find(|l| l.contains("//")).unwrap();
            let comment = &comment[comment.find("//").unwrap()..];
            assert!(
                output.lines().any(|l| l.trim_end().ends_with(comment)),
                "code was swallowed into `{comment}` and the output accepted:\n{output}"
            );
        }
        Err(error) => assert!(
            matches!(error, fmt4d::FmtError::ContentChanged { .. }),
            "expected ContentChanged, got {error:?}"
        ),
    }
}

#[test]
fn then_moved_into_a_line_comment_is_refused() {
    assert_not_swallowed_into_line_comment("if A // c2\n  then B := 1;");
}

#[test]
fn do_moved_into_a_line_comment_is_refused() {
    assert_not_swallowed_into_line_comment("while A // c\n  do B := 1;");
}

#[test]
fn to_moved_into_a_line_comment_is_refused() {
    assert_not_swallowed_into_line_comment("for I := 1 // c\n  to 2 do ;");
}

#[test]
fn semicolon_moved_into_a_line_comment_is_refused() {
    assert_not_swallowed_into_line_comment("X := A // c\n  ;\n  Y := 2;");
}
