//! In-process maintenance tests (spec §10, amendment 2026-11-03).
#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{
    EAGER, SEAL, age_past_grace, artifact, binding_evidence, check_invariant, context,
    local_member, member, open, publish_bulk, raw, read_members, read_record, records, sealed,
};
use pascal_cache::admin::{self, RecoveryAdvice, StoreStatus};
use pascal_cache::{
    ArtifactKeyRef, Bypass, DATABASE_FILE, Digest, DiscoveryInputs, DiscoveryScope, EvidenceTarget,
    EvidenceWrite, ExpandedArtifactKey, FaultPoint, Hint, HintStats, JOURNAL_SIZE_LIMIT_BYTES,
    LEASE_EXPIRY, LEASE_RENEWAL, LOW_SPACE_MARGIN_BYTES, LocalArtifactKey, Lookup, MAX_LEASE_HOLDS,
    MAX_PENDING_HINTS, MIN_HEALTH_BACKOFF, Maintenance, MaintenanceLimits, MaintenanceLimitsError,
    Miss, Observation, Phase, PolicyError, PolicyState, Pressure, Published, Reclaimed, RetireStep,
    ScannedDirectory, SnapshotSeal, SourceContent, StepBudget, Store, StorePolicy, Unhealthy,
};
use pascal_cache::{Namespace, StoreLimits};
use pascal_core::facts::ArtifactFacts;
use pascal_project::{ConditionalContext, SourceOrigin};
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn key(text: &str) -> LocalArtifactKey {
    LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    )
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
    assert!(store.checkpoint().unwrap().truncated);
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

fn lifecycle(directory: &Path, table: &str, column: &str, id: &[u8]) -> i64 {
    raw(directory)
        .query_row(
            &format!("SELECT lifecycle FROM {table} WHERE {column} = ?1"),
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn retire_all(store: &Store, limits: &MaintenanceLimits) -> usize {
    RetireStep::ALL
        .iter()
        .map(|step| store.retire_step(*step, limits).unwrap())
        .sum()
}

const SMALL_POLICY: StorePolicy = StorePolicy {
    max_disk_bytes: 64 * MIB,
    wal_threshold_bytes: 16 * MIB,
};

#[test]
fn retirement_never_breaks_the_reachability_invariant() {
    let root = private_tempdir();
    let store = open(root.path());
    let used = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let kept = store
        .publish_member_record(&local_member(
            &store,
            "/w/Main.pas",
            "unit Main;",
            "/w/Shapes.pas",
            used,
        ))
        .unwrap()
        .record_id;
    let snapshot = sealed(&store, &[kept]);
    let stray_evidence = binding_evidence(&store, "other", "/w/Other.pas");
    let stray_artifact = artifact(&store, "unit Stray;");
    let stray_record = records(&store, "Stray", 1)[0];
    age_past_grace(&store);
    assert_eq!(retire_all(&store, &EAGER), 3);
    assert_eq!(check_invariant(root.path()), 0);
    let dir = root.path();
    let record_of = |id: Digest| lifecycle(dir, "member_records", "record_id", id.as_bytes());
    let evidence_of = |id: Digest| lifecycle(dir, "evidence_sets", "evidence_id", id.as_bytes());
    assert_eq!(record_of(stray_record), 2);
    assert_eq!(evidence_of(stray_evidence), 2);
    assert_eq!(
        lifecycle(dir, "artifacts", "artifact_id", stray_artifact.as_bytes()),
        2
    );
    assert_eq!(record_of(kept), 1);
    assert_eq!(evidence_of(used), 1);
    assert_eq!(
        read_members(&store, &snapshot).map(|m| m.len()),
        Ok(1),
        "still readable"
    );
    // Once nothing lists it, the record goes, then what only it referenced.
    assert!(store.retire_snapshot(snapshot.id()).unwrap());
    assert_eq!(retire_all(&store, &EAGER), 3);
    assert_eq!(check_invariant(dir), 0);
    assert_eq!(record_of(kept), 2);
    assert_eq!(evidence_of(used), 2);
}

#[test]
fn rows_published_since_the_last_renewal_are_not_collected() {
    let root = private_tempdir();
    let store = open(root.path());
    artifact(&store, "unit Fresh;");
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(0));
    store.advance_clock_for_test(LEASE_RENEWAL);
    store.renew_lease().unwrap();
    assert_eq!(
        store.retire_step(RetireStep::Artifacts, &EAGER),
        Ok(0),
        "the floor is still behind it"
    );
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
}

#[test]
fn another_live_lease_holds_the_horizon_back() {
    let root = private_tempdir();
    let gc = open(root.path());
    let other = open(root.path());
    artifact(&other, "unit Theirs;");
    age_past_grace(&gc);
    assert_eq!(
        gc.retire_step(RetireStep::Artifacts, &EAGER),
        Ok(0),
        "the other process's floor"
    );
    gc.advance_clock_for_test(LEASE_EXPIRY);
    gc.renew_lease().unwrap();
    assert_eq!(
        gc.retire_step(RetireStep::Artifacts, &EAGER),
        Ok(1),
        "its lease expired"
    );
}

#[test]
fn the_recency_window_keeps_reused_facts_unless_the_store_is_under_pressure() {
    let root = private_tempdir();
    let store = open(root.path());
    artifact(&store, "unit Reused;");
    age_past_grace(&store);
    let limits = MaintenanceLimits::default();
    assert_eq!(store.retire_step(RetireStep::Artifacts, &limits), Ok(0));
    store.set_policy(0, SMALL_POLICY).unwrap();
    store.pretend_usage_for_test(60 * MIB);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &limits), Ok(1));
}

#[test]
fn abandoned_builds_are_retired_and_held_ones_kept() {
    let root = private_tempdir();
    let builder_store = open(root.path());
    let gc = open(root.path());
    let ids = records(&builder_store, "M", 1);
    let mut builder = builder_store.begin_snapshot(&context()).unwrap();
    assert_eq!(
        gc.retire_step(RetireStep::AbandonedBuilds, &EAGER),
        Ok(0),
        "held by a live lease"
    );
    // Past expiry: a lease renewed exactly LEASE_EXPIRY ago is still live.
    gc.advance_clock_for_test(LEASE_EXPIRY + LEASE_RENEWAL);
    assert_eq!(gc.retire_step(RetireStep::AbandonedBuilds, &EAGER), Ok(1));
    assert_eq!(
        builder_store.add_members(&mut builder, &ids),
        Err(Bypass::BuilderFenced)
    );
    assert_eq!(check_invariant(root.path()), 0);
}

#[test]
fn a_held_snapshot_and_the_newest_of_each_context_survive() {
    let root = private_tempdir();
    let store = open(root.path());
    let ids = records(&store, "M", 1);
    let headers: Vec<_> = (0..6).map(|_| sealed(&store, &ids)).collect();
    assert_eq!(store.hold_snapshot(&headers[0]), Ok(true));
    assert_eq!(
        store.retire_step(RetireStep::UnusedSnapshots, &EAGER),
        Ok(1),
        "6 - 4 newest - 1 held"
    );
    store.set_policy(0, SMALL_POLICY).unwrap();
    store.pretend_usage_for_test(60 * MIB);
    assert_eq!(
        store.retire_step(RetireStep::UnusedSnapshots, &EAGER),
        Ok(3),
        "under pressure: the newest one"
    );
    let Lookup::Hit(left) = store.find_snapshots(&context()).unwrap() else {
        panic!()
    };
    let left: Vec<_> = left.iter().map(|header| header.id()).collect();
    assert_eq!(left, [headers[5].id(), headers[0].id()]);
}

#[test]
fn a_publisher_racing_retirement_references_a_live_row_or_retries() {
    let root = private_tempdir();
    let gc = open(root.path());
    let publisher = std::sync::Arc::new(open(root.path()));
    let evidence = binding_evidence(&publisher, "shapes", "/w/Shapes.pas");
    let record = local_member(
        &publisher,
        "/w/Main.pas",
        "unit Main;",
        "/w/Shapes.pas",
        evidence,
    );
    // The record's artifact and evidence exist but nothing references them yet.
    age_past_grace(&gc);
    gc.advance_clock_for_test(LEASE_EXPIRY); // the publisher's lease looks expired
    let racer = std::sync::Arc::clone(&publisher);
    let attempt = record.clone();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = std::sync::Arc::clone(&seen);
    gc.set_fault_for_test(FaultPoint::GcRetireBeforeCommit, move || {
        // Inside the retirement transaction: the publisher cannot interleave.
        *slot.lock().unwrap() = Some(racer.publish_member_record(&attempt).map(|p| p.published));
    });
    assert_eq!(gc.retire_step(RetireStep::Evidence, &EAGER), Ok(1));
    gc.clear_fault_for_test(FaultPoint::GcRetireBeforeCommit);
    assert_eq!(*seen.lock().unwrap(), Some(Err(Bypass::Busy)));
    assert_eq!(
        publisher
            .publish_member_record(&record)
            .map(|p| p.published),
        Err(Bypass::Retiring)
    );
    assert_eq!(check_invariant(root.path()), 0);
}

#[test]
fn retirement_refuses_invalid_limits_and_writes_nothing_at_the_wal_threshold() {
    let root = private_tempdir();
    let store = open(root.path());
    let invalid = MaintenanceLimits {
        keep_snapshots: 5,
        ..MaintenanceLimits::DEFAULT
    };
    assert_eq!(
        store.retire_step(RetireStep::Records, &invalid),
        Err(Bypass::InvalidMaintenanceLimits)
    );
    artifact(&store, "unit Stray;");
    age_past_grace(&store);
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 * MIB,
                wal_threshold_bytes: 8 * MIB,
            },
        )
        .unwrap();
    publish_bulk(&store, "unit Bulk;", 9, 1_000_000).unwrap();
    assert_eq!(store.pressure(), Ok(Pressure::WalPressure));
    assert_eq!(
        store.retire_step(RetireStep::Artifacts, &EAGER),
        Err(Bypass::WalPressure)
    );
}

#[test]
fn maintenance_limits_only_go_downward() {
    assert_eq!(MaintenanceLimits::default(), MaintenanceLimits::DEFAULT);
    assert_eq!(MaintenanceLimits::DEFAULT.validate(), Ok(()));
    assert_eq!(EAGER.validate(), Ok(()));
    let with = |change: fn(&mut MaintenanceLimits)| {
        let mut limits = MaintenanceLimits::DEFAULT;
        change(&mut limits);
        limits.validate()
    };
    assert!(with(|l| l.keep_snapshots = 5).is_err());
    assert!(with(|l| l.keep_snapshots_under_pressure = 0).is_err());
    assert!(
        with(|l| {
            l.keep_snapshots = 1;
            l.keep_snapshots_under_pressure = 2;
        })
        .is_err()
    );
    assert!(with(|l| l.recency_window = 65_537).is_err());
    assert!(with(|l| l.batch_rows = 0).is_err());
    assert!(with(|l| l.batch_rows = 257).is_err());
    assert!(with(|l| l.vacuum_pages = 0).is_err());
    assert!(with(|l| l.vacuum_pages = 257).is_err());
    assert_eq!(
        with(|l| {
            l.keep_snapshots = 1;
            l.batch_rows = 1;
            l.vacuum_pages = 1;
        }),
        Ok(())
    );
}

#[test]
fn a_batch_retires_at_most_batch_rows() {
    let root = private_tempdir();
    let store = open(root.path());
    records(&store, "Stray", 3);
    age_past_grace(&store);
    let one = MaintenanceLimits {
        batch_rows: 2,
        ..EAGER
    };
    assert_eq!(store.retire_step(RetireStep::Records, &one), Ok(2));
    assert_eq!(store.retire_step(RetireStep::Records, &one), Ok(1));
    assert_eq!(store.retire_step(RetireStep::Records, &one), Ok(0));
}

fn table_rows(directory: &Path) -> i64 {
    [
        "snapshot_members",
        "snapshots",
        "member_bindings",
        "member_records",
        "observations",
        "evidence_sets",
        "artifact_rows",
        "artifacts",
        "key_manifests",
        "lease_holds",
    ]
    .iter()
    .map(|table| count(directory, &format!("SELECT count(*) FROM {table}")))
    .sum()
}

#[test]
fn retired_rows_inside_the_live_scope_are_deleted_children_first_in_bounded_batches() {
    let root = private_tempdir();
    let store = open(root.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let record = store
        .publish_member_record(&local_member(
            &store,
            "/w/Main.pas",
            "unit Main;",
            "/w/Shapes.pas",
            evidence,
        ))
        .unwrap()
        .record_id;
    let snapshot = sealed(&store, &[record]);
    publish_bulk(&store, "unit Bulk;", 300, 10).unwrap();
    age_past_grace(&store);
    assert!(store.retire_snapshot(snapshot.id()).unwrap());
    while retire_all(&store, &EAGER) > 0 {}
    let mut passes = 0;
    loop {
        let Reclaimed { rows, complete } = store.reclaim(5).unwrap();
        assert!(rows <= 5);
        assert_eq!(check_invariant(root.path()), 0, "after every batch");
        passes += 1;
        if complete {
            break;
        }
        assert!(passes < 200, "deletion must progress");
    }
    assert_eq!(table_rows(root.path()), 0, "everything was unreferenced");
    assert_eq!(store.check_scope(), Ok(()), "the live scope itself stays");
    assert_eq!(
        store.reclaim(5),
        Ok(Reclaimed {
            rows: 0,
            complete: true
        })
    );
}

#[test]
fn live_rows_are_never_deleted() {
    let root = private_tempdir();
    let store = open(root.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let record = store
        .publish_member_record(&local_member(
            &store,
            "/w/Main.pas",
            "unit Main;",
            "/w/Shapes.pas",
            evidence,
        ))
        .unwrap()
        .record_id;
    let snapshot = sealed(&store, &[record]);
    let before = table_rows(root.path());
    assert_eq!(
        store.reclaim(1000),
        Ok(Reclaimed {
            rows: 0,
            complete: true
        })
    );
    assert_eq!(table_rows(root.path()), before);
    assert_eq!(
        read_members(&store, &snapshot).map(|members| members.len()),
        Ok(1)
    );
    assert!(read_record(&store, record).is_ok());
}

#[test]
fn deletion_between_batches_is_a_miss_never_a_verified_read() {
    let root = private_tempdir();
    let reader_store = open(root.path());
    let gc = open(root.path());
    publish_bulk(&reader_store, "unit Bulk;", 300, 10).unwrap();
    let Lookup::Hit(header) = reader_store
        .open_artifact(ArtifactKeyRef::local(&key("unit Bulk;")))
        .unwrap()
    else {
        panic!("published")
    };
    let mut reader = reader_store.read_artifact(&header);
    assert!(
        matches!(reader.next_batch(), Ok(Lookup::Hit(Some(_)))),
        "first batch read"
    );
    age_past_grace(&gc);
    gc.advance_clock_for_test(LEASE_EXPIRY);
    gc.renew_lease().unwrap();
    assert_eq!(gc.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    while !gc.reclaim(256).unwrap().complete {}
    assert_eq!(count(root.path(), "SELECT count(*) FROM artifact_rows"), 0);
    assert!(matches!(
        reader.next_batch(),
        Ok(Lookup::Miss(Miss::Retired | Miss::Vanished))
    ));
    assert!(!reader.is_verified());
}

#[test]
fn a_republish_after_deletion_inserts_afresh() {
    let root = private_tempdir();
    let store = open(root.path());
    publish_bulk(&store, "unit A;", 3, 10).unwrap();
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    assert_eq!(
        publish_bulk(&store, "unit A;", 3, 10),
        Err(Bypass::Retiring),
        "retry later"
    );
    while !store.reclaim(256).unwrap().complete {}
    assert_eq!(
        publish_bulk(&store, "unit A;", 3, 10),
        Ok(Published::Inserted)
    );
}

#[test]
fn discovery_evidence_and_artifact_evidence_of_a_sealed_snapshot_survive_deletion() {
    let root = private_tempdir();
    let store = open(root.path());
    let library = LocalArtifactKey::new(
        &SourceContent::from_bytes(b"unit Lib;"),
        &ConditionalContext::default(),
        SourceOrigin::Library,
    );
    let expanded = ExpandedArtifactKey::new(&library, &[]).unwrap();
    let artifact_key = ArtifactKeyRef::expanded(&expanded, &library).unwrap();
    let observations = [Observation::Presence {
        path: "/w/b.inc".into(),
        entry: None,
        present: false,
    }];
    let write = EvidenceWrite {
        context: &context(),
        observations: &observations,
    };
    let facts = ArtifactFacts {
        slot_lengths: vec![12],
        expanded_length: 12,
        ..ArtifactFacts::default()
    };
    store
        .publish_facts_with_evidence(artifact_key, &facts, &write)
        .unwrap();
    let artifact_evidence = |store: &Store| {
        let Lookup::Hit(headers) = store.find_evidence(&context(), &library).unwrap() else {
            panic!("artifact evidence is live")
        };
        headers[0].evidence_id()
    };
    let evidence = artifact_evidence(&store);
    let shapes = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let mut lib = member("/w/Lib.pas", &["/w/Shapes.pas"], shapes);
    lib.header.artifact = Some(artifact_key.digest());
    lib.header.member_evidence = Some(evidence);
    let record = store.publish_member_record(&lib).unwrap().record_id;
    let discovery_scope = DiscoveryScope::new(&DiscoveryInputs {
        directories: &[ScannedDirectory {
            path: Path::new("/w"),
            recursive: true,
        }],
        exclusions: &[],
    });
    let discovery = store
        .publish_evidence(EvidenceTarget::Discovery(&discovery_scope), &write)
        .unwrap()
        .evidence_id;
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &[record]).unwrap();
    let snapshot = store
        .seal_snapshot(
            builder,
            &SnapshotSeal {
                discovery: Some(discovery),
                ..SEAL
            },
        )
        .unwrap();
    age_past_grace(&store);
    while retire_all(&store, &EAGER) > 0 {}
    while !store.reclaim(256).unwrap().complete {}
    assert_eq!(check_invariant(root.path()), 0);
    let dir = root.path();
    assert_eq!(
        lifecycle(dir, "evidence_sets", "evidence_id", discovery.as_bytes()),
        1
    );
    assert_eq!(
        lifecycle(dir, "evidence_sets", "evidence_id", evidence.as_bytes()),
        1
    );
    assert_eq!(
        lifecycle(
            dir,
            "artifacts",
            "artifact_id",
            artifact_key.digest().as_bytes()
        ),
        1
    );
    assert_eq!(artifact_evidence(&store), evidence);
    assert!(read_record(&store, record).is_ok());
    assert_eq!(read_members(&store, &snapshot).map(|m| m.len()), Ok(1));
    let Lookup::Hit(header) = store.open_artifact(artifact_key).unwrap() else {
        panic!("the expanded artifact and its root manifest survive")
    };
    let mut reader = store.read_artifact(&header);
    while let Lookup::Hit(Some(_)) = reader.next_batch().unwrap() {}
    assert!(reader.is_verified());
}

#[test]
fn a_gc_retired_build_loses_its_expired_holds_before_its_header() {
    let root = private_tempdir();
    let builder_store = open(root.path());
    let gc = open(root.path());
    let ids = records(&builder_store, "M", 1);
    let mut builder = builder_store.begin_snapshot(&context()).unwrap();
    builder_store.add_members(&mut builder, &ids).unwrap();
    gc.advance_clock_for_test(LEASE_EXPIRY + LEASE_RENEWAL);
    gc.renew_lease().unwrap();
    assert_eq!(gc.retire_step(RetireStep::AbandonedBuilds, &EAGER), Ok(1));
    assert_eq!(count(root.path(), "SELECT count(*) FROM lease_holds"), 1);
    let one = gc.reclaim(1).unwrap();
    assert_eq!((one.rows, one.complete), (1, false));
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM lease_holds"),
        0,
        "the hold goes first"
    );
    while !gc.reclaim(256).unwrap().complete {}
    assert_eq!(count(root.path(), "SELECT count(*) FROM snapshots"), 0);
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM snapshot_members"),
        0
    );
    assert_eq!(
        builder_store.add_members(&mut builder, &ids),
        Err(Bypass::BuilderFenced)
    );
}

#[test]
fn deletion_runs_in_one_write_transaction_through_its_fault_point() {
    let root = private_tempdir();
    let gc = open(root.path());
    let publisher = std::sync::Arc::new(open(root.path()));
    artifact(&gc, "unit Stray;");
    age_past_grace(&gc);
    gc.advance_clock_for_test(LEASE_EXPIRY);
    gc.renew_lease().unwrap();
    assert_eq!(gc.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    let racer = std::sync::Arc::clone(&publisher);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot = std::sync::Arc::clone(&seen);
    gc.set_fault_for_test(FaultPoint::GcDeleteBeforeCommit, move || {
        slot.lock()
            .unwrap()
            .push(publish_bulk(&racer, "unit Racer;", 1, 10));
    });
    let reclaimed = gc.reclaim(256).unwrap();
    gc.clear_fault_for_test(FaultPoint::GcDeleteBeforeCommit);
    assert!(reclaimed.complete && reclaimed.rows > 0);
    assert_eq!(*seen.lock().unwrap(), [Err(Bypass::Busy)]);
    assert!(FaultPoint::MAINTENANCE.contains(&FaultPoint::GcDeleteBeforeCommit));
}

#[test]
fn reclaim_writes_nothing_at_the_wal_threshold() {
    let root = private_tempdir();
    let store = open(root.path());
    artifact(&store, "unit Stray;");
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 * MIB,
                wal_threshold_bytes: 8 * MIB,
            },
        )
        .unwrap();
    publish_bulk(&store, "unit Bulk;", 9, 1_000_000).unwrap();
    assert_eq!(store.pressure(), Ok(Pressure::WalPressure));
    assert_eq!(store.reclaim(256), Err(Bypass::WalPressure));
    assert_eq!(count(root.path(), "SELECT count(*) FROM artifacts"), 2);
}

#[test]
fn a_fenced_handle_still_reclaims_whole_retired_scopes() {
    let root = private_tempdir();
    let store = open(root.path());
    artifact(&store, "unit Stray;");
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    let connection = raw(root.path());
    connection
        .execute("UPDATE namespaces SET reset_epoch = reset_epoch + 1", [])
        .unwrap();
    connection
        .execute(
            "INSERT INTO retired_scopes SELECT namespace_id, 0 FROM namespaces",
            [],
        )
        .unwrap();
    assert_eq!(store.check_scope(), Err(Bypass::Fenced));
    while !store.reclaim(256).unwrap().complete {}
    assert_eq!(table_rows(root.path()), 0);
    assert_eq!(count(root.path(), "SELECT count(*) FROM retired_scopes"), 0);
}

#[test]
fn an_idle_store_checkpoints_and_truncates_its_wal() {
    let root = private_tempdir();
    let store = open(root.path());
    publish_bulk(&store, "unit A;", 9, 1_000_000).unwrap();
    let checkpoint = store.checkpoint().unwrap();
    assert!(
        checkpoint.complete() && checkpoint.wal_frames > 0,
        "{checkpoint:?}"
    );
    assert!(checkpoint.truncated);
    assert_eq!(file_len(root.path(), "-wal"), 0);
    assert!(
        file_len(root.path(), "") > 8 << 20,
        "the pages are in the database"
    );
}

#[test]
fn a_held_reader_keeps_the_checkpoint_partial_and_writes_stop_at_the_threshold() {
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
    let reader = raw(root.path());
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
        .unwrap();
    publish_bulk(&store, "unit A;", 9, 1_000_000).unwrap();
    let checkpoint = store.checkpoint().unwrap();
    assert!(
        !checkpoint.complete() && !checkpoint.truncated,
        "{checkpoint:?}"
    );
    assert_eq!(
        publish_bulk(&store, "unit B;", 1, 10),
        Err(Bypass::WalPressure)
    );
    reader.execute_batch("COMMIT").unwrap();
    assert!(store.checkpoint().unwrap().truncated);
    assert_eq!(
        publish_bulk(&store, "unit B;", 1, 10),
        Ok(Published::Inserted)
    );
}

#[test]
fn incremental_vacuum_returns_free_pages_in_bounded_batches() {
    let root = private_tempdir();
    let store = open(root.path());
    publish_bulk(&store, "unit A;", 9, 1_000_000).unwrap();
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    while !store.reclaim(256).unwrap().complete {}
    store.checkpoint().unwrap();
    let free = || store.usage().unwrap().free_pages;
    let start = free();
    assert!(start > 256, "{start} free pages");
    let limits = MaintenanceLimits {
        vacuum_free_bytes: 0,
        ..EAGER
    };
    let first = store.vacuum_step(&limits).unwrap();
    assert!((1..=256).contains(&first));
    while store.vacuum_step(&limits).unwrap() > 0 {}
    assert_eq!(free(), 0);
    assert_eq!(
        store.vacuum_step(&MaintenanceLimits::default()),
        Ok(0),
        "below the threshold"
    );
    store.checkpoint().unwrap();
    assert!(file_len(root.path(), "") < 2 << 20, "the file shrank");
}

#[test]
fn a_vacuum_batch_releases_its_whole_page_budget() {
    let root = private_tempdir();
    let store = open(root.path());
    publish_bulk(&store, "unit A;", 9, 1_000_000).unwrap();
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    while !store.reclaim(256).unwrap().complete {}
    let start = store.usage().unwrap().free_pages;
    assert!(start > 256, "{start} free pages");
    let limits = MaintenanceLimits {
        vacuum_free_bytes: 0,
        ..EAGER
    };
    assert_eq!(store.vacuum_step(&limits), Ok(256));
    assert_eq!(store.usage().unwrap().free_pages, start - 256);
}

#[test]
fn vacuum_validates_its_limits_and_writes_nothing_at_the_wal_threshold() {
    let root = private_tempdir();
    let store = open(root.path());
    let bad = MaintenanceLimits {
        vacuum_pages: 257,
        ..EAGER
    };
    assert_eq!(
        store.vacuum_step(&bad),
        Err(Bypass::InvalidMaintenanceLimits)
    );
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 64 * MIB,
                wal_threshold_bytes: 8 * MIB,
            },
        )
        .unwrap();
    let reader = raw(root.path());
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
        .unwrap();
    publish_bulk(&store, "unit A;", 9, 1_000_000).unwrap();
    let eager = MaintenanceLimits {
        vacuum_free_bytes: 0,
        ..EAGER
    };
    assert_eq!(store.vacuum_step(&eager), Err(Bypass::WalPressure));
    assert!(store.checkpoint().is_ok(), "checkpoints are always allowed");
}

#[test]
fn a_vacuum_batch_passes_its_fault_point_before_commit() {
    let root = private_tempdir();
    let store = open(root.path());
    publish_bulk(&store, "unit A;", 9, 1_000_000).unwrap();
    age_past_grace(&store);
    assert_eq!(store.retire_step(RetireStep::Artifacts, &EAGER), Ok(1));
    while !store.reclaim(256).unwrap().complete {}
    store.checkpoint().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = hits.clone();
    store.set_fault_for_test(FaultPoint::VacuumBeforeCommit, move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let limits = MaintenanceLimits {
        vacuum_free_bytes: 0,
        ..EAGER
    };
    assert!(store.vacuum_step(&limits).unwrap() > 0);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = completed.clone();
    store.set_fault_for_test(FaultPoint::CheckpointCompleted, move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    store.checkpoint().unwrap();
    assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn reclaiming_zero_rows_with_nothing_retired_is_complete() {
    let root = private_tempdir();
    let store = open(root.path());
    assert_eq!(
        store.reclaim(0),
        Ok(Reclaimed {
            rows: 0,
            complete: true
        })
    );
}

const ROOMY: StepBudget = StepBudget {
    time: std::time::Duration::from_secs(60),
    rows: 10,
};

fn strays(store: &Store, count: usize) {
    for index in 0..count {
        artifact(store, &format!("unit Stray{index};"));
        binding_evidence(store, &format!("stray{index}"), "/w/Stray.pas");
    }
    records(store, "Stray", count);
}

#[test]
fn a_cycle_reclaims_everything_unreferenced_within_its_row_budget() {
    let root = private_tempdir();
    let store = open(root.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let kept = store
        .publish_member_record(&local_member(
            &store,
            "/w/Main.pas",
            "unit Main;",
            "/w/Shapes.pas",
            evidence,
        ))
        .unwrap()
        .record_id;
    let snapshot = sealed(&store, &[kept]);
    strays(&store, 12);
    age_past_grace(&store);
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let never = AtomicBool::new(false);
    let mut steps = 0;
    loop {
        let report = maintenance.step(&store, ROOMY, &never).unwrap();
        assert!(report.rows <= ROOMY.rows, "{report:?}");
        assert_eq!(check_invariant(root.path()), 0);
        steps += 1;
        if report.cycle_complete {
            break;
        }
        assert!(steps < 100);
    }
    assert!(steps > 1, "the row budget split the cycle");
    for table in ["artifacts", "evidence_sets", "member_records"] {
        assert_eq!(
            count(root.path(), &format!("SELECT count(*) FROM {table}")),
            1,
            "{table}"
        );
    }
    assert_eq!(read_members(&store, &snapshot).map(|m| m.len()), Ok(1));
    assert_eq!(maintenance.phase(), Phase::Done);
    let next = maintenance.step(&store, ROOMY, &never).unwrap();
    assert!(
        next.cycle_complete && next.rows == 0,
        "a new cycle: {next:?}"
    );
}

#[test]
fn a_cancelled_step_stops_between_transactions_and_resumes() {
    let root = private_tempdir();
    let store = open(root.path());
    strays(&store, 3);
    age_past_grace(&store);
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&cancel);
    store.set_fault_for_test(FaultPoint::GcRetireBeforeCommit, move || {
        flag.store(true, std::sync::atomic::Ordering::Relaxed)
    });
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let report = maintenance
        .step(
            &store,
            StepBudget {
                rows: 1000,
                ..ROOMY
            },
            &cancel,
        )
        .unwrap();
    assert!(report.cancelled && !report.cycle_complete, "{report:?}");
    let stopped = maintenance.phase();
    assert!(stopped >= Phase::AbandonedBuilds && stopped < Phase::Done);
    store.clear_fault_for_test(FaultPoint::GcRetireBeforeCommit);
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let report = maintenance
        .step(
            &store,
            StepBudget {
                rows: 1000,
                ..ROOMY
            },
            &cancel,
        )
        .unwrap();
    assert!(report.cycle_complete, "{report:?}");
    assert_eq!(count(root.path(), "SELECT count(*) FROM artifacts"), 0);
}

#[test]
fn a_step_is_skipped_while_another_holder_has_the_maintenance_lock() {
    let root = private_tempdir();
    let first = open(root.path());
    let second = open(root.path());
    let held = first.hold_maintenance_lock_for_test().unwrap();
    assert!(root.path().join("maintenance.lock").is_file());
    assert_eq!(
        second.location().maintenance_lock(),
        root.path().join("maintenance.lock")
    );
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let report = maintenance
        .step(&second, ROOMY, &AtomicBool::new(false))
        .unwrap();
    assert!(report.skipped && report.rows == 0, "{report:?}");
    drop(held);
    assert!(
        !maintenance
            .step(&second, ROOMY, &AtomicBool::new(false))
            .unwrap()
            .skipped
    );
}

fn newest_renewal(directory: &Path) -> i64 {
    count(
        directory,
        "SELECT max(renewed_unix_seconds) FROM process_leases",
    )
}

#[test]
fn a_skipped_step_still_renews_a_due_lease_and_every_step_does() {
    let root = private_tempdir();
    let first = open(root.path());
    let second = open(root.path());
    let opened = newest_renewal(root.path());
    let held = first.hold_maintenance_lock_for_test().unwrap();
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let never = AtomicBool::new(false);
    assert!(maintenance.step(&second, ROOMY, &never).unwrap().skipped);
    assert_eq!(newest_renewal(root.path()), opened, "not due: no write");
    second.advance_clock_for_test(LEASE_RENEWAL);
    assert!(maintenance.step(&second, ROOMY, &never).unwrap().skipped);
    let renewed = newest_renewal(root.path());
    assert!(
        renewed >= opened + LEASE_RENEWAL.as_secs() as i64,
        "{opened} -> {renewed}"
    );
    drop(held);
    // A step resuming past the Leases phase renews too.
    let limited = StepBudget { rows: 0, ..ROOMY };
    let report = maintenance.step(&second, limited, &never).unwrap();
    assert_eq!((report.rows, maintenance.phase()), (0, Phase::Hints));
    second.advance_clock_for_test(LEASE_RENEWAL);
    maintenance.step(&second, limited, &never).unwrap();
    assert!(newest_renewal(root.path()) >= renewed + LEASE_RENEWAL.as_secs() as i64);
}

#[test]
fn the_driver_refuses_invalid_limits() {
    assert_eq!(
        Maintenance::new(MaintenanceLimits {
            batch_rows: 0,
            ..EAGER
        })
        .err(),
        Some(MaintenanceLimitsError::BatchRows)
    );
    assert_eq!(Maintenance::new(EAGER).unwrap().phase(), Phase::Hints);
}

#[test]
fn under_wal_pressure_maintenance_only_checkpoints() {
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
    strays(&store, 2);
    age_past_grace(&store);
    let reader = raw(root.path());
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
        .unwrap();
    publish_bulk(&store, "unit Bulk;", 9, 1_000_000).unwrap();
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let never = AtomicBool::new(false);
    store.advance_clock_for_test(LEASE_RENEWAL);
    let renewal = newest_renewal(root.path());
    let report = maintenance.step(&store, ROOMY, &never).unwrap();
    assert_eq!(
        (report.pressure, report.rows, maintenance.phase()),
        (Pressure::WalPressure, 0, Phase::Hints)
    );
    assert_eq!(
        newest_renewal(root.path()),
        renewal,
        "not even a lease renewal"
    );
    reader.execute_batch("COMMIT").unwrap();
    let report = maintenance.step(&store, ROOMY, &never).unwrap();
    assert!(report.rows > 0, "{report:?}");
}

#[test]
fn a_full_store_still_runs_maintenance() {
    let root = private_tempdir();
    let store = open(root.path());
    strays(&store, 2);
    age_past_grace(&store);
    store.limit_growth_for_test(4).unwrap();
    assert_eq!(
        publish_bulk(&store, "unit Big;", 10, 200_000),
        Err(Bypass::Full)
    );
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let report = maintenance
        .step(
            &store,
            StepBudget {
                rows: 1000,
                ..ROOMY
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(report.cycle_complete && report.rows > 0, "{report:?}");
}

#[test]
fn wal_pressure_reached_mid_cycle_stops_the_writes_not_the_step() {
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
    strays(&store, 2);
    age_past_grace(&store);
    // Opened after the strays, so its lease does not hold them back.
    let publisher = std::sync::Arc::new(open(root.path()));
    let reader = std::sync::Arc::new(std::sync::Mutex::new(raw(root.path())));
    let (racer, held) = (std::sync::Arc::clone(&publisher), reader.clone());
    let once = std::sync::Once::new();
    store.set_fault_for_test(FaultPoint::CheckpointCompleted, move || {
        once.call_once(|| {
            let held = held.lock().unwrap();
            held.execute_batch("BEGIN").unwrap();
            let _: i64 = held
                .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
                .unwrap();
            publish_bulk(&racer, "unit Bulk;", 9, 1_000_000).unwrap();
        })
    });
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let never = AtomicBool::new(false);
    let budget = StepBudget {
        rows: 1000,
        ..ROOMY
    };
    let report = maintenance.step(&store, budget, &never).unwrap();
    assert_eq!(
        (report.pressure, report.rows, report.phase),
        (Pressure::WalPressure, 0, Phase::ObsoleteNamespaces),
        "{report:?}"
    );
    assert_eq!(maintenance.phase(), Phase::ObsoleteNamespaces);
    assert_eq!(store.health(), None, "not a failure");
    reader.lock().unwrap().execute_batch("COMMIT").unwrap();
    let report = maintenance.step(&store, budget, &never).unwrap();
    assert!(report.cycle_complete && report.rows > 0, "{report:?}");
}

#[test]
fn a_cycle_completes_only_once_every_retired_scope_is_gone() {
    let root = private_tempdir();
    let store = open(root.path());
    raw(root.path())
        .execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40) \
             INSERT INTO retired_scopes (namespace_id, reset_epoch) SELECT randomblob(32), 0 FROM n",
        )
        .unwrap();
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let never = AtomicBool::new(false);
    let budget = StepBudget {
        rows: 1000,
        ..ROOMY
    };
    let report = maintenance.step(&store, budget, &never).unwrap();
    assert!(report.cycle_complete, "{report:?}");
    assert_eq!(count(root.path(), "SELECT count(*) FROM retired_scopes"), 0);
}

fn namespace() -> Namespace {
    Namespace::new(Digest::from_bytes([common::records::NAMESPACE; 32]))
}

#[test]
fn status_of_an_absent_store_creates_nothing() {
    let root = private_tempdir();
    let directory = root.path().join("cache");
    assert_eq!(admin::status(&directory), StoreStatus::Absent);
    assert!(!directory.exists());
    assert_eq!(
        admin::prune(&directory, &namespace(), StoreLimits::default(), ROOMY),
        Ok(None)
    );
    assert_eq!(
        admin::reset(&directory, &namespace(), StoreLimits::default()),
        Ok(false)
    );
    assert!(!directory.exists());
}

#[test]
fn status_reports_usage_policy_and_counts_without_registering() {
    let root = private_tempdir();
    let store = open(root.path());
    let StoreStatus::Present(report) = admin::status(root.path()) else {
        panic!()
    };
    assert_eq!(report.policy.policy, StorePolicy::default());
    assert_eq!(
        (
            report.live_namespaces,
            report.live_leases,
            report.retired_scopes
        ),
        (1, 1, 0)
    );
    assert!(report.usage.physical_bytes() > 0);
    assert_eq!(
        (report.container.version, report.container.compatible),
        (pascal_cache::CONTAINER_VERSION, true)
    );
    assert_eq!(report.advice, RecoveryAdvice::None);
    drop(store);
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM process_leases"),
        0,
        "status took no lease"
    );
    assert_eq!(count(root.path(), "SELECT count(*) FROM namespaces"), 1);
}

#[test]
fn a_corrupt_container_is_left_in_place_with_fresh_directory_advice() {
    let root = private_tempdir();
    let path = root.path().join(DATABASE_FILE);
    std::fs::write(&path, vec![0x5a; 8192]).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        admin::status(root.path()),
        StoreStatus::Unreadable {
            bypass: Bypass::Corrupt,
            advice: RecoveryAdvice::FreshDirectory
        }
    );
    assert_eq!(
        admin::prune(root.path(), &namespace(), StoreLimits::default(), ROOMY),
        Err(Bypass::Corrupt)
    );
    assert_eq!(
        admin::reset(root.path(), &namespace(), StoreLimits::default()),
        Err(Bypass::Corrupt)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "never rewritten");
    let names: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        names.contains(&DATABASE_FILE.into()),
        "never renamed or removed"
    );
}

#[test]
fn an_unsupported_container_is_reported_and_never_reclaimed() {
    let root = private_tempdir();
    drop(open(root.path()));
    raw(root.path())
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let before = std::fs::read(root.path().join(DATABASE_FILE)).unwrap();
    assert_eq!(
        admin::status(root.path()),
        StoreStatus::Unreadable {
            bypass: Bypass::UnsupportedContainer { version: 99 },
            advice: RecoveryAdvice::FreshDirectory
        }
    );
    assert!(admin::prune(root.path(), &namespace(), StoreLimits::default(), ROOMY).is_err());
    assert_eq!(
        std::fs::read(root.path().join(DATABASE_FILE)).unwrap(),
        before
    );
}

#[test]
fn a_read_only_database_is_bypassed_and_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let root = private_tempdir();
    drop(open(root.path()));
    let path = root.path().join(DATABASE_FILE);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        admin::status(root.path()),
        StoreStatus::Unreadable {
            bypass: Bypass::ReadOnly,
            ..
        }
    ));
    assert_eq!(
        admin::prune(root.path(), &namespace(), StoreLimits::default(), ROOMY),
        Err(Bypass::ReadOnly)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_reset_fences_every_handle_and_reclaims_the_old_epoch() {
    let root = private_tempdir();
    let first = open(root.path());
    let second = open(root.path());
    sealed(&first, &records(&first, "M", 3));
    first.reset_namespace().unwrap();
    assert_eq!(first.check_scope(), Err(Bypass::Fenced));
    assert_eq!(
        second.find_snapshots(&context()).err(),
        Some(Bypass::Fenced)
    );
    drop((first, second));
    let again = open(root.path());
    assert_eq!(again.scope().reset_epoch, 1);
    assert_eq!(
        again.find_snapshots(&context()),
        Ok(Lookup::Miss(Miss::Absent))
    );
    while !again.reclaim(256).unwrap().complete {}
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM member_records WHERE reset_epoch = 0"
        ),
        0
    );
    assert_eq!(count(root.path(), "SELECT count(*) FROM namespaces"), 1);
}

#[test]
fn admin_reset_fences_and_reclaims_a_present_store() {
    let root = private_tempdir();
    let holder = open(root.path());
    sealed(&holder, &records(&holder, "M", 3));
    assert!(
        count(
            root.path(),
            "SELECT count(*) FROM member_records WHERE reset_epoch = 0"
        ) > 0
    );
    assert_eq!(
        admin::reset(root.path(), &namespace(), StoreLimits::default()),
        Ok(true)
    );
    assert_eq!(holder.check_scope(), Err(Bypass::Fenced));
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM member_records WHERE reset_epoch = 0"
        ),
        0,
        "the bounded reclamation ran"
    );
    drop(holder);
    assert_eq!(open(root.path()).scope().reset_epoch, 1);
}

#[test]
fn a_read_only_directory_is_bypassed_and_left_alone() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let root = private_tempdir();
    drop(open(root.path()));
    let snapshot = |directory: &Path| {
        let mut entries: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let metadata = entry.metadata().unwrap();
                (
                    entry.file_name(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                )
            })
            .collect();
        entries.sort();
        entries
    };
    let before = snapshot(root.path());
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let status = admin::status(root.path());
    let pruned = admin::prune(root.path(), &namespace(), StoreLimits::default(), ROOMY);
    let opened = Store::open(root.path(), &namespace(), StoreLimits::default());
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let StoreStatus::Unreadable { bypass, advice } = status else {
        panic!("{status:?}")
    };
    assert_eq!(advice, pascal_cache::admin::advice(&bypass));
    assert_eq!(pruned.err(), Some(bypass.clone()));
    assert_eq!(opened.err(), Some(bypass.clone()));
    assert_eq!(bypass, Bypass::ReadOnly, "observed");
    assert_eq!(advice, RecoveryAdvice::FreshDirectory);
    assert_eq!(snapshot(root.path()), before);
}

#[test]
fn ac2_a_low_budget_bounds_writes_and_maintenance_reclaims_obsolete_versions() {
    let root = private_tempdir();
    let obsolete = Store::open(
        root.path(),
        &Namespace::new(Digest::from_bytes([2; 32])),
        StoreLimits::default(),
    )
    .unwrap();
    publish_bulk(&obsolete, "unit Old;", 9, 1_000_000).unwrap();
    drop(obsolete);
    let store = open(root.path()); // retires namespace 2 at open
    store.checkpoint().unwrap();
    let full = store.usage().unwrap();
    store
        .set_policy(
            0,
            StorePolicy {
                max_disk_bytes: 32 * MIB,
                wal_threshold_bytes: 8 * MIB,
            },
        )
        .unwrap();
    store.pretend_usage_for_test(20 * MIB);
    assert_eq!(
        publish_bulk(&store, "unit New;", 1, 10),
        Err(Bypass::OverBudget)
    );
    assert_eq!(store.pressure(), Ok(Pressure::OverHeadroom));
    let mut maintenance = Maintenance::new(MaintenanceLimits::default()).unwrap();
    let never = AtomicBool::new(false);
    while !maintenance
        .step(&store, ROOMY, &never)
        .unwrap()
        .cycle_complete
    {}
    // A second cycle vacuums what the first one's deletion freed.
    while !maintenance
        .step(&store, ROOMY, &never)
        .unwrap()
        .cycle_complete
    {}
    store.checkpoint().unwrap();
    store.pretend_usage_for_test(0);
    let after = store.usage().unwrap();
    let live =
        |usage: &pascal_cache::Usage| (usage.page_count - usage.free_pages) * usage.page_size;
    assert!(
        live(&after) < live(&full) / 4,
        "obsolete rows reclaimed: {full:?} -> {after:?}"
    );
    assert!(
        after.database_bytes < full.database_bytes / 4,
        "free pages returned: {full:?} -> {after:?}"
    );
    assert!(
        after.wal_bytes <= JOURNAL_SIZE_LIMIT_BYTES as u64,
        "the WAL is bounded: {after:?}"
    );
    assert_eq!(
        publish_bulk(&store, "unit New;", 1, 10),
        Ok(Published::Inserted)
    );
}
