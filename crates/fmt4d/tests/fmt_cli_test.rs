use assert_cmd::Command;
use predicates::prelude::*;

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
