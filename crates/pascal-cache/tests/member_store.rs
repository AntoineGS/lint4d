#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{
    NAMESPACE, artifact, binding_evidence, context, identity, member, open, raw, read_record,
};
use pascal_cache::{
    Binding, BindingKind, BindingOutcome, Bypass, Digest, DiscoveryInputs, DiscoveryScope,
    EvidenceTarget, EvidenceWrite, IncompleteReason, Integrity, Lookup, MAX_MEMBER_BINDINGS,
    MAX_ROW_BATCH_BYTES, Malformed, MemberError, MemberHeader, MemberRecord, Miss, Namespace,
    Published, PublishedRecord, Store, StoreLimits, TooLarge,
};
use rusqlite::Connection;

#[test]
fn a_member_record_publishes_once_and_reads_back_verified() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let mut record = member("/w/Main.pas", &["/w/Shapes.pas"], evidence);
    record.header.artifact = Some(artifact(&store, "unit Main;"));
    let id = record.id().unwrap();
    assert_eq!(
        store.publish_member_record(&record).unwrap(),
        PublishedRecord {
            record_id: id,
            published: Published::Inserted
        }
    );
    assert_eq!(
        store.publish_member_record(&record).unwrap(),
        PublishedRecord {
            record_id: id,
            published: Published::AlreadyPresent
        }
    );
    assert_eq!(read_record(&store, id), Ok(record.clone()));
    let Lookup::Hit(header) = store.open_member_record(id).unwrap() else {
        panic!("published")
    };
    assert_eq!(
        (header.record_id(), header.binding_count(), header.member()),
        (id, 1, &record.header)
    );
    assert_eq!(
        store
            .open_member_record(Digest::from_bytes([9; 32]))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
}

#[test]
fn a_record_requires_live_references_of_the_right_purpose() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let valid = member("/w/Main.pas", &["/w/Shapes.pas"], evidence);
    let mut missing_artifact = valid.clone();
    missing_artifact.header.artifact = Some(Digest::from_bytes([8; 32]));
    assert_eq!(
        store.publish_member_record(&missing_artifact),
        Err(Bypass::MissingReference)
    );
    let missing_evidence = member(
        "/w/Main.pas",
        &["/w/Shapes.pas"],
        Digest::from_bytes([8; 32]),
    );
    assert_eq!(
        store.publish_member_record(&missing_evidence),
        Err(Bypass::MissingReference)
    );
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
    assert_eq!(
        store.publish_member_record(&member("/w/Main.pas", &["/w/Shapes.pas"], discovery)),
        Err(Bypass::InvalidRecord(MemberError::EvidencePurpose)),
        "discovery evidence cannot prove a binding"
    );
    let mut misdirected = valid.clone();
    misdirected.header.artifact = Some(artifact(&store, "unit Main;"));
    misdirected.header.member_evidence = Some(evidence);
    assert_eq!(
        store.publish_member_record(&misdirected),
        Err(Bypass::InvalidRecord(MemberError::EvidencePurpose)),
        "member evidence must be artifact evidence for the member's artifact"
    );
    let mut unordered = valid.clone();
    unordered.bindings.push(unordered.bindings[0].clone());
    assert_eq!(
        store.publish_member_record(&unordered),
        Err(Bypass::InvalidRecord(MemberError::BindingOrder {
            ordinal: 1
        }))
    );
    raw(directory.path())
        .execute(
            "UPDATE evidence_sets SET lifecycle = 2 WHERE evidence_id = ?1",
            [evidence.as_bytes().as_slice()],
        )
        .unwrap();
    assert_eq!(store.publish_member_record(&valid), Err(Bypass::Retiring));
    let records: i64 = raw(directory.path())
        .query_row("SELECT count(*) FROM member_records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(records, 0, "nothing was published");
}

type Tamper = Box<dyn Fn(&Connection, &MemberRecord)>;

#[test]
fn tampered_records_are_never_a_verified_read() {
    let swap_ordinals = "UPDATE member_bindings SET ordinal = 99 WHERE ordinal = 0; \
                         UPDATE member_bindings SET ordinal = 0 WHERE ordinal = 1; \
                         UPDATE member_bindings SET ordinal = 1 WHERE ordinal = 99;";
    let cases: [(&str, Tamper, Miss); 6] = [
        (
            "a well-formed binding swapped in",
            Box::new(|db, record| {
                let mut moved = record.bindings[1].clone();
                moved.occurrence = 5;
                db.execute(
                    "UPDATE member_bindings SET payload = ?1 WHERE ordinal = 1",
                    [moved.encoded()],
                )
                .unwrap();
            }),
            Miss::ChecksumMismatch,
        ),
        (
            "a target column that disagrees with its payload",
            Box::new(|db, _| {
                db.execute(
                    "UPDATE member_bindings SET target_key = \
                     (SELECT target_key FROM member_bindings WHERE ordinal = 0) WHERE ordinal = 1",
                    [],
                )
                .unwrap();
            }),
            Miss::Malformed(Malformed::Member),
        ),
        (
            "reordered rows",
            Box::new(move |db, _| db.execute_batch(swap_ordinals).unwrap()),
            Miss::Malformed(Malformed::Member),
        ),
        (
            "a deleted row",
            Box::new(|db, _| {
                db.execute("DELETE FROM member_bindings WHERE ordinal = 1", [])
                    .unwrap();
            }),
            Miss::Malformed(Malformed::Count),
        ),
        (
            "an appended row",
            Box::new(|db, _| {
                db.execute(
                    "INSERT INTO member_bindings SELECT namespace_id, reset_epoch, record_id, 2, \
                     kind, outcome, target_key, evidence_id, payload FROM member_bindings \
                     WHERE ordinal = 0",
                    [],
                )
                .unwrap();
            }),
            Miss::Malformed(Malformed::Count),
        ),
        (
            "a rewritten header",
            Box::new(|db, record| {
                let header = MemberHeader {
                    fallback_unit_name: Some("Other".into()),
                    ..record.header.clone()
                };
                db.execute("UPDATE member_records SET header = ?1", [header.encoded()])
                    .unwrap();
            }),
            Miss::ChecksumMismatch,
        ),
    ];
    for (label, tamper, miss) in cases {
        let directory = private_tempdir();
        let store = open(directory.path());
        let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
        let record = member("/w/Main.pas", &["/w/A.pas", "/w/B.pas"], evidence);
        let id = store.publish_member_record(&record).unwrap().record_id;
        tamper(&raw(directory.path()), &record);
        assert_eq!(read_record(&store, id), Err(miss), "{label}");
    }
}

#[test]
fn a_record_retired_or_deleted_between_batches_is_a_miss() {
    for (label, change, miss) in [
        (
            "retired",
            "UPDATE member_records SET lifecycle = 2",
            Miss::Retired,
        ),
        (
            "deleted",
            "DELETE FROM member_bindings; DELETE FROM member_records",
            Miss::Vanished,
        ),
    ] {
        let directory = private_tempdir();
        let store = open(directory.path());
        let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
        let targets: Vec<String> = (0..300).map(|index| format!("/w/T{index}.pas")).collect();
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
        let record = member("/w/Main.pas", &targets, evidence);
        let id = store.publish_member_record(&record).unwrap().record_id;
        let Lookup::Hit(header) = store.open_member_record(id).unwrap() else {
            panic!("published")
        };
        let mut reader = store.read_member_record(&header);
        let Lookup::Hit(Some(first)) = reader.next_batch().unwrap() else {
            panic!("{label}: first batch")
        };
        assert_eq!(first.bindings().len(), 256);
        raw(directory.path()).execute_batch(change).unwrap();
        assert!(
            matches!(reader.next_batch().unwrap(), Lookup::Miss(found) if found == miss),
            "{label}"
        );
        assert!(!reader.is_verified());
    }
}

#[test]
fn a_conflicting_record_is_an_integrity_failure() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
    let record = member("/w/Main.pas", &["/w/Shapes.pas"], evidence);
    store.publish_member_record(&record).unwrap();
    raw(directory.path())
        .execute(
            "UPDATE member_records SET content_bytes = content_bytes + 1",
            [],
        )
        .unwrap();
    assert_eq!(
        store.publish_member_record(&record),
        Err(Bypass::Integrity(Integrity::ConflictingRecord))
    );
}

#[test]
fn oversized_records_are_not_persisted() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let mut record = member("/w/Main.pas", &[], Digest::from_bytes([0; 32]));
    record.bindings = (0..=MAX_MEMBER_BINDINGS as u64)
        .map(|occurrence| Binding {
            kind: BindingKind::Include,
            occurrence,
            outcome: BindingOutcome::Incomplete(IncompleteReason::Unsupported),
            evidence: None,
        })
        .collect();
    assert_eq!(
        store.publish_member_record(&record),
        Err(Bypass::TooLarge(TooLarge::Bindings {
            count: MAX_MEMBER_BINDINGS + 1
        }))
    );
    let mut long = member("/w/Main.pas", &[], Digest::from_bytes([0; 32]));
    long.header.source = identity(&format!("/{}", "x".repeat(5000)));
    assert!(matches!(
        store.publish_member_record(&long),
        Err(Bypass::TooLarge(TooLarge::SourceKey { .. }))
    ));
}

#[test]
fn a_record_retired_before_its_final_check_is_a_miss() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let record = member("/w/Main.pas", &[], Digest::from_bytes([0; 32]));
    let id = store.publish_member_record(&record).unwrap().record_id;
    let Lookup::Hit(header) = store.open_member_record(id).unwrap() else {
        panic!("published")
    };
    let mut reader = store.read_member_record(&header);
    raw(directory.path())
        .execute("UPDATE member_records SET lifecycle = 2", [])
        .unwrap();
    assert!(matches!(
        reader.next_batch().unwrap(),
        Lookup::Miss(Miss::Retired)
    ));
    assert!(!reader.is_verified());
}

#[test]
fn binding_columns_that_disagree_with_their_payload_are_never_a_verified_read() {
    let cases: [(&str, &str, Miss); 5] = [
        (
            "kind",
            "UPDATE member_bindings SET kind = 2 WHERE ordinal = 0",
            Miss::Malformed(Malformed::Member),
        ),
        (
            "outcome",
            "UPDATE member_bindings SET outcome = 2 WHERE ordinal = 1",
            Miss::Malformed(Malformed::Member),
        ),
        (
            "evidence removed",
            "UPDATE member_bindings SET evidence_id = NULL WHERE ordinal = 1",
            Miss::Malformed(Malformed::Member),
        ),
        (
            "evidence replaced",
            "UPDATE member_bindings SET evidence_id = zeroblob(32) WHERE ordinal = 1",
            Miss::Malformed(Malformed::Member),
        ),
        (
            "content bytes",
            "UPDATE member_records SET content_bytes = content_bytes + 1",
            Miss::ChecksumMismatch,
        ),
    ];
    for (label, tamper, miss) in cases {
        let directory = private_tempdir();
        let store = open(directory.path());
        let evidence = binding_evidence(&store, "shapes", "/w/Shapes.pas");
        let mut record = member("/w/Main.pas", &["/w/Shapes.pas"], evidence);
        record.bindings.push(Binding {
            kind: BindingKind::Import,
            occurrence: 1,
            outcome: BindingOutcome::Unavailable,
            evidence: Some(evidence),
        });
        let id = store.publish_member_record(&record).unwrap().record_id;
        assert_eq!(read_record(&store, id), Ok(record), "{label}: untouched");
        raw(directory.path()).execute(tamper, []).unwrap();
        assert_eq!(read_record(&store, id), Err(miss), "{label}");
    }
}

#[test]
fn record_columns_that_disagree_with_the_header_are_not_admitted() {
    for (label, tamper) in [
        ("source key", "UPDATE member_records SET source_key = x'00'"),
        (
            "artifact",
            "UPDATE member_records SET artifact_id = zeroblob(32)",
        ),
        (
            "member evidence",
            "UPDATE member_records SET member_evidence = zeroblob(32)",
        ),
    ] {
        let directory = private_tempdir();
        let store = open(directory.path());
        let mut record = member("/w/Main.pas", &[], Digest::from_bytes([0; 32]));
        record.header.artifact = Some(artifact(&store, "unit Main;"));
        let id = store.publish_member_record(&record).unwrap().record_id;
        assert!(
            matches!(store.open_member_record(id).unwrap(), Lookup::Hit(_)),
            "{label}: untouched"
        );
        raw(directory.path()).execute(tamper, []).unwrap();
        assert_eq!(
            store.open_member_record(id).unwrap(),
            Lookup::Miss(Miss::Malformed(Malformed::Member)),
            "{label}"
        );
    }
}

#[test]
fn a_retired_artifact_or_record_refuses_publication() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let mut record = member("/w/Main.pas", &[], Digest::from_bytes([0; 32]));
    record.header.artifact = Some(artifact(&store, "unit Main;"));
    raw(directory.path())
        .execute("UPDATE artifacts SET lifecycle = 2", [])
        .unwrap();
    assert_eq!(store.publish_member_record(&record), Err(Bypass::Retiring));
    let plain = member("/w/Plain.pas", &[], Digest::from_bytes([0; 32]));
    let id = store.publish_member_record(&plain).unwrap().record_id;
    assert!(store.retire_member_record(id).unwrap());
    assert_eq!(
        store.publish_member_record(&plain),
        Err(Bypass::Retiring),
        "a retired record is never revived by republishing"
    );
    assert_eq!(
        store.open_member_record(id).unwrap(),
        Lookup::Miss(Miss::Retired)
    );
}

/// Spec §10: the scratch is reserved from input sizes before anything is
/// encoded. A record whose bound exceeds the scratch is refused at once;
/// encoding first would have reached the over-long target key and returned
/// `TooLarge::IndexKey` instead.
#[test]
fn a_record_whose_bound_exceeds_the_scratch_is_refused_before_encoding() {
    let long = format!("/{}", "x".repeat(5000));
    let targets = vec![long.as_str(); 128];
    let record = member("/w/Main.pas", &targets, Digest::from_bytes([1; 32]));
    let small = private_tempdir();
    let store = Store::open(
        small.path(),
        &Namespace::new(Digest::from_bytes([NAMESPACE; 32])),
        StoreLimits {
            scratch_bytes: MAX_ROW_BATCH_BYTES as u64,
            ..StoreLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        store.publish_member_record(&record),
        Err(Bypass::ScratchExhausted)
    );
    assert_eq!(store.scratch_in_use(), 0);
    let roomy = private_tempdir();
    let store = open(roomy.path());
    assert!(matches!(
        store.publish_member_record(&record),
        Err(Bypass::TooLarge(TooLarge::IndexKey { ordinal: 0, .. }))
    ));
    assert_eq!(
        store.scratch_in_use(),
        0,
        "the refusal releases its scratch"
    );
}

#[test]
fn an_identical_republish_takes_the_next_sequence() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let record = member("/w/Main.pas", &[], Digest::from_bytes([0; 32]));
    assert_eq!(
        store.publish_member_record(&record).unwrap().published,
        Published::Inserted
    );
    let sequence = || -> i64 {
        raw(directory.path())
            .query_row("SELECT publication_seq FROM member_records", [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    let first = sequence();
    assert!(first >= 1);
    assert_eq!(
        store.publish_member_record(&record).unwrap().published,
        Published::AlreadyPresent
    );
    assert!(sequence() > first, "the republish committed its recency");
}
