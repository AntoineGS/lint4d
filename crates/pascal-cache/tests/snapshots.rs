#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{
    SEAL, binding_evidence, context, member, open, raw, read_members, records, sealed,
};
use pascal_cache::{
    Bypass, ContextKey, Digest, DiscoveryInputs, DiscoveryScope, EvidenceTarget, EvidenceWrite,
    FaultPoint, Lookup, MAX_INDEX_KEY_BYTES, MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS,
    MAX_SNAPSHOT_CANDIDATES, MAX_SNAPSHOT_MEMBERS, Malformed, Miss, SealRefusal, SnapshotHeader,
    SnapshotSeal, TooLarge, path_key,
};
use pascal_project::{ProjectContext, ProjectContextParts, ProjectPathEntry};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Arc;

fn member_rows(directory: &Path) -> i64 {
    raw(directory)
        .query_row("SELECT count(*) FROM snapshot_members", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn found(store: &pascal_cache::Store) -> Vec<SnapshotHeader> {
    match store.find_snapshots(&context()).unwrap() {
        Lookup::Hit(headers) => headers,
        Lookup::Miss(Miss::Absent) => Vec::new(),
        Lookup::Miss(miss) => panic!("{miss:?}"),
    }
}

#[test]
fn a_sealed_snapshot_reads_back_its_members_verified() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 3);
    let header = sealed(&store, &ids);
    assert_eq!(header.member_count(), 3);
    assert_eq!(header.coverage(), SEAL.coverage);
    assert_eq!(found(&store), vec![header.clone()]);
    let members = read_members(&store, &header).unwrap();
    assert_eq!(
        members
            .iter()
            .map(|member| member.record_id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(members[0].ordinal, 0);
    assert_eq!(members[0].source_key, path_key(Path::new("/w/U0.pas")));
    let empty = sealed(&store, &[]);
    assert_eq!(read_members(&store, &empty), Ok(Vec::new()));
}

#[test]
fn a_token_used_by_two_writers_lets_exactly_one_write() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 2);
    let mut first = store.begin_snapshot(&context()).unwrap();
    let mut second = first.clone();
    store.add_members(&mut first, &ids[..1]).unwrap();
    assert_eq!(first.revision(), 1);
    assert_eq!(
        store.add_members(&mut second, &ids[1..]),
        Err(Bypass::BuilderFenced)
    );
    assert_eq!(second.revision(), 0, "the loser did not advance");
    assert_eq!(member_rows(directory.path()), 1, "the loser wrote nothing");
    assert_eq!(
        store.seal_snapshot(second, &SEAL),
        Err(Bypass::BuilderFenced),
        "a stale copy cannot seal"
    );
    let header = store.seal_snapshot(first, &SEAL).unwrap();
    assert_eq!(read_members(&store, &header).unwrap().len(), 1);
}

#[test]
fn a_duplicate_source_identity_fails_the_whole_batch() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "a", "/w/A.pas");
    let plain = store
        .publish_member_record(&member("/w/Main.pas", &[], evidence))
        .unwrap()
        .record_id;
    let importing = store
        .publish_member_record(&member("/w/Main.pas", &["/w/A.pas"], evidence))
        .unwrap()
        .record_id;
    let other = records(&store, "U", 1)[0];
    let mut builder = store.begin_snapshot(&context()).unwrap();
    assert_eq!(
        store.add_members(&mut builder, &[other, plain, importing]),
        Err(Bypass::DuplicateMember)
    );
    assert_eq!(builder.revision(), 0);
    assert_eq!(member_rows(directory.path()), 0, "the batch wrote nothing");
    store.add_members(&mut builder, &[other, plain]).unwrap();
    assert_eq!(
        store.add_members(&mut builder, &[importing]),
        Err(Bypass::DuplicateMember),
        "across batches too"
    );
    assert_eq!(
        store.add_members(&mut builder, &[plain]),
        Err(Bypass::DuplicateMember),
        "the same record twice"
    );
    assert_eq!(
        store.add_members(&mut builder, &[Digest::from_bytes([9; 32])]),
        Err(Bypass::MissingReference)
    );
    let header = store.seal_snapshot(builder, &SEAL).unwrap();
    assert_eq!(read_members(&store, &header).unwrap().len(), 2);
}

#[test]
fn a_record_retired_before_the_seal_refuses_it() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 3);
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &ids[..2]).unwrap();
    assert!(store.retire_member_record(ids[1]).unwrap());
    assert!(!store.retire_member_record(ids[1]).unwrap(), "once");
    assert!(store.retire_member_record(ids[2]).unwrap());
    assert_eq!(
        store.add_members(&mut builder, &ids[2..]),
        Err(Bypass::Retiring),
        "a retired record cannot be added"
    );
    assert_eq!(
        store.seal_snapshot(builder, &SEAL),
        Err(Bypass::SealRefused(SealRefusal::RetiredRecord))
    );
    assert!(found(&store).is_empty());
}

#[test]
fn a_record_retired_by_maintenance_after_the_digest_streamed_refuses_the_seal() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let maintenance = Arc::new(open(directory.path()));
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let id = store
        .publish_member_record(&member("/w/Main.pas", &["/w/Shapes.pas"], evidence))
        .unwrap()
        .record_id;
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &[id]).unwrap();
    // The digest streamed with the record live; maintenance retires it
    // before the seal transaction, which must see it.
    let retirer = Arc::clone(&maintenance);
    store.set_fault_for_test(FaultPoint::SealDigestStreamed, move || {
        assert!(retirer.retire_member_record(id).unwrap());
    });
    assert_eq!(
        store.seal_snapshot(builder, &SEAL),
        Err(Bypass::SealRefused(SealRefusal::RetiredRecord))
    );
    store.clear_fault_for_test(FaultPoint::SealDigestStreamed);
    assert!(found(&store).is_empty());
}

#[test]
fn a_count_that_disagrees_with_the_members_refuses_the_seal() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 2);
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &ids).unwrap();
    raw(directory.path())
        .execute("DELETE FROM snapshot_members WHERE ordinal = 1", [])
        .unwrap();
    assert_eq!(
        store.seal_snapshot(builder, &SEAL),
        Err(Bypass::SealRefused(SealRefusal::CountMismatch))
    );
}

#[test]
fn a_namespace_reset_between_batches_fences_the_builder() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 2);
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &ids[..1]).unwrap();
    raw(directory.path())
        .execute("UPDATE namespaces SET reset_epoch = reset_epoch + 1", [])
        .unwrap();
    assert_eq!(
        store.add_members(&mut builder, &ids[1..]),
        Err(Bypass::Fenced)
    );
    assert_eq!(store.seal_snapshot(builder, &SEAL), Err(Bypass::Fenced));
    drop(store);
    let reopened = open(directory.path());
    assert_eq!(reopened.scope().reset_epoch, 1);
    assert!(
        found(&reopened).is_empty(),
        "old-epoch rows satisfy nothing"
    );
    let fresh = records(&reopened, "N", 1);
    let header = sealed(&reopened, &fresh);
    assert_eq!(found(&reopened), vec![header]);
}

#[test]
fn a_seal_racing_maintenance_retirement_has_one_winner() {
    // Retired after the digest streamed: the seal's compare-and-set loses.
    let directory = private_tempdir();
    let store = open(directory.path());
    let maintenance = Arc::new(open(directory.path()));
    let ids = records(&store, "U", 2);
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &ids).unwrap();
    let id = builder.id();
    let retirer = Arc::clone(&maintenance);
    store.set_fault_for_test(FaultPoint::SealDigestStreamed, move || {
        assert!(retirer.retire_snapshot(id).unwrap());
    });
    assert_eq!(
        store.seal_snapshot(builder, &SEAL),
        Err(Bypass::BuilderFenced)
    );
    store.clear_fault_for_test(FaultPoint::SealDigestStreamed);
    assert!(found(&store).is_empty());
    // Sealed first: the retirement wins next and removes eligibility at once.
    let header = sealed(&store, &ids);
    assert!(maintenance.retire_snapshot(header.id()).unwrap());
    assert!(!maintenance.retire_snapshot(header.id()).unwrap(), "once");
    assert!(found(&store).is_empty());
    assert_eq!(read_members(&store, &header), Err(Miss::Retired));
}

#[test]
fn abandoned_and_retired_builds_never_hide_an_earlier_sealed_snapshot() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let first = sealed(&store, &records(&store, "A", 2));
    // A cancelled attempt: begun, partly built, never sealed.
    {
        let mut abandoned = store.begin_snapshot(&context()).unwrap();
        store
            .add_members(&mut abandoned, &records(&store, "B", 1))
            .unwrap();
    }
    // A build its own builder retired.
    let mut retired = store.begin_snapshot(&context()).unwrap();
    store
        .add_members(&mut retired, &records(&store, "C", 1))
        .unwrap();
    assert!(store.retire_build(&retired).unwrap());
    assert!(!store.retire_build(&retired).unwrap(), "once");
    assert_eq!(
        store.add_members(&mut retired, &records(&store, "D", 1)),
        Err(Bypass::BuilderFenced)
    );
    assert_eq!(
        store.seal_snapshot(retired, &SEAL),
        Err(Bypass::BuilderFenced)
    );
    assert_eq!(found(&store), vec![first.clone()]);
    assert_eq!(read_members(&store, &first).unwrap().len(), 2);
}

#[test]
fn an_old_builder_sealing_late_is_first_but_keeps_its_own_members() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let publish = |target: &str| {
        let evidence = binding_evidence(&store, "provider", target);
        store
            .publish_member_record(&member("/w/Main.pas", &[target], evidence))
            .unwrap()
            .record_id
    };
    let (old_record, new_record) = (publish("/w/Old.pas"), publish("/w/New.pas"));
    let mut old = store.begin_snapshot(&context()).unwrap();
    let mut new = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut old, &[old_record]).unwrap();
    store.add_members(&mut new, &[new_record]).unwrap();
    let new_header = store.seal_snapshot(new, &SEAL).unwrap();
    let old_header = store.seal_snapshot(old, &SEAL).unwrap();
    assert!(old_header.publication_seq() > new_header.publication_seq());
    // History, newest publication first: sessions validate in order (spec §9).
    assert_eq!(found(&store), vec![old_header.clone(), new_header.clone()]);
    let records_of = |header: &SnapshotHeader| {
        read_members(&store, header)
            .unwrap()
            .into_iter()
            .map(|member| member.record_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(records_of(&old_header), [old_record]);
    assert_eq!(records_of(&new_header), [new_record]);
}

type Tamper = Box<dyn Fn(&Connection, &[Digest])>;

#[test]
fn tampered_snapshots_are_never_a_verified_read() {
    let swap_ordinals = "UPDATE snapshot_members SET ordinal = 99 WHERE ordinal = 0; \
                         UPDATE snapshot_members SET ordinal = 0 WHERE ordinal = 1; \
                         UPDATE snapshot_members SET ordinal = 1 WHERE ordinal = 99;";
    let cases: [(&str, Tamper, Miss); 5] = [
        (
            "another live record swapped in",
            Box::new(|db, ids| {
                db.execute(
                    "UPDATE snapshot_members SET record_id = ?1 WHERE ordinal = 1",
                    [ids[2].as_bytes().as_slice()],
                )
                .unwrap();
            }),
            Miss::ChecksumMismatch,
        ),
        (
            "a source key copy rewritten",
            Box::new(|db, _| {
                db.execute(
                    "UPDATE snapshot_members SET source_key = x'00' WHERE ordinal = 1",
                    [],
                )
                .unwrap();
            }),
            Miss::ChecksumMismatch,
        ),
        (
            "reordered rows",
            Box::new(move |db, _| db.execute_batch(swap_ordinals).unwrap()),
            Miss::ChecksumMismatch,
        ),
        (
            "a deleted row",
            Box::new(|db, _| {
                db.execute("DELETE FROM snapshot_members WHERE ordinal = 1", [])
                    .unwrap();
            }),
            Miss::Malformed(pascal_cache::Malformed::Count),
        ),
        (
            "an appended row",
            Box::new(|db, _| {
                db.execute(
                    "INSERT INTO snapshot_members SELECT namespace_id, reset_epoch, snapshot_id, 2, \
                     record_id, x'01' FROM snapshot_members WHERE ordinal = 0",
                    [],
                )
                .unwrap();
            }),
            Miss::Malformed(pascal_cache::Malformed::Count),
        ),
    ];
    for (label, tamper, miss) in cases {
        let directory = private_tempdir();
        let store = open(directory.path());
        let ids = records(&store, "U", 3);
        let header = sealed(&store, &ids[..2]);
        tamper(&raw(directory.path()), &ids);
        assert_eq!(read_members(&store, &header), Err(miss), "{label}");
    }
}

#[test]
fn a_member_or_snapshot_retired_between_batches_is_a_miss() {
    let cases = [
        (
            "a member's record retired",
            "UPDATE member_records SET lifecycle = 2 WHERE record_id = ?1",
            Miss::Retired,
        ),
        (
            "a member's record deleted",
            "DELETE FROM member_records WHERE record_id = ?1",
            Miss::Vanished,
        ),
        (
            "the snapshot retired",
            "UPDATE snapshots SET state = 3 WHERE ?1 IS NOT NULL",
            Miss::Retired,
        ),
    ];
    for (label, change, miss) in cases {
        let directory = private_tempdir();
        let store = open(directory.path());
        let ids = records(&store, "U", 300);
        let header = sealed(&store, &ids);
        let mut reader = store.read_snapshot(&header);
        let Lookup::Hit(Some(first)) = reader.next_batch().unwrap() else {
            panic!("{label}: first batch")
        };
        assert_eq!(first.members().len(), 256);
        raw(directory.path())
            .execute(change, [ids[299].as_bytes().as_slice()])
            .unwrap();
        assert!(
            matches!(reader.next_batch().unwrap(), Lookup::Miss(found) if found == miss),
            "{label}"
        );
        assert!(!reader.is_verified());
    }
}

#[test]
fn at_most_eight_sealed_snapshots_are_hints_newest_first() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 1);
    for _ in 0..10 {
        sealed(&store, &ids);
    }
    let other = ContextKey::new(&ProjectContext::from_parts(ProjectContextParts {
        include_path_entries: vec![ProjectPathEntry::configured("/inc".into())],
        ..ProjectContextParts::default()
    }));
    let mut builder = store.begin_snapshot(&other).unwrap();
    store.add_members(&mut builder, &ids).unwrap();
    store.seal_snapshot(builder, &SEAL).unwrap();
    let headers = found(&store);
    assert_eq!(headers.len(), MAX_SNAPSHOT_CANDIDATES);
    assert!(
        headers
            .windows(2)
            .all(|pair| pair[0].publication_seq() > pair[1].publication_seq())
    );
    assert!(
        headers
            .iter()
            .all(|header| header.context() == context().digest())
    );
    let Lookup::Hit(others) = store.find_snapshots(&other).unwrap() else {
        panic!("sealed under the other context")
    };
    assert_eq!(others.len(), 1);
}

#[test]
fn a_seal_takes_discovery_evidence_only_of_its_purpose() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 1);
    let scope = DiscoveryScope::new(&DiscoveryInputs {
        directories: &[],
        exclusions: &[],
    });
    let discovery = store
        .publish_evidence(
            EvidenceTarget::Discovery(&scope),
            &EvidenceWrite {
                context: &context(),
                observations: &[],
            },
        )
        .unwrap()
        .evidence_id;
    let binding = binding_evidence(&store, "a", "/w/A.pas");
    let seal_with = |evidence| SnapshotSeal {
        discovery: Some(evidence),
        ..SEAL
    };
    let mut builder = store.begin_snapshot(&context()).unwrap();
    store.add_members(&mut builder, &ids).unwrap();
    assert_eq!(
        store.seal_snapshot(builder.clone(), &seal_with(binding)),
        Err(Bypass::SealRefused(SealRefusal::DiscoveryEvidence))
    );
    assert_eq!(
        store.seal_snapshot(builder.clone(), &seal_with(Digest::from_bytes([7; 32]))),
        Err(Bypass::MissingReference)
    );
    assert!(found(&store).is_empty(), "a refused seal wrote nothing");
    let header = store.seal_snapshot(builder, &seal_with(discovery)).unwrap();
    assert_eq!(header.discovery_evidence(), Some(discovery));
    assert_eq!(found(&store), vec![header.clone()]);
    assert_eq!(read_members(&store, &header).unwrap().len(), 1);
    // The sealed digest covers the discovery evidence.
    raw(directory.path())
        .execute(
            "UPDATE snapshots SET discovery_evidence = ?1",
            [binding.as_bytes().as_slice()],
        )
        .unwrap();
    let tampered = found(&store);
    assert_eq!(tampered[0].discovery_evidence(), Some(binding));
    assert_eq!(
        read_members(&store, &tampered[0]),
        Err(Miss::ChecksumMismatch)
    );
}

/// Publish `count` members whose source keys are each just under the
/// index key limit.
fn long_records(store: &pascal_cache::Store, count: usize) -> Vec<Digest> {
    (0..count)
        .map(|index| {
            let mut pad = MAX_INDEX_KEY_BYTES;
            let path = loop {
                let path = format!("/w/{index:03}{}.pas", "x".repeat(pad));
                if path_key(Path::new(&path)).len() <= MAX_INDEX_KEY_BYTES {
                    break path;
                }
                pad -= 1;
            };
            let record = member(&path, &[], Digest::from_bytes([0; 32]));
            store.publish_member_record(&record).unwrap().record_id
        })
        .collect()
}

#[test]
fn oversized_member_batches_write_nothing() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let mut builder = store.begin_snapshot(&context()).unwrap();
    let unchecked = vec![Digest::from_bytes([9; 32]); MAX_ROW_BATCH_ROWS + 1];
    assert_eq!(
        store.add_members(&mut builder, &unchecked),
        Err(Bypass::TooLarge(TooLarge::Rows {
            count: MAX_ROW_BATCH_ROWS + 1
        }))
    );
    let long = long_records(&store, MAX_ROW_BATCH_ROWS);
    let refused = store.add_members(&mut builder, &long);
    assert!(
        matches!(
            refused,
            Err(Bypass::TooLarge(TooLarge::Batch { bytes })) if bytes > MAX_ROW_BATCH_BYTES
        ),
        "{refused:?}"
    );
    assert_eq!(builder.revision(), 0);
    assert_eq!(member_rows(directory.path()), 0, "the batch wrote nothing");
    store
        .add_members(&mut builder, &long[..MAX_ROW_BATCH_ROWS / 2])
        .unwrap();
    // A snapshot one batch short of the member limit.
    raw(directory.path())
        .execute(
            "UPDATE snapshots SET member_count = ?1",
            [MAX_SNAPSHOT_MEMBERS as i64 - 1],
        )
        .unwrap();
    let ids = records(&store, "U", 2);
    assert_eq!(
        store.add_members(&mut builder, &ids),
        Err(Bypass::TooLarge(TooLarge::Members { added: 2 }))
    );
    assert_eq!(builder.revision(), 1);
    assert_eq!(member_rows(directory.path()), MAX_ROW_BATCH_ROWS as i64 / 2);
    store.add_members(&mut builder, &ids[..1]).unwrap();
}

#[test]
fn a_malformed_sealed_header_is_a_miss() {
    let directory = private_tempdir();
    let store = open(directory.path());
    sealed(&store, &records(&store, "U", 1));
    raw(directory.path())
        .execute("UPDATE snapshots SET coverage = x''", [])
        .unwrap();
    assert_eq!(
        store.find_snapshots(&context()).unwrap(),
        Lookup::Miss(Miss::Malformed(Malformed::Snapshot))
    );
}
