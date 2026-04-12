//! Security regression tests: file-size limits, symlink rejection,
//! parse-error propagation, TOML error surfacing, atomic writes.
//!
//! These tests guard against the P0 findings in `.full-review/`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Default file-size ceiling fmt4d enforces before reading a file.
/// Must match `main.rs::MAX_FILE_SIZE_BYTES`.
const MAX_FILE_SIZE_BYTES: usize = 16 * 1024 * 1024;

#[test]
fn oversized_file_is_rejected_with_clear_message() {
    // Regression guard for SEC-H2 (no file-size limit → OOM on large files).
    // A 32 MB .pas file must be rejected before `fs::read`, not OOM the
    // runner and not produce a zero-exit "success".
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("huge.pas");

    // 32 MB of ASCII '0' padding wrapped in a valid unit shell. We avoid
    // repeating newline-heavy content so the on-disk size is ~32 MB.
    let padding = "0".repeat(32 * 1024 * 1024);
    let source = format!(
        "unit Huge;\ninterface\nconst K = '{}';\nimplementation\nend.\n",
        padding
    );
    assert!(source.len() > MAX_FILE_SIZE_BYTES);
    fs::write(&path, source.as_bytes()).unwrap();

    Command::cargo_bin("fmt4d")
        .unwrap()
        .arg(path.to_str().unwrap())
        .arg("--check")
        .assert()
        .code(predicate::in_iter([0i32, 2i32])) // Either "skipped" (0) or "error" (2) is acceptable
        .stderr(
            predicate::str::contains("huge.pas")
                .and(predicate::str::contains("too large").or(predicate::str::contains("skipp"))),
        );
}

#[cfg(unix)]
#[test]
fn write_mode_does_not_clobber_symlink_target() {
    // Regression guard for SEC-CRIT-1 / SEC-CRIT-2 (symlink + TOCTOU).
    // A .pas-named symlink pointing at an arbitrary file must NOT cause
    // fs::write to follow the symlink and overwrite the target.
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let victim_path = dir.path().join("victim.txt");
    let link_path = dir.path().join("evil.pas");

    fs::write(&victim_path, b"SECRET CONTENT\n").unwrap();
    symlink(&victim_path, &link_path).unwrap();

    // Invoke fmt4d on the directory. Any exit code is acceptable — what
    // matters is that the victim file is not modified.
    let _ = Command::cargo_bin("fmt4d")
        .unwrap()
        .arg(dir.path().to_str().unwrap())
        .assert();

    let after = fs::read(&victim_path).unwrap();
    assert_eq!(
        after, b"SECRET CONTENT\n",
        "symlink target was clobbered via evil.pas"
    );
}
