use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

const DEMO: &str = "unit Demo;
interface
implementation

procedure TFoo.Load(A: Integer);
begin
  Bar;
end;

procedure TFoo.Load(S: string);
begin
  Baz;
end;

procedure TBar.Save;
begin
  Qux;
end;

initialization
  Setup;
end.
";

fn demo() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("demo.pas"), DEMO).unwrap();
    dir
}

fn digest(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("lint4d").unwrap();
    command
        .current_dir(dir.path())
        .arg("digest")
        .arg("demo.pas");
    command
}

#[test]
fn default_is_the_routine_index() {
    let dir = demo();
    digest(&dir)
        .assert()
        .code(0)
        .stdout(predicate::str::starts_with("demo.pas  4 routines\n"))
        .stdout(predicate::str::contains("TBar.Save"))
        .stdout(predicate::str::contains("Demo.<initialization>"));
}

#[test]
fn routine_matches_every_overload() {
    let dir = demo();
    digest(&dir).args(["--routine", "TFoo.Load"]).assert().code(0).stdout(
        "TFoo.Load  demo.pas:5-8 (4 lines)\nL7 calls Bar\n\nTFoo.Load  demo.pas:10-13 (4 lines)\nL12 calls Baz\n",
    );
}

#[test]
fn routine_falls_back_to_the_last_component() {
    let dir = demo();
    digest(&dir)
        .args(["--routine", "save"])
        .assert()
        .code(0)
        .stdout("TBar.Save  demo.pas:15-18 (4 lines)\nL17 calls Qux\n");
}

#[test]
fn line_selects_one_routine() {
    let dir = demo();
    digest(&dir)
        .args(["--line", "12"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("L12 calls Baz"))
        .stdout(predicate::str::contains("L7 calls Bar").not());
}

#[test]
fn line_in_initialization_selects_section() {
    let dir = demo();
    digest(&dir)
        .args(["--line", "21"])
        .assert()
        .code(0)
        .stdout(predicate::str::starts_with(
            "Demo.<initialization>  demo.pas:20-",
        ))
        .stdout(predicate::str::contains("L21 calls Setup"));
}

#[test]
fn unknown_routine_fails_with_suggestions() {
    let dir = demo();
    digest(&dir)
        .args(["--routine", "Lod"])
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("no routine named 'Lod'"))
        .stderr(predicate::str::contains("closest routines:"))
        .stderr(predicate::str::contains("TFoo.Load (L5-8)"));
}

#[test]
fn line_outside_routines_fails_with_suggestions() {
    let dir = demo();
    digest(&dir)
        .args(["--line", "2"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no routine contains line 2"))
        .stderr(predicate::str::contains("TFoo.Load (L5-8)"));
}

#[test]
fn json_serializes_the_selected_routines() {
    let dir = demo();
    let output = digest(&dir)
        .args(["--format", "json", "--routine", "TBar.Save"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["routines"].as_array().unwrap().len(), 1);
    assert_eq!(json["routines"][0]["name"], "TBar.Save");
}

#[test]
fn unreadable_file_and_bad_arguments_exit_2() {
    let dir = TempDir::new().unwrap();
    let mut command = Command::cargo_bin("lint4d").unwrap();
    command
        .current_dir(dir.path())
        .args(["digest", "missing.pas"]);
    command
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot read missing.pas"));
    let dir = demo();
    digest(&dir).args(["--format", "xml"]).assert().code(2);
    digest(&dir).args(["--line", "3", "--all"]).assert().code(2);
}

#[test]
fn a_directory_named_digest_is_still_linted_with_a_path_prefix() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("digest")).unwrap();
    fs::write(
        dir.path().join("digest/Clean.pas"),
        "unit Clean;\n\ninterface\n\nimplementation\n\nend.\n",
    )
    .unwrap();
    Command::cargo_bin("lint4d")
        .unwrap()
        .current_dir(dir.path())
        .arg("./digest")
        .assert()
        .code(0);
}

#[test]
fn a_directory_named_help_is_still_linted() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("help")).unwrap();
    fs::write(
        dir.path().join("help/Clean.pas"),
        "unit Clean;\n\ninterface\n\nimplementation\n\nend.\n",
    )
    .unwrap();
    Command::cargo_bin("lint4d")
        .unwrap()
        .current_dir(dir.path())
        .arg("help")
        .assert()
        .code(0);
}

#[test]
fn the_help_flag_still_prints_usage() {
    Command::cargo_bin("lint4d")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("Usage"));
}
