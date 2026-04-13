use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn cli_prints_help() {
    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("fmt4d"));
}

#[test]
fn cli_exits_zero_on_no_args() {
    Command::cargo_bin("fmt4d").unwrap().assert().success();
}

#[test]
fn check_mode_exits_zero_when_formatted() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("test.pas");
    // Write already-formatted content
    let content = "unit Test;\n\ninterface\n\nimplementation\n\nend.\n";
    fs::write(&file_path, content).unwrap();

    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg("--check")
        .arg(file_path.to_str().unwrap())
        .assert()
        .success();
}

#[test]
fn check_mode_exits_one_when_unformatted() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("test.pas");
    // Write poorly-formatted content (missing blank lines between sections)
    let content = "unit Test;\ninterface\ntype\nTFoo = class\nprivate\nFX: Integer;\nend;\nimplementation\nend.\n";
    fs::write(&file_path, content).unwrap();

    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg("--check")
        .arg(file_path.to_str().unwrap())
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Would reformat"));
}

#[test]
fn diff_mode_shows_changes() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("test.pas");
    // Write poorly-formatted content
    let content = "unit Test;\ninterface\ntype\nTFoo = class\nprivate\nFX: Integer;\nend;\nimplementation\nend.\n";
    fs::write(&file_path, content).unwrap();

    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg("--diff")
        .arg(file_path.to_str().unwrap())
        .assert()
        .success()
        .stdout(predicate::str::contains("---"))
        .stdout(predicate::str::contains("+++"));

    // File should NOT be modified
    let after = fs::read_to_string(&file_path).unwrap();
    assert_eq!(after, content, "diff mode should not modify the file");
}

#[test]
fn stdin_mode_formats_to_stdout() {
    let input = "unit Test;\ninterface\nimplementation\nend.\n";

    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg("--stdin")
        .write_stdin(input)
        .assert()
        .success()
        .stdout(predicate::str::contains("unit Test;"))
        .stdout(predicate::str::contains("interface"))
        .stdout(predicate::str::contains("implementation"))
        .stdout(predicate::str::contains("end."));
}

#[test]
fn default_mode_formats_in_place() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("test.pas");
    let content = "unit Test;\ninterface\ntype\nTFoo = class\nprivate\nFX: Integer;\nend;\nimplementation\nend.\n";
    fs::write(&file_path, content).unwrap();

    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg(file_path.to_str().unwrap())
        .assert()
        .success()
        .stdout(predicate::str::contains("Formatted:"));

    // File should be modified
    let after = fs::read_to_string(&file_path).unwrap();
    assert_ne!(after, content, "file should have been formatted");
}

#[test]
fn init_flag_writes_parseable_toml() {
    // Regression guard for TST-M6: `fmt4d --init` must write a .fmt4d.toml
    // that FmtConfig::from_toml accepts.
    let dir = TempDir::new().unwrap();

    Command::cargo_bin("fmt4d")
        .unwrap()
        .current_dir(dir.path())
        .arg("--init")
        .assert()
        .success();

    let written =
        fs::read_to_string(dir.path().join(".fmt4d.toml")).expect(".fmt4d.toml was not created");

    let parsed = fmt4d::config::FmtConfig::from_toml(&written);
    assert!(
        parsed.is_ok(),
        ".fmt4d.toml from --init failed to parse: {:?}",
        parsed.err()
    );
}
