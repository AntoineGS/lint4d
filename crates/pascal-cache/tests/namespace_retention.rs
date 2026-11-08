//! Namespace retention (l4d-1ypi): a store keeps the most recently used
//! namespaces up to `StoreLimits::max_namespaces` while it stays within its
//! disk policy, retires the least recently used first, and never retires a
//! namespace a live process holds.
#![cfg(unix)]

mod common;
use common::private_tempdir;

use pascal_cache::admin::{self, StoreStatus};
use pascal_cache::{
    DATABASE_FILE, Digest, LimitsError, MAX_NAMESPACES, Namespace, ROW_OVERHEAD_BYTES,
    RetireReport, Store, StoreLimits, StorePolicy,
};
use rusqlite::{Connection, params};
use std::path::Path;
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
    pretend_excess(&store, 5 * MIB);
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 1,
            in_use: 0,
            kept: 1,
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
    let StoreStatus::Present(report) = admin::status(dir) else {
        panic!("a present store");
    };
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
    assert_eq!(first.estimated_bytes, 4 * MIB + ROW_OVERHEAD_BYTES);
    assert_eq!(report.namespaces[1].estimated_bytes, 0);
    assert!(!report.namespaces_truncated);
    drop(held);
}
