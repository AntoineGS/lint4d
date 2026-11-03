//! In-process maintenance tests (spec §10, amendment 2026-11-03).
#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{SEAL, binding_evidence, context, open, raw, records, sealed};
use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, Bypass, DATABASE_FILE, Digest, FaultPoint, Hint,
    HintStats, LEASE_EXPIRY, LEASE_RENEWAL, LOW_SPACE_MARGIN_BYTES, LocalArtifactKey, Lookup,
    MAX_LEASE_HOLDS, MAX_PENDING_HINTS, MIN_HEALTH_BACKOFF, Miss, PolicyError, PolicyState,
    Pressure, Published, SourceContent, Store, StorePolicy, Unhealthy,
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

fn count(directory: &Path, sql: &str) -> i64 {
    raw(directory).query_row(sql, [], |row| row.get(0)).unwrap()
}

fn lease_row(directory: &Path) -> (i64, i64, i64) {
    raw(directory)
        .query_row(
            "SELECT renewed_unix_seconds, renewal_seq, floor_seq FROM process_leases",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

#[test]
fn every_open_store_holds_a_lease_until_it_closes() {
    let root = private_tempdir();
    let first = open(root.path());
    let second = open(root.path());
    assert_eq!(count(root.path(), "SELECT count(*) FROM process_leases"), 2);
    drop(second);
    assert_eq!(count(root.path(), "SELECT count(*) FROM process_leases"), 1);
    drop(first);
    assert_eq!(count(root.path(), "SELECT count(*) FROM process_leases"), 0);
}

#[test]
fn a_lease_is_renewed_at_most_once_a_minute_inside_writes() {
    let root = private_tempdir();
    let store = open(root.path());
    let opened = lease_row(root.path());
    publish_bulk(&store, "unit A;", 1, 10).unwrap();
    assert_eq!(lease_row(root.path()), opened, "not due: no renewal write");
    store.advance_clock_for_test(LEASE_RENEWAL);
    publish_bulk(&store, "unit B;", 1, 10).unwrap();
    let renewed = lease_row(root.path());
    assert!(renewed.0 >= opened.0 + 60);
    assert_eq!(renewed.2, opened.1, "the floor is the previous renewal");
    assert!(
        renewed.1 > opened.1,
        "unit A's publication is behind the new renewal"
    );
}

#[test]
fn two_renewal_attempts_within_a_minute_leave_the_floor_alone() {
    let root = private_tempdir();
    let store = open(root.path());
    store.advance_clock_for_test(LEASE_RENEWAL);
    store.renew_lease().unwrap();
    let first = lease_row(root.path());
    publish_bulk(&store, "unit A;", 1, 10).unwrap();
    store.renew_lease().unwrap(); // a no-op until due
    publish_bulk(&store, "unit B;", 1, 10).unwrap();
    assert_eq!(lease_row(root.path()), first);
    store.advance_clock_for_test(LEASE_RENEWAL);
    store.renew_lease().unwrap();
    let second = lease_row(root.path());
    assert_eq!(second.2, first.1);
    assert!(second.1 > first.1);
}

#[test]
fn a_build_is_held_until_it_seals_or_is_retired() {
    let root = private_tempdir();
    let store = open(root.path());
    let holds = || count(root.path(), "SELECT count(*) FROM lease_holds");
    let ids = records(&store, "M", 1);
    let mut builder = store.begin_snapshot(&context()).unwrap();
    assert_eq!(holds(), 1);
    store.add_members(&mut builder, &ids).unwrap();
    store.seal_snapshot(builder, &SEAL).unwrap();
    assert_eq!(holds(), 0);
    let builder = store.begin_snapshot(&context()).unwrap();
    assert!(store.retire_build(&builder).unwrap());
    assert_eq!(holds(), 0);
}

#[test]
fn snapshot_holds_are_bounded_per_lease_and_builds_do_not_count() {
    let root = private_tempdir();
    let store = open(root.path());
    let ids = records(&store, "M", 1);
    let headers: Vec<_> = (0..=MAX_LEASE_HOLDS)
        .map(|_| sealed(&store, &ids))
        .collect();
    let _building = store.begin_snapshot(&context()).unwrap();
    for header in &headers[..MAX_LEASE_HOLDS] {
        assert_eq!(store.hold_snapshot(header), Ok(true));
    }
    assert_eq!(store.hold_snapshot(&headers[0]), Ok(true), "already held");
    assert_eq!(
        store.hold_snapshot(&headers[MAX_LEASE_HOLDS]),
        Err(Bypass::TooManyHolds)
    );
    assert_eq!(store.release_snapshot(headers[0].id()), Ok(true));
    assert_eq!(store.release_snapshot(headers[0].id()), Ok(false));
    assert!(
        store
            .retire_snapshot(headers[MAX_LEASE_HOLDS].id())
            .unwrap()
    );
    assert_eq!(
        store.hold_snapshot(&headers[MAX_LEASE_HOLDS]),
        Ok(false),
        "no longer sealed"
    );
}

#[test]
fn retiring_a_snapshot_drops_every_leases_hold() {
    let root = private_tempdir();
    let first = open(root.path());
    let second = open(root.path());
    let header = sealed(&first, &records(&first, "M", 1));
    assert_eq!(first.hold_snapshot(&header), Ok(true));
    assert_eq!(second.hold_snapshot(&header), Ok(true));
    assert!(first.retire_snapshot(header.id()).unwrap());
    assert_eq!(count(root.path(), "SELECT count(*) FROM lease_holds"), 0);
}

#[test]
fn hints_are_coalesced_bounded_and_flushed_at_most_once_a_minute() {
    let root = private_tempdir();
    let store = open(root.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let sequence = || -> i64 {
        raw(root.path())
            .query_row("SELECT publication_seq FROM evidence_sets", [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    let before = sequence();
    store.hint_used(Hint::Evidence(evidence));
    store.hint_used(Hint::Evidence(evidence));
    assert_eq!(
        store.hint_stats(),
        HintStats {
            pending: 1,
            dropped: 0
        }
    );
    assert_eq!(sequence(), before, "a hint writes nothing by itself");
    assert_eq!(store.flush_hints(), Ok(1));
    assert!(sequence() > before);
    store.hint_used(Hint::Evidence(evidence));
    assert_eq!(store.flush_hints(), Ok(0), "rate-limited");
    store.advance_clock_for_test(LEASE_RENEWAL);
    assert_eq!(store.flush_hints(), Ok(1));
    for byte in 0..=MAX_PENDING_HINTS {
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(&(byte as u64).to_le_bytes());
        store.hint_used(Hint::Artifact(Digest::from_bytes(id)));
    }
    assert_eq!(
        store.hint_stats(),
        HintStats {
            pending: MAX_PENDING_HINTS,
            dropped: 1
        }
    );
}

#[test]
fn a_fenced_handle_neither_renews_nor_holds() {
    let root = private_tempdir();
    let store = open(root.path());
    let header = sealed(&store, &records(&store, "M", 1));
    raw(root.path())
        .execute("UPDATE namespaces SET reset_epoch = reset_epoch + 1", [])
        .unwrap();
    store.advance_clock_for_test(LEASE_RENEWAL);
    assert_eq!(store.renew_lease(), Err(Bypass::Fenced));
    assert_eq!(store.hold_snapshot(&header), Err(Bypass::Fenced));
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM process_leases WHERE reset_epoch > 0"
        ),
        0
    );
    assert_eq!(count(root.path(), "SELECT count(*) FROM lease_holds"), 0);
}

#[test]
fn expired_leases_and_their_holds_are_deleted_in_bounded_batches() {
    let root = private_tempdir();
    let dead = open(root.path());
    let live = open(root.path());
    let header = sealed(&dead, &records(&dead, "M", 1));
    assert_eq!(dead.hold_snapshot(&header), Ok(true));
    assert_eq!(live.expire_leases(10), Ok(0), "nothing is expired yet");
    live.advance_clock_for_test(LEASE_EXPIRY + LEASE_RENEWAL);
    live.renew_lease().unwrap();
    // `dead` and `live` both look expired to `live`'s clock except `live`'s own renewal.
    assert_eq!(live.expire_leases(1), Ok(1), "the hold goes first");
    assert_eq!(live.expire_leases(10), Ok(1), "then the lease row");
    assert_eq!(count(root.path(), "SELECT count(*) FROM lease_holds"), 0);
    assert_eq!(count(root.path(), "SELECT count(*) FROM process_leases"), 1);
    // The dead process's handle re-inserts its row on next use.
    assert_eq!(dead.hold_snapshot(&header), Ok(true));
    assert_eq!(count(root.path(), "SELECT count(*) FROM process_leases"), 2);
}

#[test]
fn a_renewal_that_lost_the_race_to_a_write_does_nothing() {
    let root = private_tempdir();
    let store = std::sync::Arc::new(open(root.path()));
    let opened = lease_row(root.path());
    publish_bulk(&store, "unit First;", 1, 10).unwrap();
    store.advance_clock_for_test(LEASE_RENEWAL);
    // Between the "due" decision and the writer, a publisher of the same
    // store renews inside admit_write.
    let racer = store.clone();
    store.set_fault_for_test(FaultPoint::LeaseRenewDecided, move || {
        publish_bulk(&racer, "unit Racer;", 1, 10).unwrap();
    });
    store.renew_lease().unwrap();
    store.clear_fault_for_test(FaultPoint::LeaseRenewDecided);
    let row = lease_row(root.path());
    assert!(row.1 > opened.1, "the publisher renewed");
    assert_eq!(
        row.2, opened.1,
        "renewed once: the floor is still the open-time sequence"
    );
}
