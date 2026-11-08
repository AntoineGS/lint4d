use pascal_cache::{Bypass, DATABASE_FILE, Namespace, Store, StoreLimits};
use serde_json::Value;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn admin(environment: &Path, arguments: &[&str]) -> (i32, Option<Value>, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(env!("CARGO_BIN_EXE_pascal-lsp"))
        .args(arguments)
        .env("HOME", environment.join("home"))
        .env("XDG_CACHE_HOME", environment.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("run pascal-lsp");
    let stdout = String::from_utf8(stdout).unwrap();
    let json = (!stdout.is_empty()).then(|| {
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
        assert!(stdout.len() <= 4096, "{} bytes", stdout.len());
        serde_json::from_str(stdout.trim_end()).unwrap()
    });
    (
        status.code().unwrap(),
        json,
        String::from_utf8(stderr).unwrap(),
    )
}

fn private_directory(environment: &Path, name: &str) -> PathBuf {
    let directory = environment.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn initialize_policy(environment: &Path, directory: &Path) {
    let (code, json, _) = admin(
        environment,
        &[
            "--cache-directory",
            text(directory),
            "--cache-max-disk-bytes",
            "1073741824",
        ],
    );
    assert_eq!(code, 0, "{json:?}");
}

fn files(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = listing(directory);
    entries.retain(|(name, _)| directory.join(name).is_file());
    entries
}

fn listing(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = if entry.path().is_file() {
                std::fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            (entry.file_name().to_string_lossy().into_owned(), bytes)
        })
        .collect();
    entries.sort();
    entries
}

#[test]
fn status_of_an_absent_default_store_creates_nothing() {
    let environment = tempfile::tempdir().unwrap();
    let (code, json, _) = admin(environment.path(), &["--cache-status"]);
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["store"], "absent");
    assert_eq!(
        json["directory"],
        environment
            .path()
            .join("cache/lint4d/analysis")
            .to_str()
            .unwrap()
    );
    assert!(!environment.path().join("cache").exists());
}

#[test]
fn status_prune_and_reset_of_an_absent_store_succeed_without_creating_it() {
    let environment = tempfile::tempdir().unwrap();
    let directory = environment.path().join("x");
    for mode in ["--cache-status", "--cache-prune", "--cache-reset"] {
        let (code, json, _) = admin(
            environment.path(),
            &[mode, "--cache-directory", text(&directory)],
        );
        assert_eq!(code, 0, "{mode} {json:?}");
        assert!(!directory.exists(), "{mode}");
    }
}

#[test]
fn the_policy_mode_initializes_a_store_and_status_reports_it() {
    let environment = tempfile::tempdir().unwrap();
    let directory = environment.path().join("store");
    let (code, json, _) = admin(
        environment.path(),
        &[
            "--cache-directory",
            text(&directory),
            "--cache-max-disk-bytes",
            "1073741824",
        ],
    );
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["mode"], "policy");
    assert_eq!(json["policy"]["maxDiskBytes"], 1073741824u64);
    assert_eq!(json["policy"]["walThresholdBytes"], 268435456u64);
    let (code, status, _) = admin(
        environment.path(),
        &["--cache-status", "--cache-directory", text(&directory)],
    );
    let status = status.unwrap();
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["store"], "present");
    assert_eq!(status["policy"]["maxDiskBytes"], 1073741824u64);
    assert_eq!(status["policy"]["walThresholdBytes"], 268435456u64);
    assert_eq!(status["container"]["compatible"], true);
    assert_eq!(status["liveLeases"], 0);
    assert_eq!(status["advice"], "none");
}

#[test]
fn an_invalid_policy_is_refused_and_leaves_the_policy_unchanged() {
    let environment = tempfile::tempdir().unwrap();
    let directory = environment.path().join("store");
    initialize_policy(environment.path(), &directory);
    let revision = |environment: &Path| {
        let (code, status, _) = admin(
            environment,
            &["--cache-status", "--cache-directory", text(&directory)],
        );
        assert_eq!(code, 0);
        status.unwrap()["policy"].clone()
    };
    let before = revision(environment.path());
    for (flag, value, kind) in [
        (
            "--cache-wal-threshold-bytes",
            "1024",
            "walThresholdTooSmall",
        ),
        (
            "--cache-max-disk-bytes",
            "33554432",
            "noMaintenanceHeadroom",
        ),
    ] {
        let (code, json, stderr) = admin(
            environment.path(),
            &["--cache-directory", text(&directory), flag, value],
        );
        let json = json.unwrap();
        assert_eq!(code, 1, "{json}");
        assert_eq!(json["error"]["policy"], kind, "{json}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(
            stderr.starts_with(&format!("pascal-lsp: policy change failed: {kind};")),
            "{stderr}"
        );
    }
    assert_eq!(revision(environment.path()), before);
}

#[test]
fn usage_errors_exit_two_with_nothing_on_stdout_and_nothing_created() {
    let environment = tempfile::tempdir().unwrap();
    let directory = environment.path().join("y");
    let directory = text(&directory);
    let cases: &[&[&str]] = &[
        &[
            "--cache-status",
            "--cache-prune",
            "--cache-directory",
            directory,
        ],
        &[
            "--cache-reset",
            "--cache-max-disk-bytes",
            "1",
            "--cache-directory",
            directory,
        ],
        &["--cache-directory", directory],
        &["--cache-status", "--stdio", "--cache-directory", directory],
        &["--cache-status", "--help", "--cache-directory", directory],
        &[
            "--cache-status",
            "--cache-status",
            "--cache-directory",
            directory,
        ],
        &[
            "--cache-max-disk-bytes",
            "0",
            "--cache-directory",
            directory,
        ],
        &[
            "--cache-max-disk-bytes",
            "1k",
            "--cache-directory",
            directory,
        ],
        &["--cache-max-disk-bytes"],
        &["--cache-directory", "relative", "--cache-status"],
        &["--bogus"],
    ];
    for case in cases {
        let (code, json, stderr) = admin(environment.path(), case);
        assert_eq!(code, 2, "{case:?}");
        assert!(json.is_none(), "{case:?}");
        assert!(stderr.contains("--help"), "{case:?}: {stderr}");
    }
    assert!(!Path::new(directory).exists());
}

#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_a_usage_error() {
    use std::os::unix::ffi::OsStrExt;
    let environment = tempfile::tempdir().unwrap();
    let mut directory = environment.path().as_os_str().as_bytes().to_vec();
    directory.extend_from_slice(b"/store\xff");
    let directory = std::ffi::OsStr::from_bytes(&directory);
    let output = Command::new(env!("CARGO_BIN_EXE_pascal-lsp"))
        .arg("--cache-status")
        .arg("--cache-directory")
        .arg(directory)
        .env("HOME", environment.path().join("home"))
        .env("XDG_CACHE_HOME", environment.path().join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("run pascal-lsp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("--help"), "{stderr}");
    assert!(!Path::new(directory).exists());
}

#[test]
fn prune_reports_its_outcome() {
    let environment = tempfile::tempdir().unwrap();
    let directory = environment.path().join("store");
    initialize_policy(environment.path(), &directory);
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-prune", "--cache-directory", text(&directory)],
    );
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["outcome"], "withinBudget", "{json}");
    assert!(json["cycles"].as_u64().unwrap() >= 1);
    assert!(json["before"]["physicalBytes"].is_u64());
    assert!(json["after"]["freeBytes"].is_u64());
}

#[test]
fn reset_fences_a_live_store_without_touching_its_files() {
    let environment = tempfile::tempdir().unwrap();
    let directory = private_directory(environment.path(), "store");
    let store = Store::open(&directory, Namespace::current(), StoreLimits::default()).unwrap();
    let inodes = || -> Vec<u64> {
        [DATABASE_FILE.to_string(), format!("{DATABASE_FILE}-wal")]
            .iter()
            .map(|name| std::fs::metadata(directory.join(name)).unwrap().ino())
            .collect()
    };
    let before = inodes();
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-reset", "--cache-directory", text(&directory)],
    );
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["reset"], true);
    assert_eq!(store.check_scope(), Err(Bypass::Fenced));
    assert_eq!(inodes(), before);
}

#[test]
fn a_corrupt_store_is_reported_and_left_in_place() {
    let environment = tempfile::tempdir().unwrap();
    let directory = private_directory(environment.path(), "store");
    std::fs::write(directory.join(DATABASE_FILE), vec![0x5a; 8192]).unwrap();
    std::fs::set_permissions(
        directory.join(DATABASE_FILE),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = files(&directory);
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-status", "--cache-directory", text(&directory)],
    );
    let json = json.unwrap();
    assert_eq!(code, 1, "{json}");
    assert_eq!(json["store"], "unreadable");
    assert_eq!(json["error"]["bypass"], "corrupt", "{json}");
    assert_eq!(json["error"]["advice"], "freshDirectory");
    for mode in ["--cache-prune", "--cache-reset"] {
        let (code, json, _) = admin(
            environment.path(),
            &[mode, "--cache-directory", text(&directory)],
        );
        assert_eq!(code, 1, "{mode} {json:?}");
    }
    assert_eq!(files(&directory), before);
}

#[test]
fn a_non_private_directory_is_refused_and_left_unchanged() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let environment = tempfile::tempdir().unwrap();
    let directory = private_directory(environment.path(), "store");
    initialize_policy(environment.path(), &directory);
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = listing(&directory);
    let policy = ["--cache-max-disk-bytes", "2147483648"];
    let modes: [&[&str]; 4] = [
        &["--cache-status"],
        &["--cache-prune"],
        &["--cache-reset"],
        &policy,
    ];
    for mode in modes {
        let mut arguments = mode.to_vec();
        arguments.extend(["--cache-directory", text(&directory)]);
        let (code, json, _) = admin(environment.path(), &arguments);
        let json = json.unwrap();
        assert_eq!(code, 1, "{mode:?} {json}");
        assert_eq!(json["error"]["bypass"], "location", "{json}");
        assert_eq!(json["error"]["location"], "notPrivate", "{json}");
        if mode == ["--cache-status"] {
            assert_eq!(json["store"], "unreadable");
        }
    }
    assert_eq!(listing(&directory), before);
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn every_mode_writes_one_bounded_json_line() {
    let environment = tempfile::tempdir().unwrap();
    let mut deep = environment.path().to_path_buf();
    while deep.as_os_str().len() < 3900 {
        deep.push("n".repeat(200));
    }
    let directory = deep.to_str().unwrap().to_owned();
    assert!(directory.len() <= 4096);
    // The deep path may exceed the platform's path limit, so the store is
    // absent or refused; the output bound is what is checked.
    for mode in [
        &["--cache-status"][..],
        &["--cache-prune"],
        &["--cache-reset"],
        &["--cache-max-disk-bytes", "1073741824"],
    ] {
        let mut arguments = mode.to_vec();
        arguments.extend(["--cache-directory", directory.as_str()]);
        let (code, json, _) = admin(environment.path(), &arguments);
        let json = json.unwrap();
        assert!(code == 0 || code == 1, "{mode:?} {json}");
        assert_eq!(json["directoryTruncated"], true, "{json}");
        assert!(json["directory"].as_str().unwrap().len() <= 2048);
    }
    // SQLite's unix VFS limits a database path to about 512 bytes, so a
    // present store can only have a moderately long directory.
    let mut present = environment.path().to_path_buf();
    present.push("m".repeat(200));
    std::fs::create_dir_all(&present).unwrap();
    std::fs::set_permissions(&present, std::fs::Permissions::from_mode(0o700)).unwrap();
    initialize_policy(environment.path(), &present);
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-status", "--cache-directory", text(&present)],
    );
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["store"], "present");
    assert!(json.get("directoryTruncated").is_none());
}

fn other_build(byte: u8) -> Namespace {
    Namespace::new(pascal_cache::Digest::from_bytes([byte; 32]))
}

/// l4d-1ypi: status lists every retained namespace with its last use, size
/// and whether a live process holds it.
#[test]
fn status_lists_the_retained_namespaces() {
    let environment = tempfile::tempdir().unwrap();
    let directory = private_directory(environment.path(), "store");
    drop(Store::open(&directory, &other_build(1), StoreLimits::default()).unwrap());
    let held = Store::open(&directory, Namespace::current(), StoreLimits::default()).unwrap();
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-status", "--cache-directory", text(&directory)],
    );
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["namespaces"]["live"], 2, "{json}");
    let list = json["namespaces"]["list"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{json}");
    let entry = |id: pascal_cache::Digest| {
        list.iter()
            .find(|entry| entry["id"] == id.to_hex())
            .unwrap_or_else(|| panic!("{json}"))
    };
    let current = entry(Namespace::current().id());
    assert_eq!(current["current"], true);
    assert_eq!(current["held"], true);
    assert_eq!(current["resetEpoch"], 0);
    assert!(current["lastUsed"].is_u64(), "{json}");
    assert!(current["estimatedBytes"].is_u64(), "{json}");
    let other = entry(other_build(1).id());
    assert_eq!(other["current"], false);
    assert_eq!(other["held"], false);
    assert_eq!(json["namespaces"]["listTruncated"], false);
    drop(held);
}

#[test]
fn a_status_listing_many_namespaces_stays_within_one_bounded_line() {
    let environment = tempfile::tempdir().unwrap();
    let directory = private_directory(environment.path(), &"s".repeat(200));
    let keep_all = StoreLimits {
        max_namespaces: pascal_cache::MAX_NAMESPACES,
        ..StoreLimits::default()
    };
    for byte in 1..=pascal_cache::MAX_NAMESPACES as u8 {
        drop(Store::open(&directory, &other_build(byte), keep_all).unwrap());
    }
    // `admin` checks the 4 KiB bound.
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-status", "--cache-directory", text(&directory)],
    );
    let json = json.unwrap();
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["namespaces"]["live"], pascal_cache::MAX_NAMESPACES);
    let listed = json["namespaces"]["list"].as_array().unwrap().len();
    assert!(listed >= 8, "{json}");
    assert_eq!(
        json["namespaces"]["listTruncated"],
        listed < pascal_cache::MAX_NAMESPACES,
        "{json}"
    );
}

#[test]
fn prune_keeping_one_namespace_retires_the_other_builds() {
    let environment = tempfile::tempdir().unwrap();
    let directory = private_directory(environment.path(), "store");
    drop(Store::open(&directory, &other_build(1), StoreLimits::default()).unwrap());
    let (code, json, _) = admin(
        environment.path(),
        &["--cache-prune", "--cache-directory", text(&directory)],
    );
    assert_eq!(code, 0, "{json:?}");
    let live = |environment: &Path| {
        let (_, json, _) = admin(
            environment,
            &["--cache-status", "--cache-directory", text(&directory)],
        );
        json.unwrap()["namespaces"]["live"].as_u64().unwrap()
    };
    assert_eq!(live(environment.path()), 2, "the default keeps both builds");
    let (code, json, _) = admin(
        environment.path(),
        &[
            "--cache-prune",
            "--cache-max-namespaces",
            "1",
            "--cache-directory",
            text(&directory),
        ],
    );
    assert_eq!(code, 0, "{json:?}");
    assert_eq!(live(environment.path()), 1);
}

#[test]
fn the_namespace_count_option_is_validated() {
    let environment = tempfile::tempdir().unwrap();
    let directory = environment.path().join("z");
    let directory = text(&directory);
    let cases: &[&[&str]] = &[
        &["--cache-prune", "--cache-max-namespaces", "0"],
        &["--cache-prune", "--cache-max-namespaces", "17"],
        &["--cache-prune", "--cache-max-namespaces", "x"],
        &["--cache-prune", "--cache-max-namespaces"],
        &["--cache-status", "--cache-max-namespaces", "2"],
        &[
            "--cache-reset",
            "--cache-max-namespaces",
            "2",
            "--cache-max-namespaces",
            "2",
        ],
    ];
    for case in cases {
        let mut arguments = case.to_vec();
        arguments.extend(["--cache-directory", directory]);
        let (code, json, stderr) = admin(environment.path(), &arguments);
        assert_eq!(code, 2, "{case:?} {json:?}");
        assert!(stderr.contains("--help"), "{case:?}: {stderr}");
    }
    assert!(!Path::new(directory).exists());
}
