use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn lint4d() -> Command {
    Command::cargo_bin("lint4d").unwrap()
}

#[test]
fn version_flag() {
    lint4d()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("lint4d"));
}

#[test]
fn no_args_shows_help() {
    lint4d()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn lint_clean_file_exits_zero() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Clean.pas"),
        "unit Clean;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg(dir.path().join("Clean.pas"))
        .assert()
        .success();
}

#[test]
fn lint_with_json_format() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Clean.pas"),
        "unit Clean;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--format")
        .arg("json")
        .arg(dir.path().join("Clean.pas"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"version\": 1"));
}

#[test]
fn init_creates_config_file() {
    let dir = TempDir::new().unwrap();
    lint4d()
        .arg("--init")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(dir.path().join(".lint4d.toml").exists());
}

#[test]
fn list_rules_shows_all_rules() {
    lint4d()
        .arg("--list-rules")
        .assert()
        .success()
        .stdout(predicate::str::contains("empty-except"))
        .stdout(predicate::str::contains("resource-leak-unprotected"));
}

#[test]
fn explain_known_rule() {
    lint4d()
        .arg("--explain")
        .arg("empty-except")
        .assert()
        .success()
        .stdout(predicate::str::contains("empty-except"))
        .stdout(predicate::str::contains("Empty Except Block"));
}

#[test]
fn explain_unknown_rule() {
    lint4d()
        .arg("--explain")
        .arg("nonexistent-rule")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown rule"));
}

#[test]
fn lint_directory_recursively() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(
        sub.join("Unit1.pas"),
        "unit Unit1;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d().arg(dir.path()).assert().success();
}

#[test]
fn fail_on_error_ignores_warnings() {
    let dir = TempDir::new().unwrap();
    // with-statement triggers a warning, not an error
    fs::write(
        dir.path().join("WithTest.pas"),
        "unit WithTest;\ninterface\nimplementation\nprocedure Foo;\nvar Obj: TObject;\nbegin\n  with Obj do\n    Writeln('hi');\nend;\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fail-on")
        .arg("error")
        .arg(dir.path().join("WithTest.pas"))
        .assert()
        .success();
}

#[test]
fn fail_on_warning_catches_warnings() {
    let dir = TempDir::new().unwrap();
    // with-statement triggers a warning
    fs::write(
        dir.path().join("WithTest.pas"),
        "unit WithTest;\ninterface\nimplementation\nprocedure Foo;\nvar Obj: TObject;\nbegin\n  with Obj do\n    Writeln('hi');\nend;\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fail-on")
        .arg("warning")
        .arg(dir.path().join("WithTest.pas"))
        .assert()
        .code(1);
}

#[test]
fn generate_baseline_placeholder() {
    lint4d()
        .arg("--generate-baseline")
        .assert()
        .success()
        .stderr(predicate::str::contains("not yet implemented"));
}
