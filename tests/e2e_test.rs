use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/integration")
        .join(name)
}

#[test]
fn e2e_detects_multiple_issues_in_file() {
    Command::cargo_bin("lint4d")
        .unwrap()
        .arg(fixture_path("project/Unit1.pas"))
        .assert()
        .failure()
        .stdout(predicate::str::contains("resource-leak-unprotected"))
        .stdout(predicate::str::contains("empty-except"))
        .stdout(predicate::str::contains("type-prefix"))
        .stdout(predicate::str::contains("with-statement"));
}

#[test]
fn e2e_json_output_is_valid() {
    let output = Command::cargo_bin("lint4d")
        .unwrap()
        .arg("--format")
        .arg("json")
        .arg(fixture_path("project/Unit1.pas"))
        .output()
        .unwrap();

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("JSON output should be valid");
    assert_eq!(json["version"], 1);
    assert!(!json["files"].as_array().unwrap().is_empty());
}

#[test]
fn e2e_fail_on_error_only() {
    Command::cargo_bin("lint4d")
        .unwrap()
        .arg("--fail-on")
        .arg("error")
        .arg(fixture_path("project/Unit1.pas"))
        .assert()
        .failure(); // resource-leak-unprotected is severity error
}
