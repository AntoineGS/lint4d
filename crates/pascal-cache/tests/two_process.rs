#![cfg(unix)]

use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, Digest, LocalArtifactKey, Lookup, Miss, Namespace,
    Published, RetireReport, SourceContent, Store, StoreLimits,
};
use pascal_project::{ConditionalContext, SourceOrigin};
use rusqlite::Connection;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};

const ROLE: &str = "PASCAL_CACHE_CHILD_ROLE";
const DIRECTORY: &str = "PASCAL_CACHE_CHILD_DIRECTORY";
const ARTIFACT: &str = "PASCAL_CACHE_CHILD_ARTIFACT";

fn namespace(byte: u8) -> Namespace {
    Namespace::new(Digest::from_bytes([byte; 32]))
}

fn key(name: &str) -> LocalArtifactKey {
    LocalArtifactKey::new(
        &SourceContent::from_bytes(format!("unit {name};").as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    )
}

fn payloads(name: &str) -> Vec<Vec<u8>> {
    (0..300)
        .map(|index| format!("{name} row {index}").into_bytes())
        .collect()
}

fn publish(store: &Store, name: &str) -> Published {
    let key = key(name);
    let payloads = payloads(name);
    let rows: Vec<ArtifactRow<'_>> = payloads
        .iter()
        .map(|payload| ArtifactRow {
            row_kind: 1,
            index_key: Some(name.as_bytes()),
            payload,
        })
        .collect();
    let write = ArtifactWrite {
        key: ArtifactKeyRef::local(&key),
        supplemental: b"",
        rows: &rows,
    };
    store.publish_artifact(&write).unwrap()
}

/// Read and verify every row; `None` on a miss.
fn read(store: &Store, name: &str) -> Option<Vec<Vec<u8>>> {
    let key = key(name);
    let Lookup::Hit(header) = store.open_artifact(ArtifactKeyRef::local(&key)).unwrap() else {
        return None;
    };
    let mut reader = store.read_artifact(&header);
    let mut rows = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => {
                rows.extend(batch.rows().iter().map(|row| row.payload.clone()))
            }
            Lookup::Hit(None) => return Some(rows),
            Lookup::Miss(_) => return None,
        }
    }
}

#[test]
#[ignore = "run by the two-process tests"]
fn child_process_entry() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let directory = std::env::var_os(DIRECTORY).expect("directory");
    let artifact = std::env::var(ARTIFACT).unwrap_or_default();
    let byte = if role == "hold" { 9 } else { 1 };
    let store = Store::open(
        Path::new(&directory),
        &namespace(byte),
        StoreLimits::default(),
    )
    .unwrap();
    match role.to_str().unwrap() {
        "publish" => println!("result {:?}", publish(&store, &artifact)),
        "read" => println!(
            "result {}",
            read(&store, &artifact).map_or(0, |rows| rows.len())
        ),
        "abort-before-commit" => {
            store.set_before_commit_for_test(|| std::process::abort());
            publish(&store, &artifact);
            unreachable!("the hook aborts the process");
        }
        "hold" => {
            println!("ready");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
        }
        other => panic!("unknown role {other}"),
    }
}

fn child(role: &str, directory: &Path, artifact: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "child_process_entry",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE, role)
        .env(DIRECTORY, directory)
        .env(ARTIFACT, artifact);
    command
}

/// The value the child printed after "result ".
fn result(role: &str, directory: &Path, artifact: &str) -> String {
    let output = child(role, directory, artifact).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout
        .lines()
        .find_map(|line| {
            line.split_once("result ")
                .map(|(_, value)| value.to_string())
        })
        .unwrap_or_else(|| panic!("no result in {stdout}"))
}

fn private_directory(root: &Path) -> std::path::PathBuf {
    root.join("cache")
}

#[test]
fn two_processes_share_published_records() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_directory(root.path());
    let store = Store::open(&directory, &namespace(1), StoreLimits::default()).unwrap();
    // Published by another process, read and verified here.
    assert_eq!(result("publish", &directory, "Alpha"), "Inserted");
    assert_eq!(read(&store, "Alpha"), Some(payloads("Alpha")));
    // A duplicate publisher in another process converges on the same record.
    assert_eq!(result("publish", &directory, "Alpha"), "AlreadyPresent");
    // Published here, read and verified by another process.
    assert_eq!(publish(&store, "Beta"), Published::Inserted);
    assert_eq!(result("read", &directory, "Beta"), "300");
}

#[test]
fn a_process_aborted_before_commit_leaves_no_partial_artifact() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_directory(root.path());
    drop(Store::open(&directory, &namespace(1), StoreLimits::default()).unwrap());
    let status = child("abort-before-commit", &directory, "Gamma")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    // Death by SIGABRT proves the child reached the hook.
    assert_eq!(status.signal(), Some(libc::SIGABRT), "{status:?}");
    // A new process recovers the WAL and sees no trace of the aborted write.
    let store = Store::open(&directory, &namespace(1), StoreLimits::default()).unwrap();
    assert_eq!(
        store
            .open_artifact(ArtifactKeyRef::local(&key("Gamma")))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    let rows: i64 = Connection::open(directory.join("analysis.sqlite3"))
        .unwrap()
        .query_row("SELECT count(*) FROM artifact_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(publish(&store, "Gamma"), Published::Inserted);
    assert_eq!(read(&store, "Gamma"), Some(payloads("Gamma")));
}

#[test]
fn a_live_process_keeps_its_namespace_until_it_exits() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_directory(root.path());
    let mut holder = child("hold", &directory, "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(holder.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready") {
        line.clear();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "child exited early"
        );
    }
    let store = Store::open(&directory, &namespace(1), StoreLimits::default()).unwrap();
    let lock = store.location().namespace_lock(namespace(9).id());
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 0,
            in_use: 1
        })
    );
    assert!(lock.exists());
    drop(holder.stdin.take()); // EOF: the child returns and exits
    assert!(holder.wait().unwrap().success());
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 1,
            in_use: 0
        })
    );
    assert!(!lock.exists());
}
