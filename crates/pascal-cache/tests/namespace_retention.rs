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
    Namespace, RetireReport, StepBudget, Store, StoreLimits, StorePolicy,
};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// The retention scenarios below keep three namespaces, one more than the
/// default (l4d-i2tj), so that retirement order shows.
fn open(directory: &Path, byte: u8) -> Store {
    Store::open(directory, &namespace(byte), keeping(3)).unwrap()
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

/// l4d-i2tj: by default the store keeps the opening build's namespace and
/// the most recently used other only.
#[test]
fn the_default_keeps_the_current_and_the_previous_build() {
    let root = private_tempdir();
    let dir = root.path();
    let default_session = |byte: u8, at: u64| {
        drop(Store::open(dir, &namespace(byte), StoreLimits::default()).unwrap());
        used_at(dir, byte, at);
    };
    default_session(1, 1);
    default_session(2, 2);
    assert!(
        live(dir, 1) && live(dir, 2),
        "current and previous are kept"
    );
    default_session(3, 3);
    assert_eq!(state(dir, 1), Some((0, 2)), "the oldest build is retired");
    assert!(live(dir, 2) && live(dir, 3));
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

/// Record `bytes` as namespace `byte`'s current estimate: its epoch 0 and
/// its lock file's time, as maintenance writes it.
fn record(directory: &Path, byte: u8, bytes: u64) {
    let used = std::fs::metadata(lock(directory, byte))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::fs::write(
        directory
            .join("namespaces")
            .join(format!("{}.size", namespace(byte).id().to_hex())),
        format!("l4d-size 1 0 {used} {bytes}\n"),
    )
    .unwrap();
}

/// Review M-r2-2: under pressure the least recently used kept namespace is
/// retired at once, its size unknown, and the pass stops there.
#[test]
fn over_its_disk_budget_a_store_retires_the_least_recently_used_namespace_at_once() {
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
    pretend_excess(&store, 100 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 1,
            in_use: 0,
            kept: 1,
            unknown_size: 1,
        })
    );
    assert!(!live(dir, 1), "the least recently used");
    assert!(live(dir, 2), "a size unknown stops the pass");
    assert_eq!(store.check_scope(), Ok(()), "the current build stays");
}

/// Review I-r2-2: the estimate of a retired scope not yet reclaimed counts
/// as freed space.
#[test]
fn a_retired_scope_still_to_reclaim_counts_as_freed() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    record(dir, 1, 10 * MIB);
    session(dir, 2, 2);
    let store = open(dir, 3);
    pretend_excess(&store, 5 * MIB);
    assert_eq!(store.retire_obsolete().unwrap().retired, 1);
    assert!(!live(dir, 1) && live(dir, 2));
    // Its record stays until the scope is reclaimed, for other processes.
    let size = dir
        .join("namespaces")
        .join(format!("{}.size", namespace(1).id().to_hex()));
    assert!(size.exists());
    let other = open(dir, 4);
    other.pretend_usage_for_test(0);
    pretend_excess(&other, 5 * MIB);
    assert_eq!(other.retire_obsolete().unwrap().retired, 0, "known pending");
    drop(other);
    let again = store.retire_obsolete().unwrap();
    assert_eq!((again.retired, again.unknown_size), (0, 0), "{again:?}");
    // Over by more than build 1 frees, build 2 goes too, size unknown.
    pretend_excess(&store, 15 * MIB);
    let more = store.retire_obsolete().unwrap();
    assert_eq!((more.retired, more.unknown_size), (1, 1), "{more:?}");
    assert!(!live(dir, 2));
    reclaim_all(&store);
    assert!(!size.exists(), "removed with its scope");
}

/// Review I-r2-2: current estimates let one pass retire several kept
/// namespaces, until they cover the excess.
#[test]
fn current_estimates_let_one_pass_retire_until_the_excess_is_covered() {
    let root = private_tempdir();
    let dir = root.path();
    for byte in 1..=3 {
        session(dir, byte, u64::from(byte));
        record(dir, byte, 10 * MIB);
    }
    let store = Store::open(dir, &namespace(4), keeping(4)).unwrap();
    pretend_excess(&store, 15 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 2,
            in_use: 0,
            kept: 1,
            unknown_size: 0,
        })
    );
    assert!(!live(dir, 1) && !live(dir, 2));
    assert!(live(dir, 3), "builds 1 and 2 cover the excess");
}

/// A known size retired by the count rule counts against the excess too:
/// the pressure then keeps the kept namespace.
#[test]
fn a_count_retirement_of_known_size_counts_against_the_excess() {
    let root = private_tempdir();
    let dir = root.path();
    // Both held while build 3 opens, so its open retires neither.
    let holders = (open(dir, 1), open(dir, 2));
    let store = Store::open(dir, &namespace(3), keeping(2)).unwrap();
    drop(holders);
    for byte in 1..=2 {
        used_at(dir, byte, u64::from(byte));
        record(dir, byte, 10 * MIB);
    }
    pretend_excess(&store, 5 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 1,
            in_use: 0,
            kept: 1,
            unknown_size: 0,
        })
    );
    assert!(!live(dir, 1), "beyond the count");
    assert!(live(dir, 2), "build 1's size covers the excess");
}

/// Review I-r2-1: a record whose lock-file time no longer matches is not a
/// size: retiring with it stops the pass and frees nothing on paper.
#[test]
fn a_stale_record_is_an_unknown_size() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    record(dir, 1, 100 * MIB);
    used_at(dir, 1, 2);
    session(dir, 2, 3);
    let store = open(dir, 4);
    pretend_excess(&store, 5 * MIB);
    let first = store.retire_obsolete().unwrap();
    assert_eq!((first.retired, first.unknown_size), (1, 1), "{first:?}");
    assert!(live(dir, 2), "the pass stopped");
    // Review I-r3-1: until build 1 is reclaimed, its unknown size stops
    // every pass.
    assert_eq!(store.retire_obsolete().unwrap().retired, 0);
    assert!(live(dir, 2));
    reclaim_all(&store);
    assert_eq!(store.retire_obsolete().unwrap().retired, 1);
    assert!(!live(dir, 2));
}

fn reclaim_all(store: &Store) {
    while !store.reclaim(4096).unwrap().complete {}
}

/// Review I-r3-1: a store over its budget at open retired one kept
/// namespace there; later passes wait until reclamation has run.
#[test]
fn passes_after_an_unknown_size_retirement_wait_for_its_reclamation() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant(dir, 1, 10 * MIB);
    session(dir, 2, 2);
    plant(dir, 2, 10 * MIB);
    raw(dir)
        .execute(
            "UPDATE store_state SET max_disk_bytes = ?1, wal_threshold_bytes = ?2",
            params![1i64 << 20, (1i64 << 20) - 1],
        )
        .unwrap();
    let store = Arc::new(open(dir, 3));
    assert!(!live(dir, 1) && live(dir, 2));
    assert_eq!(store.retire_obsolete().unwrap().retired, 0);
    assert!(live(dir, 2), "build 1 is not reclaimed yet");
    // Maintenance: build 2 stays until the Delete phase has run.
    let retired_before_delete = Arc::new(AtomicBool::new(false));
    let (flag, seen) = (retired_before_delete.clone(), dir.to_path_buf());
    let first_delete = Arc::new(AtomicBool::new(true));
    store.set_fault_for_test(FaultPoint::GcDeleteBeforeCommit, move || {
        if first_delete.swap(false, Ordering::Relaxed) && !live(&seen, 2) {
            flag.store(true, Ordering::Relaxed);
        }
    });
    maintain(&store, 1);
    assert!(!retired_before_delete.load(Ordering::Relaxed));
    // Once build 1 is reclaimed the store, still over budget, goes on.
    let mut cycles = 0;
    while live(dir, 2) {
        maintain(&store, 1);
        cycles += 1;
        assert!(cycles < 10, "build 2 is retired once build 1 is reclaimed");
    }
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
    pretend_excess(&store, 5 * MIB);
    maintain(&store, 1);
    assert!(live(dir, 1), "held, though least recently used");
    assert!(!live(dir, 2));
    drop(held);
}

/// A store over its budget at open retires its least recently used kept
/// namespace there, without reading any namespace's rows (reviews I1,
/// M-r2-2).
#[test]
fn opening_over_budget_retires_the_least_recently_used_namespace_unread() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant(dir, 1, 10 * MIB);
    session(dir, 2, 2);
    raw(dir)
        .execute(
            "UPDATE store_state SET max_disk_bytes = ?1, wal_threshold_bytes = ?2",
            params![1i64 << 20, (1i64 << 20) - 1],
        )
        .unwrap();
    let _store = open(dir, 3);
    assert!(!live(dir, 1), "retired at open");
    assert!(live(dir, 2), "its size unknown, the pass stopped");
    assert_eq!(estimate_of(dir, 1), None);
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

/// Delete-phase transactions of `store`, counted by a fault hook.
fn count_deletes(store: &Store) -> Arc<AtomicUsize> {
    let deletes = Arc::new(AtomicUsize::new(0));
    let counter = deletes.clone();
    store.set_fault_for_test(FaultPoint::GcDeleteBeforeCommit, move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    deletes
}

/// After a pass stopped at a retirement of unknown size, maintenance
/// estimates the next kept namespace in chunks bounded by the step's rows,
/// never on the writer, stopping at the cancel flag, and at most its cap
/// per cycle, so reclamation runs in the same cycle; the next pass does
/// not wait for the estimate (reviews I-new-1, M-r2-2).
#[test]
fn an_estimate_never_holds_back_retirement_or_reclamation() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    session(dir, 2, 2);
    plant_many(dir, 2, 5000);
    let store = Arc::new(open(dir, 3));
    pretend_excess(&store, 1024);
    let writer_free = Arc::new(AtomicBool::new(true));
    let chunks = Arc::new(AtomicUsize::new(0));
    let (weak, free, counter) = (Arc::downgrade(&store), writer_free.clone(), chunks.clone());
    store.set_fault_for_test(FaultPoint::EstimateChunk, move || {
        counter.fetch_add(1, Ordering::Relaxed);
        if let Some(store) = weak.upgrade()
            && store.hold_writer_for_test().is_err()
        {
            free.store(false, Ordering::Relaxed);
        }
    });
    let deletes = count_deletes(&store);
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
    loop {
        let report = maintenance.step(&store, budget, &cancel).unwrap();
        assert!(report.rows <= 100, "{report:?}");
        steps += 1;
        if report.cycle_complete {
            break;
        }
        assert!(steps < 200);
    }
    assert!(!live(dir, 1), "retired by the first pass");
    assert!(live(dir, 2), "its size unknown, the first pass stopped");
    assert!(steps >= 40, "4096 entries in steps of at most 100: {steps}");
    assert!(chunks.load(Ordering::Relaxed) >= 40);
    assert!(
        deletes.load(Ordering::Relaxed) > 0,
        "reclamation in the same cycle"
    );
    assert!(writer_free.load(Ordering::Relaxed));
    // The next cycle's pass retires build 2 without its estimate.
    maintain(&store, 1);
    assert!(!live(dir, 2));
}

/// Review I-new-1: without pressure no estimate is read.
#[test]
fn no_estimate_is_read_without_pressure() {
    let root = private_tempdir();
    let dir = root.path();
    session(dir, 1, 1);
    plant_many(dir, 1, 500);
    let store = open(dir, 2);
    let chunks = Arc::new(AtomicUsize::new(0));
    let counter = chunks.clone();
    store.set_fault_for_test(FaultPoint::EstimateChunk, move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(maintain(&store, 2), 0);
    assert_eq!(chunks.load(Ordering::Relaxed), 0);
    assert_eq!(estimate_of(dir, 1), None);
}

/// Review I-new-1: in a low-space emergency, a kept namespace reopened
/// during every scan is retired anyway after a few tries (its size counted
/// as unknown), and reclamation runs meanwhile.
#[test]
fn a_candidate_reused_during_its_scans_is_retired_without_stalling() {
    let root = private_tempdir();
    let dir = root.path().to_path_buf();
    session(&dir, 1, 1);
    plant_many(&dir, 1, 1000);
    let store = open(&dir, 2);
    let reused = dir.clone();
    store.set_fault_for_test(FaultPoint::EstimateChunk, move || {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(lock(&reused, 1))
            .unwrap();
        file.set_modified(SystemTime::now()).unwrap();
    });
    let deletes = count_deletes(&store);
    store.pretend_free_space_for_test(Some(pascal_cache::LOW_SPACE_MARGIN_BYTES - MIB));
    let mut maintenance = Maintenance::new(MaintenanceLimits::default()).unwrap();
    let never = AtomicBool::new(false);
    let budget = StepBudget {
        time: Duration::from_secs(5),
        rows: 100,
    };
    let mut steps = 0;
    while artifacts(&dir, 1) > 0 {
        maintenance.step(&store, budget, &never).unwrap();
        steps += 1;
        assert!(steps < 200, "stalled: {steps} steps");
    }
    assert!(!live(&dir, 1));
    assert!(
        deletes.load(Ordering::Relaxed) > 1,
        "reclamation ran every cycle"
    );
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
            unknown_size: 0,
        })
    );
    assert_eq!(held.check_scope(), Ok(()));
}

#[test]
fn the_namespace_count_is_validated() {
    assert_eq!(StoreLimits::default().max_namespaces, 2);
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
    assert!(
        report
            .namespaces
            .iter()
            .all(|entry| entry.estimated_bytes.is_none()),
        "without pressure nothing is estimated"
    );
    assert!(!report.namespaces_truncated);
    drop(held);
}

/// A store keeping only its own namespace, with build 1 unheld and live
/// beside it.
fn alone_beside_build_one(dir: &Path) -> Store {
    let holder = open(dir, 1);
    let store = Store::open(dir, &namespace(2), keeping(1)).unwrap();
    drop(holder);
    store
}

/// Review M-new-1: a namespace ranked without a lock file that a process
/// opens and closes before its retirement is not retired, and its new lock
/// file stays.
#[test]
fn a_namespace_ranked_without_a_lock_file_and_then_used_is_not_retired() {
    let root = private_tempdir();
    let dir = root.path().to_path_buf();
    let store = alone_beside_build_one(&dir);
    std::fs::remove_file(lock(&dir, 1)).unwrap();
    let reopen = dir.clone();
    store.set_fault_for_test(FaultPoint::NamespacesRanked, move || {
        // Ranked without a lock file; now a process opens and closes it.
        drop(open(&reopen, 1));
    });
    let report = store.retire_obsolete().unwrap();
    assert_eq!(report.retired, 0, "{report:?}");
    assert!(live(&dir, 1));
    assert!(lock(&dir, 1).exists());
}

/// Review M-new-1: a pass that finds its candidate no longer live at the
/// ranked epoch (another process moved it on) leaves its lock file.
#[test]
fn a_candidate_gone_from_its_epoch_keeps_its_lock_file() {
    let root = private_tempdir();
    let dir = root.path().to_path_buf();
    let store = alone_beside_build_one(&dir);
    let moved = dir.clone();
    store.set_fault_for_test(FaultPoint::NamespacesRanked, move || {
        raw(&moved)
            .execute(
                "UPDATE namespaces SET reset_epoch = 1 WHERE namespace_id = ?1",
                [namespace(1).id().as_bytes().as_slice()],
            )
            .unwrap();
    });
    let report = store.retire_obsolete().unwrap();
    assert_eq!(report.retired, 0, "{report:?}");
    assert!(lock(&dir, 1).exists(), "its use stays recorded");
    assert_eq!(state(&dir, 1), Some((1, 1)));
}

/// Review M-new-4: opening the store removes record temporaries a crashed
/// writer left, older than a few minutes, and keeps recent ones.
#[test]
fn opening_removes_stale_record_temporaries() {
    let root = private_tempdir();
    let dir = root.path();
    drop(open(dir, 1));
    let locks = dir.join("namespaces");
    let stale = locks.join(format!("{}.size.00ff.tmp", namespace(1).id().to_hex()));
    let fresh = locks.join(format!("{}.size.0100.tmp", namespace(1).id().to_hex()));
    for path in [&stale, &fresh] {
        std::fs::write(path, b"l4d-size 1 0 - 1\n").unwrap();
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    drop(open(dir, 1));
    assert!(!stale.exists());
    assert!(fresh.exists());
    assert!(lock(dir, 1).exists());
}
