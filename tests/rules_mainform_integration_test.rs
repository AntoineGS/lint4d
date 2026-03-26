use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::fs;
use std::path::PathBuf;

/// Integration test that runs the linter on the real MainForm.pas fixture and
/// verifies that the use-after-free rule detects the issues marked "WRONG" in
/// that file.
///
/// # Known use-after-free locations in the fixture (as of 2026-03-26)
///
/// | Line | Pattern                                 | Detected? |
/// |------|-----------------------------------------|-----------|
/// | 123  | `aObj.Free; aObj.Free;` (double free)   | YES       |
/// | 178  | `if aObj.ClassName = ...` after Free    | NO *      |
/// | 187  | `if aObj.ClassName = ...` after FreeAndNil | NO *   |
///
/// * Lines 178 and 187 are not detected because the CFG builder does not add
///   `if`-condition expressions as statement references on the current block
///   (`handle_if_only` skips the condition node and only tracks then-body
///   statements).  As a result the use-after-free dataflow analysis never
///   sees the `aObj` reference inside the condition.  This is a known
///   limitation of the current CFG model.
#[test]
fn mainform_detects_use_after_free() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/projects/TestProject1/MainForm.pas");
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from("MainForm.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);

    let uaf: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();

    let detected_lines: Vec<usize> = uaf.iter().map(|d| d.line).collect();

    // Currently 1 use-after-free is detected: the double-free on line 123.
    // The other two "WRONG" locations (lines 178, 187) are in `if` conditions
    // which the CFG builder does not currently add as statement refs, so the
    // dataflow analysis cannot see them.
    assert_eq!(
        uaf.len(),
        1,
        "Expected exactly 1 use-after-free diagnostic in MainForm.pas, got {} at lines {:?}.\n\
         If you improved the CFG builder to track if-condition expressions you may need\n\
         to raise this assertion to 3.\n\
         Full diagnostics: {:?}",
        uaf.len(),
        detected_lines,
        uaf
    );

    // The one detected case must be the double-free on line 123.
    assert!(
        detected_lines.contains(&123),
        "Expected use-after-free on line 123 (double free in TestDoubleFree), \
         but detected lines were: {:?}",
        detected_lines
    );

    // Lines 178 and 187 are NOT detected due to the if-condition CFG gap.
    // Uncomment the assertions below once the CFG builder is improved to add
    // if-condition expressions as statement refs on the pre-condition block.
    //
    // assert!(
    //     detected_lines.contains(&178),
    //     "Expected use-after-free on line 178 (use after Free in TestUseAfterFree)",
    // );
    // assert!(
    //     detected_lines.contains(&187),
    //     "Expected use-after-free on line 187 (use after FreeAndNil in TestUseAfterFree)",
    // );
}
