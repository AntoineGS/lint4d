//! Namespace retention (l4d-1ypi): a store keeps the most recently used
//! namespaces up to `StoreLimits::max_namespaces` while it stays within its
//! disk policy, retires the least recently used first, and never retires a
//! namespace a live process holds.
#![cfg(unix)]

mod common;
use common::private_tempdir;

use pascal_cache::admin::{self, StatusReport, StoreStatus};
use pascal_cache::{
    DATABASE_FILE, Digest, FaultPoint, LimitsError, MAX_NAMESPACES, Maintenance, MaintenanceLimits,
    Namespace, ROW_OVERHEAD_BYTES, RetireReport, StepBudget, Store, StoreLimits, StorePolicy,
};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIB: u64 = 1 << 20;

fn namespace(byte: u8) -> Namespace {
    Namespace::new(Digest::from_bytes([byte; 32]))
}

fn keeping(max_namespaces: usize) -> StoreLimits {
    StoreLimits {
        max_namespaces,
        ..StoreLimits::default()
    }
}

fn open(directory: &Path, byte: u8) -> Store {
    Store::open(directory, &namespace(byte), StoreLimits::default()).unwrap()
}

fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

/// `(reset_epoch, state)`; state 1 is live, 2 retired.
fn state(directory: &Path, byte: u8) -> Option<(i64, i64)> {
    raw(directory)
        .query_row(
            "SELECT reset_epoch, state FROM namespaces WHERE namespace_id = ?1",
            [namespace(byte).id().as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
}

fn live(directory: &Path, byte: u8) -> bool {
    state(directory, byte) == Some((0, 1))
}

fn lock(directory: &Path, byte: u8) -> std::path::PathBuf {
    directory
        .join("namespaces")
        .join(format!("{}.lock", namespace(byte).id().to_hex()))
}

/// Record namespace `byte` as last used `at` seconds after a fixed origin.
fn used_at(directory: &Path, byte: u8, at: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(lock(directory, byte))
        .unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + at))
        .unwrap();
}

/// Open, use and close namespace `byte`, recording it as used at `at`.
fn session(directory: &Path, byte: u8, at: u64) {
    drop(open(directory, byte));
    used_at(directory, byte, at);
}

/// Plant one artifact of `content_bytes` in namespace `byte`'s epoch 0.
fn plant(directory: &Path, byte: u8, content_bytes: u64) {
    let connection = raw(directory);
    let ns = namespace(byte).id();
    let id = [0xa0u8; 32];
    connection
        .execute(
            "INSERT INTO key_manifests VALUES (?1, 0, ?2, 2, ?3)",
            params![
                ns.as_bytes().as_slice(),
                id.as_slice(),
                b"L4DC\x01\x00\x00\x00\x02".as_slice()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO artifacts VALUES (NULL, ?1, 0, ?2, 2, NULL, 1, 0, ?3, ?4, x'', 1)",
            params![
                ns.as_bytes().as_slice(),
                id.as_slice(),
                content_bytes as i64,
                [0u8; 32].as_slice()
            ],
        )
        .unwrap();
}

fn artifacts(directory: &Path, byte: u8) -> i64 {
    raw(directory)
        .query_row(
            "SELECT count(*) FROM artifacts WHERE namespace_id = ?1",
            [namespace(byte).id().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn two_builds_alternating_on_one_store_keep_each_others_facts() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    plant(root.path(), 1, 1024);
    drop(open(root.path(), 2));
    let again = open(root.path(), 1);
    assert_eq!(again.scope().reset_epoch, 0, "build 1 was never retired");
    assert!(live(root.path(), 2), "build 2 survives build 1's start");
    assert_eq!(
        artifacts(root.path(), 1),
        1,
        "build 1's facts are still there"
    );
}

#[test]
fn a_store_keeping_one_namespace_retires_every_other_build() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    plant(root.path(), 1, 1024);
    let _new = Store::open(root.path(), &namespace(2), keeping(1)).unwrap();
    assert_eq!(state(root.path(), 1), Some((0, 2)));
    assert!(!lock(root.path(), 1).exists());
}

#[test]
fn the_least_recently_used_namespace_is_retired_first() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    session(dir, 2, 2);
    session(dir, 3, 3);
    assert!(
        live(dir, 1) && live(dir, 2) && live(dir, 3),
        "three are kept"
    );
    // A fourth build: the store keeps it and the two most recently used.
    session(dir, 4, 4);
    assert_eq!(state(dir, 1), Some((0, 2)), "the oldest is retired");
    assert!(live(dir, 2) && live(dir, 3) && live(dir, 4));
    // Build 2 is used again, so build 3 is now the least recently used.
    session(dir, 2, 5);
    assert!(live(dir, 3));
    let _five = open(dir, 5);
    assert_eq!(state(dir, 3), Some((0, 2)), "used before 2 and 4");
    assert!(live(dir, 2) && live(dir, 4) && live(dir, 5));
}

#[test]
fn opening_a_namespace_records_its_use() {
    let root = private_tempdir();
    session(root.path(), 1, 0);
    let before = SystemTime::now() - Duration::from_secs(5);
    let store = open(root.path(), 1);
    let modified = std::fs::metadata(lock(root.path(), 1))
        .unwrap()
        .modified()
        .unwrap();
    assert!(modified >= before, "{modified:?}");
    drop(store);
}

/// The bytes over the default policy's headroom `extra` more would make.
fn pretend_excess(store: &Store, extra: u64) {
    store.pretend_usage_for_test(0);
    let usage = store.usage().unwrap();
    let policy = StorePolicy::default();
    let at_limit = policy.max_disk_bytes - policy.wal_threshold_bytes - usage.physical_bytes()
        + usage.free_bytes();
    store.pretend_usage_for_test(at_limit + extra);
}

/// Run maintenance cycles until one completes, `cycles` times.
fn maintain(store: &Store, cycles: usize) -> usize {
    let mut maintenance = Maintenance::new(MaintenanceLimits::default()).unwrap();
    let never = AtomicBool::new(false);
    let mut rows = 0;
    for _ in 0..cycles {
        loop {
            let report = maintenance
                .step(
                    store,
                    StepBudget {
                        time: Duration::from_secs(5),
                        rows: 4096,
                    },
                    &never,
                )
                .unwrap();
            rows += report.rows;
            if report.cycle_complete {
                break;
            }
        }
    }
    rows
}

#[test]
fn over_its_disk_budget_a_store_retires_the_least_recently_used_namespaces_first() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant(dir, 1, 10 * MIB);
    session(dir, 2, 2);
    plant(dir, 2, 10 * MIB);
    let store = open(dir, 3);
    assert!(
        live(dir, 1) && live(dir, 2),
        "within the budget both are kept"
    );
    // Maintenance estimates both namespaces' sizes (no pressure yet).
    maintain(&store, 1);
    pretend_excess(&store, 5 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 1,
            in_use: 0,
            kept: 1,
            unestimated: 0,
        })
    );
    assert_eq!(state(dir, 1), Some((0, 2)), "the least recently used");
    assert!(live(dir, 2));
    // Build 1's rows are not reclaimed yet, but they count as freed: the
    // next pass retires nothing more.
    assert_eq!(store.retire_obsolete().unwrap().retired, 0);
    assert!(live(dir, 2));
    // Over by more than build 1 frees, build 2 goes too.
    pretend_excess(&store, 15 * MIB);
    assert_eq!(store.retire_obsolete().unwrap().retired, 1);
    assert_eq!(state(dir, 2), Some((0, 2)));
    assert_eq!(store.check_scope(), Ok(()), "the current build stays");
}

#[test]
fn a_held_namespace_among_the_kept_ones_is_skipped_under_pressure() {
    let root = private_tempdir();
    let dir = root.path();
    let held = open(dir, 1);
    used_at(dir, 1, 1);
    plant(dir, 1, 10 * MIB);
    session(dir, 2, 2);
    plant(dir, 2, 10 * MIB);
    let store = open(dir, 3);
    maintain(&store, 1);
    pretend_excess(&store, 5 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 1,
            in_use: 1,
            kept: 0,
            unestimated: 0,
        })
    );
    assert!(live(dir, 1), "held, though least recently used");
    assert_eq!(state(dir, 2), Some((0, 2)));
    drop(held);
}

/// A store over its budget at open sets the planted namespaces' sizes
/// apart: `Store::open` never scans a namespace's rows (review I1), so the
/// pressure part waits for maintenance to estimate them.
#[test]
fn opening_over_budget_leaves_the_pressure_retirement_to_maintenance() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant(dir, 1, 10 * MIB);
    raw(dir)
        .execute(
            "UPDATE store_state SET max_disk_bytes = ?1, wal_threshold_bytes = ?2",
            params![1i64 << 20, (1i64 << 20) - 1],
        )
        .unwrap();
    let store = open(dir, 2);
    assert!(live(dir, 1), "not estimated at open, so not retired there");
    assert_eq!(store.retire_obsolete().unwrap().unestimated, 1);
    maintain(&store, 1);
    // Retired, and reclaimed in the same cycle.
    assert!(!live(dir, 1), "maintenance estimated and retired it");
}

/// Rows of `artifacts` artifacts with no content in namespace `byte`.
fn plant_many(directory: &Path, byte: u8, artifacts: u32) {
    let connection = raw(directory);
    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    let ns = namespace(byte).id();
    for index in 0..artifacts {
        let mut id = [0xb0u8; 32];
        id[28..].copy_from_slice(&index.to_be_bytes());
        connection
            .execute(
                "INSERT INTO artifacts VALUES (NULL, ?1, 0, ?2, 2, NULL, 1, 0, 1, ?3, x'', 1)",
                params![
                    ns.as_bytes().as_slice(),
                    id.as_slice(),
                    [0u8; 32].as_slice()
                ],
            )
            .unwrap();
    }
}

fn status(directory: &Path) -> StatusReport {
    let StoreStatus::Present(report) = admin::status(directory) else {
        panic!("a present store");
    };
    report
}

fn estimate_of(directory: &Path, byte: u8) -> Option<u64> {
    status(directory)
        .namespaces
        .iter()
        .find(|entry| entry.id == namespace(byte).id())
        .and_then(|entry| entry.estimated_bytes)
}

/// Estimation reads in bounded chunks that count against the step's rows,
/// never holds the writer, stops at the cancel flag, resumes across steps,
/// and is remembered: a later cycle reads nothing again.
#[test]
fn namespace_sizes_are_estimated_in_bounded_steps_and_remembered() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant_many(dir, 1, 1000);
    let store = Arc::new(open(dir, 2));
    // While estimation reads, the writer is free.
    let writer_free = Arc::new(AtomicBool::new(true));
    let (weak, free) = (Arc::downgrade(&store), writer_free.clone());
    store.set_fault_for_test(FaultPoint::EstimateChunk, move || {
        if let Some(store) = weak.upgrade()
            && store.hold_writer_for_test().is_err()
        {
            free.store(false, Ordering::Relaxed);
        }
    });
    let mut maintenance = Maintenance::new(MaintenanceLimits::default()).unwrap();
    let cancel = AtomicBool::new(true);
    let budget = StepBudget {
        time: Duration::from_secs(5),
        rows: 100,
    };
    let report = maintenance.step(&store, budget, &cancel).unwrap();
    assert!(report.cancelled && report.rows == 0, "{report:?}");
    let cancel = AtomicBool::new(false);
    let mut steps = 0;
    while estimate_of(dir, 1).is_none() {
        let report = maintenance.step(&store, budget, &cancel).unwrap();
        assert!(report.rows <= 100, "{report:?}");
        steps += 1;
        assert!(steps < 100);
    }
    assert!(steps >= 10, "1000 rows in steps of at most 100: {steps}");
    assert!(writer_free.load(Ordering::Relaxed));
    assert_eq!(estimate_of(dir, 1), Some(1000 * (1 + ROW_OVERHEAD_BYTES)));
    store.clear_fault_for_test(FaultPoint::EstimateChunk);
    let before = maintain(&store, 1);
    assert_eq!(
        maintain(&store, 1),
        0,
        "nothing read again (first cycle {before})"
    );
    // Another process's status and retirement reuse the recorded size.
    drop(store);
    let other = open(dir, 3);
    assert_eq!(estimate_of(dir, 1), Some(1000 * (1 + ROW_OVERHEAD_BYTES)));
    // Only namespace 2, now closed and empty, is estimated: one chunk per
    // table.
    assert_eq!(maintain(&other, 1), 3, "build 1 is not read again");
    drop(other);
}

/// Review M1: a namespace used between the ranking and its retirement is
/// not retired as the least recently used.
#[test]
fn a_namespace_used_after_the_ranking_is_not_retired() {
    let root = private_tempdir();
    let dir = root.path().to_path_buf();
    let holder = open(&dir, 1);
    let store = Store::open(&dir, &namespace(2), keeping(1)).unwrap();
    drop(holder);
    used_at(&dir, 1, 1);
    let reopen = dir.clone();
    store.set_fault_for_test(FaultPoint::NamespacesRanked, move || {
        drop(open(&reopen, 1));
    });
    let report = store.retire_obsolete().unwrap();
    assert_eq!(report.retired, 0, "{report:?}");
    assert!(live(&dir, 1), "used since the ranking");
    store.clear_fault_for_test(FaultPoint::NamespacesRanked);
    assert_eq!(store.retire_obsolete().unwrap().retired, 1);
}

/// Review M2: a long session records its use while it runs, so after a
/// crash it is not ranked as old as its start.
#[test]
fn a_long_session_records_its_use_while_it_runs() {
    let root = private_tempdir();
    let dir = root.path();
    let long = open(dir, 1);
    used_at(dir, 1, 1);
    session(dir, 2, 2);
    long.renew_lease().unwrap();
    assert_eq!(
        status(dir).namespaces[0].id,
        namespace(2).id(),
        "renewed too soon"
    );
    long.advance_clock_for_test(Duration::from_secs(61));
    long.renew_lease().unwrap();
    // Whatever happens to the process now, its last use is recent.
    assert_eq!(status(dir).namespaces[0].id, namespace(1).id());
    drop(long);
}

/// Review M3: a last use in the future counts as now.
#[test]
fn a_last_use_in_the_future_counts_as_now() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(lock(dir, 1))
        .unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(86_400))
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let used = status(dir).namespaces[0].last_used_unix_seconds.unwrap();
    assert!(used <= now + 1, "{used} > {now}");
}

/// Review M7: `held` is what retirement sees, the namespace lock, not the
/// lease (a crashed process's lease stays live for minutes).
#[test]
fn status_reports_held_as_retirement_sees_it() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    raw(dir)
        .execute(
            "INSERT INTO process_leases VALUES (?1, 0, zeroblob(16), ?2, 0, 0)",
            params![
                namespace(1).id().as_bytes().as_slice(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
            ],
        )
        .unwrap();
    let held = open(dir, 2);
    let report = status(dir);
    let entry = |byte: u8| {
        report
            .namespaces
            .iter()
            .find(|entry| entry.id == namespace(byte).id())
            .unwrap()
            .held
    };
    assert!(!entry(1), "a live lease without a holder is not held");
    assert!(entry(2));
    drop(held);
}

/// Review M9: the examined namespaces are the most recently used, not the
/// lowest ids.
#[test]
fn the_most_recently_used_namespaces_are_examined_first() {
    let root = private_tempdir();
    let dir = root.path();
    drop(open(dir, 1));
    let connection = raw(dir);
    for index in 0..300u32 {
        let mut id = [0u8; 32];
        id[..4].copy_from_slice(&index.to_be_bytes());
        connection
            .execute(
                "INSERT INTO namespaces VALUES (?1, x'00', 0, 1, 0)",
                [id.as_slice()],
            )
            .unwrap();
        let path = dir
            .join("namespaces")
            .join(format!("{}.lock", Digest::from_bytes(id).to_hex()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000 + u64::from(index)))
            .unwrap();
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(lock(dir, 1))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_500_000_000))
        .unwrap();
    let mut newest = [0u8; 32];
    newest[..4].copy_from_slice(&299u32.to_be_bytes());
    assert_eq!(status(dir).namespaces[0].id, Digest::from_bytes(newest));
}

#[test]
fn a_held_namespace_is_never_retired() {
    let root = private_tempdir();
    let dir = root.path();
    let held = open(dir, 1);
    plant(dir, 1, 10 * MIB);
    let store = Store::open(dir, &namespace(2), keeping(1)).unwrap();
    assert!(live(dir, 1));
    pretend_excess(&store, 100 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 0,
            in_use: 1,
            kept: 0,
            unestimated: 0,
        })
    );
    assert_eq!(held.check_scope(), Ok(()));
}

#[test]
fn the_namespace_count_is_validated() {
    assert_eq!(StoreLimits::default().max_namespaces, 3);
    assert_eq!(keeping(0).validate(), Err(LimitsError::MaxNamespaces));
    assert_eq!(
        keeping(MAX_NAMESPACES + 1).validate(),
        Err(LimitsError::MaxNamespaces)
    );
    assert_eq!(keeping(1).validate(), Ok(()));
    assert_eq!(keeping(MAX_NAMESPACES).validate(), Ok(()));
}

#[test]
fn status_lists_every_retained_namespace_most_recently_used_first() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant(dir, 1, 4 * MIB);
    session(dir, 2, 2);
    let held = open(dir, 3);
    assert_eq!(estimate_of(dir, 1), None, "not estimated yet");
    maintain(&held, 1);
    let report = status(dir);
    let listed: Vec<_> = report
        .namespaces
        .iter()
        .map(|entry| (entry.id, entry.held))
        .collect();
    assert_eq!(
        listed,
        [
            (namespace(3).id(), true),
            (namespace(2).id(), false),
            (namespace(1).id(), false)
        ]
    );
    let first = &report.namespaces[2];
    assert_eq!(first.last_used_unix_seconds, Some(1_700_000_001));
    assert_eq!(first.reset_epoch, 0);
    assert_eq!(first.estimated_bytes, Some(4 * MIB + ROW_OVERHEAD_BYTES));
    assert_eq!(report.namespaces[1].estimated_bytes, Some(0));
    assert_eq!(
        report.namespaces[0].estimated_bytes, None,
        "the current namespace of a running server is not estimated"
    );
    assert!(!report.namespaces_truncated);
    drop(held);
}
