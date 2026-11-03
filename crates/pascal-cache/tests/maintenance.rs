//! In-process maintenance tests (spec §10, amendment 2026-11-03).
#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{context, open, records, sealed};
use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, Bypass, DATABASE_FILE, LOW_SPACE_MARGIN_BYTES,
    LocalArtifactKey, Lookup, MIN_HEALTH_BACKOFF, Miss, PolicyError, PolicyState, Pressure,
    Published, SourceContent, Store, StorePolicy, Unhealthy,
};
use pascal_project::{ConditionalContext, SourceOrigin};
use std::path::Path;

fn key(text: &str) -> LocalArtifactKey {
    LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    )
}

/// Publish an artifact of `rows` rows of `row_bytes` bytes each.
fn publish_bulk(
    store: &Store,
    name: &str,
    rows: usize,
    row_bytes: usize,
) -> Result<pascal_cache::Published, pascal_cache::Bypass> {
    let key = key(name);
    let payload = vec![7u8; row_bytes];
    let rows: Vec<ArtifactRow<'_>> = (0..rows)
        .map(|_| ArtifactRow {
            row_kind: 1,
            index_key: None,
            payload: &payload,
        })
        .collect();
    store.publish_artifact(&ArtifactWrite {
        key: ArtifactKeyRef::local(&key),
        supplemental: b"",
        rows: &rows,
    })
}

const MIB: u64 = 1 << 20;

fn file_len(directory: &Path, suffix: &str) -> u64 {
    std::fs::metadata(directory.join(format!("{DATABASE_FILE}{suffix}"))).map_or(0, |m| m.len())
}

#[test]
fn commits_never_checkpoint_in_the_writer() {
    let root = private_tempdir();
    let store = open(root.path());
    let main_before = file_len(root.path(), "");
    // About 5 MiB: more than SQLite's default 1000-page autocheckpoint.
    publish_bulk(&store, "unit Bulk;", 6, 900_000).unwrap();
    assert!(
        file_len(root.path(), "-wal") > 4 << 20,
        "the commit is in the WAL"
    );
    assert_eq!(
        file_len(root.path(), ""),
        main_before,
        "no checkpoint copied pages into the database on the commit path"
    );
}

#[test]
fn a_full_disk_makes_writes_back_off_until_a_probe_succeeds() {
    let root = private_tempdir();
    let store = open(root.path());
    store.limit_growth_for_test(4).unwrap();
    assert_eq!(
        publish_bulk(&store, "unit A;", 10, 200_000),
        Err(Bypass::Full)
    );
    assert_eq!(store.health(), Some(Unhealthy::Full));
    store.limit_growth_for_test(1 << 20).unwrap(); // space is back
    assert_eq!(
        publish_bulk(&store, "unit B;", 1, 10),
        Err(Bypass::Full),
        "blocked without touching SQLite until the backoff ends"
    );
    assert_eq!(
        store.open_artifact(ArtifactKeyRef::local(&key("unit A;"))),
        Ok(Lookup::Miss(Miss::Absent)),
        "reads continue"
    );
    store.advance_clock_for_test(MIN_HEALTH_BACKOFF);
    assert_eq!(
        publish_bulk(&store, "unit B;", 1, 10),
        Ok(Published::Inserted)
    );
    assert_eq!(store.health(), None);
}

#[test]
fn corruption_mid_session_bypasses_every_operation_and_leaves_the_files() {
    let root = private_tempdir();
    let store = open(root.path());
    let before = std::fs::read(root.path().join(DATABASE_FILE)).unwrap();
    store.fail_next_for_test(Bypass::Corrupt);
    assert_eq!(store.check_scope(), Err(Bypass::Corrupt));
    assert_eq!(store.check_scope(), Err(Bypass::Corrupt), "blocked");
    assert_eq!(publish_bulk(&store, "unit A;", 1, 10), Err(Bypass::Corrupt));
    assert_eq!(
        std::fs::read(root.path().join(DATABASE_FILE)).unwrap(),
        before
    );
    store.advance_clock_for_test(MIN_HEALTH_BACKOFF);
    assert_eq!(store.check_scope(), Ok(()));
    assert_eq!(store.health(), None);
}

#[test]
fn the_policy_is_read_from_the_store_and_changed_against_its_revision() {
    let root = private_tempdir();
    let store = open(root.path());
    let state = store.policy().unwrap();
    assert_eq!(state.revision, 0);
    assert_eq!(
        state.policy,
        StorePolicy {
            max_disk_bytes: 8 << 30,
            wal_threshold_bytes: 256 << 20
        }
    );
    let small = StorePolicy {
        max_disk_bytes: 64 * MIB,
        wal_threshold_bytes: 8 * MIB,
    };
    assert_eq!(
        store.set_policy(0, small).unwrap(),
        PolicyState {
            revision: 1,
            policy: small
        }
    );
    assert_eq!(
        store.set_policy(0, small),
        Err(PolicyError::RevisionChanged { current: 1 })
    );
}

#[test]
fn a_policy_change_applies_to_the_next_write() {
    let root = private_tempdir();
    let writer = open(root.path());
    let administrator = open(root.path());
    writer.pretend_usage_for_test(60 * MIB);
    assert_eq!(
        publish_bulk(&writer, "unit A;", 1, 10),
        Ok(Published::Inserted)
    );
    administrator
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 * MIB,
                wal_threshold_bytes: 8 * MIB,
            },
        )
        .unwrap();
    assert_eq!(
        publish_bulk(&writer, "unit B;", 1, 10),
        Err(Bypass::OverBudget)
    );
    assert_eq!(writer.pressure(), Ok(Pressure::OverHeadroom));
    assert_eq!(writer.health(), None, "over budget is not unhealthy");
}

#[test]
fn writes_are_refused_without_maintenance_headroom_and_maintenance_is_exempt() {
    let root = private_tempdir();
    let store = open(root.path());
    let header = sealed(&store, &records(&store, "M", 1));
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 * MIB,
                wal_threshold_bytes: 16 * MIB,
            },
        )
        .unwrap();
    store.pretend_usage_for_test(50 * MIB);
    assert_eq!(
        publish_bulk(&store, "unit A;", 1, 10),
        Err(Bypass::OverBudget)
    );
    assert_eq!(
        store.begin_snapshot(&context()).err(),
        Some(Bypass::OverBudget)
    );
    assert_eq!(
        store.retire_snapshot(header.id()),
        Ok(true),
        "retirement frees space"
    );
}

#[test]
fn low_free_space_refuses_writes() {
    let root = private_tempdir();
    let store = open(root.path());
    store.pretend_free_space_for_test(Some(LOW_SPACE_MARGIN_BYTES));
    assert_eq!(
        publish_bulk(&store, "unit A;", 1, 10),
        Err(Bypass::LowDiskSpace)
    );
    assert_eq!(store.pressure(), Ok(Pressure::LowDiskSpace));
}

#[test]
fn the_wal_threshold_stops_writes_until_a_checkpoint_makes_room() {
    let root = private_tempdir();
    let store = open(root.path());
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 * MIB,
                wal_threshold_bytes: 8 * MIB,
            },
        )
        .unwrap();
    assert_eq!(
        publish_bulk(&store, "unit A;", 9, 1_000_000),
        Ok(Published::Inserted)
    );
    assert_eq!(
        publish_bulk(&store, "unit B;", 1, 10),
        Err(Bypass::WalPressure)
    );
    assert_eq!(store.pressure(), Ok(Pressure::WalPressure));
    // Task 8 replaces this with a maintenance checkpoint.
    let _: (i64, i64, i64) = common::records::raw(root.path())
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        publish_bulk(&store, "unit B;", 1, 10),
        Ok(Published::Inserted)
    );
}
