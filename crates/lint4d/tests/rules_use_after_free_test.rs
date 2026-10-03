use lint4d::config::Config;
use lint4d::engine::{FileInfo, run_lint};
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
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 use-after-free, got: {:?}",
        matches
    );
}

#[test]
fn use_after_free_flags_after_freeandnil() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_use_after_freeandnil.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 use-after-free after FreeAndNil, got: {:?}",
        matches
    );
}

#[test]
fn use_after_free_flags_double_free() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_double_free.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 use-after-free for double free, got: {:?}",
        matches
    );
}

#[test]
fn use_after_free_flags_passing_freed_as_param() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/bad_pass_freed_as_param.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Expected 1 use-after-free for passing freed param, got: {:?}",
        matches
    );
}

#[test]
fn use_after_free_allows_reassigned_variable() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/good_reassign_after_free.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();
    assert!(
        matches.is_empty(),
        "Reassigned variable should not flag: {:?}",
        matches
    );
}

#[test]
fn use_after_free_allows_normal_usage() {
    let diagnostics = lint_fixture("tests/fixtures/use_after_free/good_no_use_after_free.pas");
    let matches: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "use-after-free")
        .collect();
    assert!(
        matches.is_empty(),
        "Normal usage before free should not flag: {:?}",
        matches
    );
}

fn use_after_free_lines(fixture_path: &str) -> Vec<(usize, String)> {
    lint_fixture(fixture_path)
        .into_iter()
        .filter(|d| d.rule_id == "use-after-free")
        .map(|d| (d.line, d.message))
        .collect()
}

#[test]
fn use_after_free_flags_use_after_free_call_with_parens() {
    let matches =
        use_after_free_lines("tests/fixtures/use_after_free/bad_use_after_free_with_parens.pas");
    assert_eq!(
        matches,
        vec![(
            10,
            "Use after free: 'obj' is used after being freed".to_string()
        )]
    );
}

#[test]
fn use_after_free_allows_nil_assignment_after_free() {
    let matches = use_after_free_lines("tests/fixtures/use_after_free/good_nil_after_free.pas");
    assert!(
        matches.is_empty(),
        "Assigning nil after free should not flag: {:?}",
        matches
    );
}

#[test]
fn use_after_free_reports_revisited_loop_body_once() {
    let matches = use_after_free_lines("tests/fixtures/use_after_free/bad_use_in_loop_body.pas");
    assert_eq!(
        matches,
        vec![(
            13,
            "Use after free: 'obj' is used after being freed".to_string()
        )]
    );
}

#[test]
fn use_after_free_ignores_names_in_strings_and_comments() {
    let matches =
        use_after_free_lines("tests/fixtures/use_after_free/good_name_in_string_and_comment.pas");
    assert!(
        matches.is_empty(),
        "Names inside string literals or comments are not uses: {:?}",
        matches
    );
}

#[test]
fn use_after_free_flags_freed_read_in_assignment_rhs() {
    let matches =
        use_after_free_lines("tests/fixtures/use_after_free/bad_assignment_reads_freed.pas");
    assert_eq!(
        matches,
        vec![(
            11,
            "Use after free: 'obj' is used after being freed".to_string()
        )]
    );
}
