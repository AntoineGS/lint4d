use lint4d::config::Config;
use lint4d::engine::{run_lint, FileInfo};
use std::fs;
use std::path::PathBuf;

fn lint_fixture(fixture_path: &str) -> Vec<lint4d::engine::Diagnostic> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    run_lint(&file, &source, &config)
}

#[test]
fn use_after_free_flags_method_call_after_free() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_use_after_free.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "use-after-free").collect();
    assert_eq!(matches.len(), 1, "Expected 1 use-after-free, got: {:?}", matches);
}

#[test]
fn use_after_free_flags_after_freeandnil() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_use_after_freeandnil.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "use-after-free").collect();
    assert_eq!(matches.len(), 1, "Expected 1 use-after-free after FreeAndNil, got: {:?}", matches);
}

#[test]
fn use_after_free_flags_double_free() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_double_free.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "use-after-free").collect();
    assert_eq!(matches.len(), 1, "Expected 1 use-after-free for double free, got: {:?}", matches);
}

#[test]
fn use_after_free_flags_passing_freed_as_param() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_pass_freed_as_param.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "use-after-free").collect();
    assert_eq!(matches.len(), 1, "Expected 1 use-after-free for passing freed param, got: {:?}", matches);
}

#[test]
fn use_after_free_allows_reassigned_variable() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/good_reassign_after_free.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "use-after-free").collect();
    assert!(matches.is_empty(), "Reassigned variable should not flag: {:?}", matches);
}

#[test]
fn use_after_free_allows_normal_usage() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/good_no_use_after_free.pas");
    let matches: Vec<_> = diagnostics.iter().filter(|d| d.rule_id == "use-after-free").collect();
    assert!(matches.is_empty(), "Normal usage before free should not flag: {:?}", matches);
}
