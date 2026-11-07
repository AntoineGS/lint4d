#![cfg(unix)]

mod common;

use common::records::publish_bulk;
use pascal_cache::admin;
use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, Bypass, DATABASE_FILE, Digest, LocalArtifactKey,
    Lookup, Maintenance, MaintenanceLimits, Miss, Namespace, Pressure, Published, RetireReport,
    SourceContent, StepBudget, Store, StoreLimits, StorePolicy,
};
use pascal_project::{ConditionalContext, SourceOrigin};
use rusqlite::Connection;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const ROLE: &str = "PASCAL_CACHE_CHILD_ROLE";
const DIRECTORY: &str = "PASCAL_CACHE_CHILD_DIRECTORY";
const ARTIFACT: &str = "PASCAL_CACHE_CHILD_ARTIFACT";
/// The namespace every role but "hold" opens.
const SHARED_NAMESPACE: u8 = 1;
/// The namespace the "hold" child keeps live while the parent retires.
const HOLD_NAMESPACE: u8 = 9;

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
    if role == "hold-reader" {
        // A read transaction outside any store, open until stdin closes.
        let connection = Connection::open(Path::new(&directory).join(DATABASE_FILE)).unwrap();
        connection.execute_batch("BEGIN").unwrap();
        let _: i64 = connection
            .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
            .unwrap();
        println!("ready");
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).unwrap() != 0 {}
        connection.execute_batch("COMMIT").unwrap();
        return;
    }
    let byte = if role == "hold" {
        HOLD_NAMESPACE
    } else {
        SHARED_NAMESPACE
    };
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
        "publish-abort-after-commit" => {
            publish(&store, &artifact);
            // No checkpoint, no close: the commit is only in the WAL.
            std::process::abort();
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

/// Spawn `command` and wait until it prints "ready"; the child's stdout
/// stays open with it.
fn spawn_ready(mut command: Command) -> (Child, BufReader<ChildStdout>) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready") {
        line.clear();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "child exited early"
        );
    }
    (child, stdout)
}

fn inode(directory: &Path, suffix: &str) -> u64 {
    std::fs::metadata(directory.join(format!("{DATABASE_FILE}{suffix}")))
        .unwrap()
        .ino()
}

const ROOMY: StepBudget = StepBudget {
    time: Duration::from_secs(60),
    rows: 1000,
};

#[test]
fn two_processes_share_published_records() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_directory(root.path());
    let store = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
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
    drop(
        Store::open(
            &directory,
            &namespace(SHARED_NAMESPACE),
            StoreLimits::default(),
        )
        .unwrap(),
    );
    let status = child("abort-before-commit", &directory, "Gamma")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    // Death by SIGABRT proves the child reached the hook.
    assert_eq!(status.signal(), Some(libc::SIGABRT), "{status:?}");
    // A new process recovers the WAL and sees no trace of the aborted write.
    let store = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
    assert_eq!(
        store
            .open_artifact(ArtifactKeyRef::local(&key("Gamma")))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    let rows: i64 = Connection::open(directory.join(DATABASE_FILE))
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
    let store = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
    let lock = store
        .location()
        .namespace_lock(namespace(HOLD_NAMESPACE).id());
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

#[test]
fn ac3_a_reader_held_in_another_process_stops_writes_at_the_wal_threshold() {
    let root = common::private_tempdir();
    let directory = private_directory(root.path());
    let store = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 << 20,
                wal_threshold_bytes: 8 << 20,
            },
        )
        .unwrap();
    let (mut holder, _stdout) = spawn_ready(child("hold-reader", &directory, ""));
    assert_eq!(
        publish_bulk(&store, "unit Bulk;", 9, 1_000_000),
        Ok(Published::Inserted)
    );
    let checkpoint = store.checkpoint().unwrap();
    assert!(
        !checkpoint.complete() && !checkpoint.truncated,
        "{checkpoint:?}"
    );
    assert_eq!(
        publish_bulk(&store, "unit Next;", 1, 10),
        Err(Bypass::WalPressure)
    );
    let report = Maintenance::new(MaintenanceLimits::default())
        .unwrap()
        .step(
            &store,
            StepBudget {
                time: Duration::from_secs(60),
                rows: 100,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((report.pressure, report.rows), (Pressure::WalPressure, 0));
    drop(holder.stdin.take());
    assert!(holder.wait().unwrap().success());
    assert!(store.checkpoint().unwrap().truncated);
    assert_eq!(
        publish_bulk(&store, "unit Next;", 1, 10),
        Ok(Published::Inserted)
    );
}

#[test]
fn ac3_a_commit_survives_death_before_any_checkpoint() {
    let root = common::private_tempdir();
    let directory = private_directory(root.path());
    drop(
        Store::open(
            &directory,
            &namespace(SHARED_NAMESPACE),
            StoreLimits::default(),
        )
        .unwrap(),
    );
    let status = child("publish-abort-after-commit", &directory, "Delta")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGABRT), "{status:?}");
    let wal = std::fs::metadata(directory.join(format!("{DATABASE_FILE}-wal")))
        .unwrap()
        .len();
    assert!(wal > 0, "the commit is only in the WAL");
    let store = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
    assert_eq!(read(&store, "Delta"), Some(payloads("Delta")));
}

/// Spec §12.1: a reset or prune while another process holds a reader.
#[test]
fn a_reset_and_a_prune_neither_wait_on_nor_invalidate_a_reader_in_another_process() {
    let root = common::private_tempdir();
    let directory = private_directory(root.path());
    let other = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
    assert_eq!(publish(&other, "Alpha"), Published::Inserted);
    let (mut holder, _stdout) = spawn_ready(child("hold-reader", &directory, ""));
    let files = (inode(&directory, ""), inode(&directory, "-wal"));

    assert_eq!(
        admin::reset(
            &directory,
            &namespace(SHARED_NAMESPACE),
            StoreLimits::default()
        ),
        Ok(true)
    );
    assert_eq!(other.check_scope(), Err(Bypass::Fenced), "reset fences");
    let pruned = admin::prune(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
        ROOMY,
    )
    .unwrap()
    .unwrap();
    assert!(pruned.cycle_complete && !pruned.skipped, "{pruned:?}");
    let store = Store::open(
        &directory,
        &namespace(SHARED_NAMESPACE),
        StoreLimits::default(),
    )
    .unwrap();
    assert_eq!(store.scope().reset_epoch, 1);
    assert_eq!(read(&store, "Alpha"), None, "the reset namespace is empty");
    let checkpoint = store.checkpoint().unwrap();
    // (`truncated` only says the WAL is under the journal limit, which
    // this small WAL always is.)
    assert!(
        !checkpoint.complete(),
        "the reader holds the checkpoint back: {checkpoint:?}"
    );
    assert_eq!(
        (inode(&directory, ""), inode(&directory, "-wal")),
        files,
        "nothing unlinked or renamed"
    );

    drop(holder.stdin.take());
    assert!(
        holder.wait().unwrap().success(),
        "the reader's transaction commits"
    );
    let report = Maintenance::new(MaintenanceLimits::default())
        .unwrap()
        .step(&store, ROOMY, &AtomicBool::new(false))
        .unwrap();
    assert!(report.cycle_complete, "{report:?}");
    assert!(store.checkpoint().unwrap().complete());
    assert_eq!((inode(&directory, ""), inode(&directory, "-wal")), files);
}
