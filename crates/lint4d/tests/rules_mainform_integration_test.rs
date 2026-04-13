use lint4d::config::Config;
use lint4d::engine::{FileInfo, run_lint};
use std::fs;
use std::path::PathBuf;

/// Integration test that runs the linter on the real MainForm.pas fixture and
/// verifies that the use-after-free rule detects the issues marked "WRONG" in
/// that file.
///
/// # Known use-after-free locations in the fixture (as of 2026-03-26)
///
/// | Line | Pattern                                    | Detected? |
/// |------|--------------------------------------------|-----------|
/// | 124  | `aObj.Free; aObj.Free;` (double free)       | YES       |
/// | 179  | `if aObj.ClassName = ...` after Free        | YES       |
/// | 188  | `if aObj.ClassName = ...` after FreeAndNil  | YES       |
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

    assert_eq!(
        uaf.len(),
        3,
        "Expected 3 use-after-free diagnostics in MainForm.pas, got {} at lines {:?}.\n\
         Full diagnostics: {:?}",
        uaf.len(),
        detected_lines,
        uaf
    );

    assert!(
        detected_lines.contains(&124),
        "Expected use-after-free on line 124 (double free in TestDoubleFree), \
         but detected lines were: {:?}",
        detected_lines
    );
    assert!(
        detected_lines.contains(&179),
        "Expected use-after-free on line 179 (use after Free in TestUseAfterFree), \
         but detected lines were: {:?}",
        detected_lines
    );
    assert!(
        detected_lines.contains(&188),
        "Expected use-after-free on line 188 (use after FreeAndNil in TestUseAfterFree), \
         but detected lines were: {:?}",
        detected_lines
    );
}
