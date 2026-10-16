#![cfg(unix)]

use pascal_cache::{
    Bypass, Digest, LimitsError, LocationIssue, Namespace, Reclaimed, RetireReport, Store,
    StoreLimits,
};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{Duration, Instant};

/// `tempfile::tempdir` honours the umask; the store requires a private root.
fn private_tempdir() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    root
}

fn namespace(byte: u8) -> Namespace {
    Namespace::new(Digest::from_bytes([byte; 32]))
}

fn open(directory: &Path, byte: u8) -> Store {
    Store::open(directory, &namespace(byte), StoreLimits::default()).unwrap()
}

fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join("analysis.sqlite3")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

fn state(directory: &Path, byte: u8) -> Option<(i64, i64)> {
    raw(directory)
        .query_row(
            "SELECT reset_epoch, state FROM namespaces WHERE namespace_id = ?1",
            [namespace(byte).id().as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
}

#[test]
fn opening_creates_the_store_and_registers_the_namespace() {
    let root = private_tempdir();
    let directory = root.path().join("cache");
    let store = open(&directory, 1);
    assert_eq!(store.scope().namespace, namespace(1).id());
    assert_eq!(store.scope().reset_epoch, 0);
    assert!(directory.join("analysis.sqlite3").is_file());
    assert!(store.location().namespace_lock(namespace(1).id()).is_file());
    assert_eq!(state(&directory, 1), Some((0, 1)));
    assert_eq!(store.check_scope(), Ok(()));
}

#[test]
fn reopening_keeps_the_scope() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    assert_eq!(open(root.path(), 1).scope().reset_epoch, 0);
}

#[test]
fn invalid_limits_are_a_configuration_bypass() {
    let root = private_tempdir();
    let limits = StoreLimits {
        readers: 3,
        ..StoreLimits::default()
    };
    assert_eq!(
        Store::open(root.path(), &namespace(1), limits).err(),
        Some(Bypass::InvalidLimits(LimitsError::Readers))
    );
    assert!(!root.path().join("analysis.sqlite3").exists());
}

#[test]
fn a_namespace_in_use_is_not_retired() {
    let root = private_tempdir();
    let old = open(root.path(), 1);
    let new = open(root.path(), 2);
    assert_eq!(state(root.path(), 1), Some((0, 1)));
    assert_eq!(
        new.retire_obsolete(),
        Ok(RetireReport {
            retired: 0,
            in_use: 1
        })
    );
    assert_eq!(old.check_scope(), Ok(()));
}

#[test]
fn an_unused_namespace_is_retired_and_its_lock_removed() {
    let root = private_tempdir();
    let old = open(root.path(), 1);
    let lock = old.location().namespace_lock(namespace(1).id());
    drop(old);
    let _new = open(root.path(), 2);
    assert_eq!(state(root.path(), 1), Some((0, 2)));
    assert!(!lock.exists());
    let retired: i64 = raw(root.path())
        .query_row("SELECT count(*) FROM retired_scopes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(retired, 1);
}

#[test]
fn a_retired_namespace_reopens_in_a_new_epoch() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    let new = open(root.path(), 2);
    let again = open(root.path(), 1);
    assert_eq!(again.scope().reset_epoch, 1);
    assert_eq!(state(root.path(), 1), Some((1, 1)));
    assert_eq!(
        new.retire_obsolete(),
        Ok(RetireReport {
            retired: 0,
            in_use: 1
        })
    );
}

#[test]
fn a_reset_fences_the_open_store() {
    let root = private_tempdir();
    let store = open(root.path(), 1);
    raw(root.path())
        .execute("UPDATE namespaces SET reset_epoch = reset_epoch + 1", [])
        .unwrap();
    assert_eq!(store.check_scope(), Err(Bypass::Fenced));
}

#[test]
fn a_different_manifest_under_the_same_id_is_an_integrity_failure() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    raw(root.path())
        .execute("UPDATE namespaces SET manifest = x'00'", [])
        .unwrap();
    assert_eq!(
        Store::open(root.path(), &namespace(1), StoreLimits::default()).err(),
        Some(Bypass::Integrity(
            pascal_cache::Integrity::ConflictingManifest
        ))
    );
}

/// Plant `artifacts` artifacts of `rows` rows each in the namespace's epoch 0.
fn plant(directory: &Path, byte: u8, artifacts: u8, rows: i64) {
    plant_at(directory, byte, 0, artifacts, rows);
}

fn plant_at(directory: &Path, byte: u8, epoch: i64, artifacts: u8, rows: i64) {
    let connection = raw(directory);
    let ns = namespace(byte).id();
    for artifact in 0..artifacts {
        let id = [artifact; 32];
        connection
            .execute(
                "INSERT INTO key_manifests VALUES (?1, ?4, ?2, 2, ?3)",
                params![
                    ns.as_bytes().as_slice(),
                    id.as_slice(),
                    b"L4DC\x01\x00\x00\x00\x02".as_slice(),
                    epoch
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO artifacts VALUES (?1, ?5, ?2, 2, NULL, 1, ?3, 0, ?4, x'')",
                params![
                    ns.as_bytes().as_slice(),
                    id.as_slice(),
                    rows,
                    [0u8; 32].as_slice(),
                    epoch
                ],
            )
            .unwrap();
        for ordinal in 0..rows {
            connection
                .execute(
                    "INSERT INTO artifact_rows VALUES (?1, ?5, ?2, ?3, 1, NULL, x'', ?4)",
                    params![
                        ns.as_bytes().as_slice(),
                        id.as_slice(),
                        ordinal,
                        [0u8; 32].as_slice(),
                        epoch
                    ],
                )
                .unwrap();
        }
    }
}

#[test]
fn reclaim_deletes_retired_scopes_in_bounded_batches() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    plant(root.path(), 1, 3, 4); // 12 rows + 3 headers + 3 manifests
    let store = open(root.path(), 2); // retires namespace 1
    let mut total = 0;
    let mut passes = 0;
    loop {
        let Reclaimed { rows, complete } = store.reclaim(5).unwrap();
        assert!(rows <= 5);
        total += rows;
        passes += 1;
        if complete {
            break;
        }
        assert!(passes < 20, "reclamation must progress");
    }
    assert_eq!(total, 18);
    let connection = raw(root.path());
    let count = |table: &str| -> i64 {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(count("artifact_rows"), 0);
    assert_eq!(count("artifacts"), 0);
    assert_eq!(count("key_manifests"), 0);
    assert_eq!(count("retired_scopes"), 0);
    assert_eq!(state(root.path(), 1), None);
    assert_eq!(state(root.path(), 2), Some((0, 1)));
    assert_eq!(
        store.reclaim(5),
        Ok(Reclaimed {
            rows: 0,
            complete: true
        })
    );
}

#[test]
fn reclaim_spares_a_namespace_that_is_live_again_at_a_later_epoch() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    plant(root.path(), 1, 2, 3); // epoch 0: 6 rows + 2 headers + 2 manifests
    drop(open(root.path(), 2)); // retires namespace 1 at epoch 0
    let store = open(root.path(), 1); // live again at epoch 1
    assert_eq!(store.scope().reset_epoch, 1);
    plant_at(root.path(), 1, 1, 1, 2);
    loop {
        if store.reclaim(5).unwrap().complete {
            break;
        }
    }
    let connection = raw(root.path());
    let epochs = |table: &str| -> Vec<i64> {
        let mut statement = connection
            .prepare(&format!(
                "SELECT DISTINCT reset_epoch FROM {table} ORDER BY 1"
            ))
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    for table in ["artifact_rows", "artifacts", "key_manifests"] {
        assert_eq!(epochs(table), vec![1], "{table}");
    }
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM artifact_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2);
    assert_eq!(state(root.path(), 1), Some((1, 1)));
}

#[test]
fn connections_are_bounded() {
    let root = private_tempdir();
    let store = open(root.path(), 1);
    let first = store.hold_reader_for_test().unwrap();
    let second = store.hold_reader_for_test().unwrap();
    let writer = store.hold_writer_for_test().unwrap();
    assert_eq!(store.connection_count_for_test(), 3);
    drop((first, second, writer));
    assert_eq!(
        store.connection_count_for_test(),
        3,
        "connections are reused, not reopened"
    );
}

#[test]
fn pool_waits_end_in_busy_within_the_allowance() {
    let root = private_tempdir();
    let store = open(root.path(), 1);
    let _first = store.hold_reader_for_test().unwrap();
    let second = store.hold_reader_for_test().unwrap();
    let start = Instant::now();
    assert_eq!(store.hold_reader_for_test().err(), Some(Bypass::Busy));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    drop(second);
    assert!(store.hold_reader_for_test().is_ok());
}

#[test]
fn a_held_write_lock_bypasses_maintenance_within_the_allowance() {
    let root = private_tempdir();
    let store = open(root.path(), 1);
    let other = raw(root.path());
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(store.reclaim(10), Err(Bypass::Busy));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    other.execute_batch("ROLLBACK").unwrap();
    assert!(store.reclaim(10).is_ok());
}

#[test]
fn a_symlinked_database_is_bypassed() {
    let root = private_tempdir();
    let directory = root.path().join("cache");
    drop(open(&directory, 1));
    std::fs::rename(directory.join("analysis.sqlite3"), root.path().join("real")).unwrap();
    std::os::unix::fs::symlink(root.path().join("real"), directory.join("analysis.sqlite3"))
        .unwrap();
    assert_eq!(
        Store::open(&directory, &namespace(1), StoreLimits::default()).err(),
        Some(Bypass::Location(LocationIssue::Symlink))
    );
}

#[test]
fn a_symlinked_namespace_lock_is_bypassed() {
    let root = private_tempdir();
    let directory = root.path().join("cache");
    let store = open(&directory, 1);
    let lock = store.location().namespace_lock(namespace(2).id());
    drop(store);
    std::os::unix::fs::symlink(root.path().join("elsewhere"), &lock).unwrap();
    assert_eq!(
        Store::open(&directory, &namespace(2), StoreLimits::default()).err(),
        Some(Bypass::Location(LocationIssue::Symlink))
    );
}

#[test]
fn corrupt_and_unsupported_containers_are_bypassed() {
    let root = private_tempdir();
    let corrupt = root.path().join("corrupt");
    std::fs::create_dir(&corrupt).unwrap();
    std::fs::set_permissions(
        &corrupt,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(corrupt.join("analysis.sqlite3"), vec![0x5a; 8192]).unwrap();
    assert_eq!(
        Store::open(&corrupt, &namespace(1), StoreLimits::default()).err(),
        Some(Bypass::Corrupt)
    );
    let newer = root.path().join("newer");
    drop(open(&newer, 1));
    raw(&newer).pragma_update(None, "user_version", 99).unwrap();
    assert_eq!(
        Store::open(&newer, &namespace(1), StoreLimits::default()).err(),
        Some(Bypass::UnsupportedContainer { version: 99 })
    );
}

#[test]
fn an_unusable_directory_is_bypassed() {
    let root = private_tempdir();
    let file = root.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        Store::open(&file.join("cache"), &namespace(1), StoreLimits::default()),
        Err(Bypass::Io(_))
    ));
}

#[test]
fn the_store_is_shareable_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
}

#[test]
fn retirement_waits_are_cumulative_not_per_transaction() {
    let root = private_tempdir();
    let wait = Duration::from_millis(200);
    let limits = StoreLimits {
        open_wait: wait,
        ..StoreLimits::default()
    };
    let store = Store::open(root.path(), &namespace(1), limits).unwrap();
    let other = raw(root.path());
    for byte in 10..18u8 {
        other
            .execute(
                "INSERT INTO namespaces VALUES (?1, x'00', 0, 1, 0)",
                [[byte; 32].as_slice()],
            )
            .unwrap();
    }
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(store.retire_obsolete(), Err(Bypass::Busy));
    assert!(start.elapsed() < wait * 2, "{:?}", start.elapsed());
    other.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        store.retire_obsolete(),
        Ok(RetireReport {
            retired: 8,
            in_use: 0
        })
    );
}

#[test]
fn a_fresh_connection_waits_within_the_allowance() {
    let root = private_tempdir();
    drop(open(root.path(), 1));
    // Alone on the database, so exclusive locking mode shuts every other
    // connection out, including the header read of a new one.
    let exclusive = raw(root.path());
    exclusive
        .pragma_update(None, "locking_mode", "EXCLUSIVE")
        .unwrap();
    exclusive
        .query_row("SELECT count(*) FROM namespaces", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    let wait = Duration::from_millis(200);
    let limits = StoreLimits {
        open_wait: wait,
        ..StoreLimits::default()
    };
    let start = Instant::now();
    assert_eq!(
        Store::open(root.path(), &namespace(1), limits).err(),
        Some(Bypass::Busy)
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_lock_file_that_cannot_be_removed_does_not_undo_retirement() {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions
    }
    let root = private_tempdir();
    let directory = root.path().join("cache");
    let store = open(&directory, 2);
    drop(open(&directory, 1)); // namespace 2 is held, so it stays live
    let locks = directory.join("namespaces");
    let mode = |mode| {
        std::fs::set_permissions(&locks, std::os::unix::fs::PermissionsExt::from_mode(mode))
            .unwrap()
    };
    mode(0o500);
    let report = store.retire_obsolete();
    mode(0o700);
    assert_eq!(
        report,
        Ok(RetireReport {
            retired: 1,
            in_use: 0
        })
    );
    assert_eq!(state(&directory, 1), Some((0, 2)));
}
