#![cfg(unix)]

mod common;
use common::private_tempdir;

use pascal_cache::{
    ArtifactHeader, ArtifactKeyRef, ArtifactRow, ArtifactWrite, Bypass, DATABASE_FILE, Digest,
    IndexHit, LocalArtifactKey, Lookup, Malformed, Miss, Namespace, RowBatch, SourceContent, Store,
    StoreLimits,
};
use pascal_project::{ConditionalContext, SourceOrigin};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::Duration;

fn open(directory: &Path, limits: StoreLimits) -> Store {
    Store::open(
        directory,
        &Namespace::new(Digest::from_bytes([1; 32])),
        limits,
    )
    .unwrap()
}

fn key(text: &str) -> LocalArtifactKey {
    LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    )
}

fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

fn publish(store: &Store, key: &LocalArtifactKey, payloads: &[Vec<u8>]) -> ArtifactHeader {
    let rows: Vec<ArtifactRow<'_>> = payloads
        .iter()
        .map(|payload| ArtifactRow {
            row_kind: 1,
            index_key: Some(b"name"),
            payload,
        })
        .collect();
    let write = ArtifactWrite {
        key: ArtifactKeyRef::local(key),
        supplemental: b"s",
        rows: &rows,
    };
    store.publish_artifact(&write).unwrap();
    match store.open_artifact(ArtifactKeyRef::local(key)).unwrap() {
        Lookup::Hit(header) => header,
        miss => panic!("{miss:?}"),
    }
}

fn small(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|index| format!("row {index}").into_bytes())
        .collect()
}

/// Read every batch; return the batch sizes, or the first miss.
fn read_all(store: &Store, header: &ArtifactHeader) -> Result<Vec<usize>, Miss> {
    let mut reader = store.read_artifact(header);
    let mut sizes = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => sizes.push(batch.rows().len()),
            Lookup::Hit(None) => {
                assert!(reader.is_verified());
                return Ok(sizes);
            }
            Lookup::Miss(miss) => {
                assert!(!reader.is_verified());
                return Err(miss);
            }
        }
    }
}

#[test]
fn rows_round_trip_in_batches_of_at_most_256() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let payloads = small(600);
    let header = publish(&store, &key("unit A;"), &payloads);
    let mut reader = store.read_artifact(&header);
    let mut seen = Vec::new();
    while let Lookup::Hit(Some(batch)) = reader.next_batch().unwrap() {
        assert!(batch.rows().len() <= 256);
        for row in batch.rows() {
            assert_eq!(row.ordinal, seen.len() as u64);
            assert_eq!(row.row_kind, 1);
            assert_eq!(row.index_key.as_deref(), Some(b"name".as_slice()));
            seen.push(row.payload.clone());
        }
    }
    assert!(reader.is_verified());
    assert_eq!(seen, payloads);
    assert_eq!(read_all(&store, &header), Ok(vec![256, 256, 88]));
}

#[test]
fn batches_stay_within_one_mebibyte() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let payloads = vec![vec![1u8; 600 * 1024]; 3];
    let header = publish(&store, &key("unit A;"), &payloads);
    assert_eq!(read_all(&store, &header), Ok(vec![1, 1, 1]));
}

#[test]
fn an_empty_artifact_verifies_at_once() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &[]);
    assert_eq!(read_all(&store, &header), Ok(vec![]));
}

#[test]
fn tampered_payload_is_a_checksum_miss() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(10));
    raw(root.path())
        .execute(
            "UPDATE artifact_rows SET payload = x'00' WHERE ordinal = 5",
            [],
        )
        .unwrap();
    assert_eq!(read_all(&store, &header), Err(Miss::ChecksumMismatch));
}

#[test]
fn fact_checksum_and_counts_are_verified_at_the_end() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let key = key("unit A;");
    publish(&store, &key, &small(10));
    let connection = raw(root.path());
    let reopen = || match store.open_artifact(ArtifactKeyRef::local(&key)).unwrap() {
        Lookup::Hit(header) => header,
        miss => panic!("{miss:?}"),
    };
    for (tamper, expected) in [
        (
            "UPDATE artifacts SET fact_checksum = zeroblob(32)",
            Miss::ChecksumMismatch,
        ),
        (
            "UPDATE artifacts SET content_bytes = content_bytes + 1",
            Miss::ChecksumMismatch,
        ),
        (
            "UPDATE artifacts SET row_count = 11",
            Miss::Malformed(Malformed::Count),
        ),
        (
            "UPDATE artifacts SET row_count = 9",
            Miss::Malformed(Malformed::Count),
        ),
        (
            "DELETE FROM artifact_rows WHERE ordinal = 3",
            Miss::Malformed(Malformed::Ordinal),
        ),
    ] {
        connection.execute(tamper, []).unwrap();
        assert_eq!(read_all(&store, &reopen()), Err(expected), "{tamper}");
        // Restore by republishing into a fresh copy of the rows.
        connection.execute("DELETE FROM artifact_rows", []).unwrap();
        connection.execute("DELETE FROM artifacts", []).unwrap();
        publish(&store, &key, &small(10));
    }
}

#[test]
fn oversized_rows_are_misses_before_they_are_copied() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    for (text, bytes) in [("unit A;", 3 * 1024 * 1024), ("unit B;", 1536 * 1024)] {
        let header = publish(&store, &key(text), &small(2));
        let connection = raw(root.path());
        connection
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        connection
            .execute(
                "UPDATE artifact_rows SET payload = zeroblob(?1) WHERE artifact_ref = \
                 (SELECT artifact_ref FROM artifacts WHERE artifact_id = ?2) AND ordinal = 1",
                params![bytes, header.artifact_id().as_bytes().as_slice()],
            )
            .unwrap();
        assert_eq!(read_all(&store, &header), Err(Miss::Oversized), "{bytes}");
        assert_eq!(store.scratch_in_use(), 0);
    }
}

#[test]
fn scratch_is_shared_and_bounded() {
    let root = private_tempdir();
    let limits = StoreLimits {
        scratch_bytes: 1024 * 1024,
        ..StoreLimits::default()
    };
    let store = open(root.path(), limits);
    let a = publish(&store, &key("unit A;"), &vec![vec![1u8; 600 * 1024]; 2]);
    let b = publish(&store, &key("unit B;"), &small(3));
    let mut first = store.read_artifact(&a);
    let held: RowBatch = match first.next_batch().unwrap() {
        Lookup::Hit(Some(batch)) => batch,
        other => panic!("{other:?}"),
    };
    assert_eq!(store.scratch_in_use(), 600 * 1024 + 4);
    let mut second = store.read_artifact(&b);
    assert_eq!(second.next_batch().err(), Some(Bypass::ScratchExhausted));
    drop(held);
    assert_eq!(store.scratch_in_use(), 0);
    assert!(matches!(second.next_batch().unwrap(), Lookup::Hit(Some(_))));
}

#[test]
fn an_artifact_removed_between_batches_vanishes() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(300));
    let mut reader = store.read_artifact(&header);
    assert!(matches!(reader.next_batch().unwrap(), Lookup::Hit(Some(_))));
    let connection = raw(root.path());
    connection.execute("DELETE FROM artifact_rows", []).unwrap();
    connection.execute("DELETE FROM artifacts", []).unwrap();
    assert_eq!(reader.next_batch().unwrap(), Lookup::Miss(Miss::Vanished));
    assert!(!reader.is_verified());
}

#[test]
fn readers_hold_no_snapshot_between_batches() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(300));
    let mut reader = store.read_artifact(&header);
    let _batch = reader.next_batch().unwrap();
    // A full checkpoint succeeds only if no reader still pins the WAL.
    let busy: i64 = raw(root.path())
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .unwrap();
    assert_eq!(busy, 0);
}

/// Drain `reader`, returning every ordinal received, in order.
fn drain(reader: &mut pascal_cache::ArtifactReader<'_>) -> Vec<u64> {
    let mut ordinals = Vec::new();
    while let Lookup::Hit(Some(batch)) = reader.next_batch().unwrap() {
        ordinals.extend(batch.rows().iter().map(|row| row.ordinal));
    }
    ordinals
}

#[test]
fn a_bypass_mid_batch_leaves_the_witness_where_the_caller_is() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(300));
    store.fail_row_reads_after_for_test(Some(10));
    let mut reader = store.read_artifact(&header);
    assert!(matches!(reader.next_batch(), Err(Bypass::Sqlite { .. })));
    assert!(!reader.is_verified());
    store.fail_row_reads_after_for_test(None);
    assert_eq!(drain(&mut reader), (0..300).collect::<Vec<u64>>());
    assert!(reader.is_verified());
}

#[test]
fn a_retry_after_a_bypass_resumes_at_the_undelivered_ordinal() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(600));
    let mut reader = store.read_artifact(&header);
    let mut received = Vec::new();
    match reader.next_batch().unwrap() {
        Lookup::Hit(Some(batch)) => received.extend(batch.rows().iter().map(|row| row.ordinal)),
        other => panic!("{other:?}"),
    }
    assert_eq!(received.len(), 256);
    store.fail_row_reads_after_for_test(Some(10));
    let _ = reader.next_batch();
    received.extend(drain(&mut reader));
    assert_eq!(received, (0..600).collect::<Vec<u64>>());
    assert!(reader.is_verified());
}

#[test]
fn a_row_beyond_the_header_count_in_a_full_batch_is_malformed() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(256));
    raw(root.path())
        .execute(
            "INSERT INTO artifact_rows \
             (artifact_ref, ordinal, scope_ref, row_kind, index_key, payload) \
             SELECT artifact_ref, 256, scope_ref, 1, NULL, x'00' FROM artifact_rows \
             WHERE ordinal = 0",
            [],
        )
        .unwrap();
    assert_eq!(
        read_all(&store, &header),
        Err(Miss::Malformed(Malformed::Count))
    );
}

/// 13f: a batch whose query ran out of rows read every row at or past its
/// start, so it already showed there is none past the count: the reader
/// verifies without another transaction. A full batch still asks.
#[test]
fn a_last_batch_that_ran_out_of_rows_needs_no_trailing_query() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    for (count, leases) in [(10, 1), (256, 2), (300, 2)] {
        let header = publish(&store, &key(&format!("unit A{count};")), &small(count));
        let before = pascal_cache::store_operations_on_this_thread_for_test();
        let sizes = read_all(&store, &header).unwrap();
        assert_eq!(sizes.iter().sum::<usize>(), count);
        assert_eq!(
            pascal_cache::store_operations_on_this_thread_for_test() - before,
            leases,
            "{count} rows"
        );
    }
}

fn publish_named(store: &Store, text: &str, names: &[&[u8]]) -> Digest {
    let key = key(text);
    let rows: Vec<ArtifactRow<'_>> = names
        .iter()
        .map(|name| ArtifactRow {
            row_kind: 1,
            index_key: Some(name),
            payload: b"p",
        })
        .collect();
    let write = ArtifactWrite {
        key: ArtifactKeyRef::local(&key),
        supplemental: b"",
        rows: &rows,
    };
    store.publish_artifact(&write).unwrap();
    key.digest()
}

#[test]
fn index_pages_are_stable_and_complete() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let mut expected = Vec::new();
    for index in 0..30 {
        let names: Vec<&[u8]> = vec![b"tfoo"; 20];
        let id = publish_named(&store, &format!("unit U{index};"), &names);
        expected.extend((0..20).map(|ordinal| IndexHit {
            artifact_id: id,
            ordinal,
        }));
    }
    // Hits come in the store's publication order of their artifacts.
    let mut seen: Vec<IndexHit> = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = store.find_by_index(1, b"tfoo", after, 256).unwrap();
        assert!(page.hits.len() <= 256);
        seen.extend(&page.hits);
        pages += 1;
        if page.next.is_none() {
            break;
        }
        after = page.next;
        if pages == 1 {
            // A new artifact published mid-scan sorts after the cursor, so
            // it appears once and never duplicates earlier hits.
            publish_named(&store, "unit Late;", &[b"tfoo"]);
        }
    }
    let late = key("unit Late;").digest();
    let seen_without_late: Vec<_> = seen
        .iter()
        .copied()
        .filter(|hit| hit.artifact_id != late)
        .collect();
    assert_eq!(seen_without_late, expected);
    assert_eq!(seen.last().map(|hit| hit.artifact_id), Some(late));
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "no duplicates");
    assert_eq!(pages, 3);
}

#[test]
fn index_lookup_is_binary_and_scoped_to_live_artifacts() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let foo = publish_named(&store, "unit A;", &[b"tfoo", b"tFoo"]);
    let retired = publish_named(&store, "unit B;", &[b"tfoo"]);
    raw(root.path())
        .execute(
            "UPDATE artifacts SET lifecycle = 2 WHERE artifact_id = ?1",
            [retired.as_bytes().as_slice()],
        )
        .unwrap();
    let page = store.find_by_index(1, b"tfoo", None, 10).unwrap();
    assert_eq!(
        page.hits,
        vec![IndexHit {
            artifact_id: foo,
            ordinal: 0
        }]
    );
    assert_eq!(page.next, None);
    assert!(
        store
            .find_by_index(2, b"tfoo", None, 10)
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn index_lookup_uses_the_index_without_sorting() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    publish_named(&store, "unit A;", &[b"tfoo"]);
    let plan: Vec<String> = raw(root.path())
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            pascal_cache::INDEX_QUERY_FOR_TEST
        ))
        .unwrap()
        .query_map(
            params![
                [1u8; 32].as_slice(),
                0i64,
                1i64,
                b"tfoo".as_slice(),
                0i64,
                -1i64,
                257i64,
                1i64,
                1i64
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let plan = plan.join("\n");
    assert!(
        plan.contains("artifact_rows_by_index_key (scope_ref=? AND row_kind=? AND index_key=?"),
        "{plan}"
    );
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
}

fn namespace_id(fingerprint: u8) -> [u8; 32] {
    *Namespace::new(Digest::from_bytes([fingerprint; 32]))
        .id()
        .as_bytes()
}

/// l4d-rpfc: the name index is scope-led, so another scope's rows under the
/// same key are never visited, and never returned.
#[test]
fn index_lookup_never_visits_another_scopes_rows() {
    let root = private_tempdir();
    let ours = open(root.path(), StoreLimits::default()); // Namespace [1; 32]
    let theirs = Store::open(
        root.path(),
        &Namespace::new(Digest::from_bytes([8; 32])),
        StoreLimits::default(),
    )
    .unwrap();
    let mine = publish_named(&ours, "unit A;", &[b"tfoo"]);
    let id = namespace_id(1);
    let steps = |directory: &Path| -> i32 {
        let connection = raw(directory);
        let scope_ref: i64 = connection
            .query_row(
                "SELECT scope_ref FROM scopes WHERE namespace_id = ?1 AND reset_epoch = 0",
                [id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let mut statement = connection
            .prepare(pascal_cache::INDEX_QUERY_FOR_TEST)
            .unwrap();
        let rows = statement
            .query_map(
                params![
                    id.as_slice(),
                    0i64,
                    1i64,
                    b"tfoo".as_slice(),
                    0i64,
                    -1i64,
                    257i64,
                    1i64,
                    scope_ref
                ],
                |_| Ok(()),
            )
            .unwrap()
            .count();
        assert_eq!(rows, 1);
        statement.get_status(rusqlite::StatementStatus::VmStep)
    };
    // One row of theirs already follows ours in the index, so both counts
    // include the step that reads past the end of our range.
    publish_named(&theirs, "unit B;", &[b"tfoo"]);
    let before = steps(root.path());
    let names: Vec<&[u8]> = vec![b"tfoo"; 20];
    for index in 0..300 {
        publish_named(&theirs, &format!("unit B{index};"), &names);
    }
    assert_eq!(
        steps(root.path()),
        before,
        "6,000 rows of another scope were visited"
    );
    let page = ours.find_by_index(1, b"tfoo", None, 256).unwrap();
    assert_eq!(
        page.hits,
        vec![IndexHit {
            artifact_id: mine,
            ordinal: 0
        }]
    );
    assert_eq!(page.next, None);
}

/// l4d-rpfc: `scope_ref` has no foreign key. A row whose ref names another
/// scope, or none, is hidden from both scopes, never returned to the wrong one.
#[test]
fn a_wrong_scope_ref_hides_a_row_and_never_leaks_it() {
    let root = private_tempdir();
    let ours = open(root.path(), StoreLimits::default());
    let theirs = Store::open(
        root.path(),
        &Namespace::new(Digest::from_bytes([8; 32])),
        StoreLimits::default(),
    )
    .unwrap();
    let mine = publish_named(&ours, "unit A;", &[b"tfoo"]);
    let other = publish_named(&theirs, "unit B;", &[b"tfoo"]);
    let theirs_ref: i64 = raw(root.path())
        .query_row(
            "SELECT scope_ref FROM scopes WHERE namespace_id = ?1",
            [namespace_id(8).as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let only = |id: Digest| {
        vec![IndexHit {
            artifact_id: id,
            ordinal: 0,
        }]
    };
    for wrong in [theirs_ref, 1_000_000] {
        raw(root.path())
            .execute(
                "UPDATE artifact_rows SET scope_ref = ?1 WHERE artifact_ref = \
                 (SELECT artifact_ref FROM artifacts WHERE artifact_id = ?2)",
                params![wrong, mine.as_bytes().as_slice()],
            )
            .unwrap();
        assert!(
            ours.find_by_index(1, b"tfoo", None, 256)
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            theirs.find_by_index(1, b"tfoo", None, 256).unwrap().hits,
            only(other),
            "{wrong}"
        );
    }
}

/// l4d-rpfc: a scope with no `scopes` row never published an artifact, so
/// its lookup is the empty last page.
#[test]
fn index_lookup_in_a_scope_without_artifacts_is_an_empty_last_page() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let page = store.find_by_index(1, b"tfoo", None, 256).unwrap();
    assert!(page.hits.is_empty());
    assert_eq!(page.next, None);
}

#[test]
fn supplemental_is_trusted_only_once_the_read_verifies() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(3));
    let mut reader = store.read_artifact(&header);
    assert_eq!(reader.supplemental(), None);
    while let Lookup::Hit(Some(_)) = reader.next_batch().unwrap() {
        assert_eq!(reader.supplemental(), None);
    }
    assert!(reader.is_verified());
    assert_eq!(reader.supplemental(), Some(b"s".as_slice()));
}

#[test]
fn tampered_supplemental_is_a_checksum_miss_and_stays_untrusted() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let stored = publish(&store, &key("unit A;"), &small(3));
    raw(root.path())
        .execute("UPDATE artifacts SET supplemental = x'01'", [])
        .unwrap();
    let header = match store
        .open_artifact(ArtifactKeyRef::local(&key("unit A;")))
        .unwrap()
    {
        Lookup::Hit(header) => header,
        miss => panic!("{miss:?}"),
    };
    assert_ne!(header, stored);
    let mut reader = store.read_artifact(&header);
    let miss = loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(_)) => {}
            Lookup::Hit(None) => panic!("a tampered supplemental must not verify"),
            Lookup::Miss(miss) => break miss,
        }
    };
    assert_eq!(miss, Miss::ChecksumMismatch);
    assert_eq!(reader.supplemental(), None);
}

#[test]
fn a_header_from_another_store_is_fenced() {
    let first = private_tempdir();
    let second = private_tempdir();
    let store = open(first.path(), StoreLimits::default());
    let other = Store::open(
        second.path(),
        &Namespace::new(Digest::from_bytes([2; 32])),
        StoreLimits::default(),
    )
    .unwrap();
    let header = publish(&other, &key("unit A;"), &small(3));
    let mut reader = store.read_artifact(&header);
    assert_eq!(reader.next_batch(), Err(Bypass::Fenced));
    assert!(!reader.is_verified());
}

const MIXED: [ArtifactRow<'static>; 3] = [
    ArtifactRow {
        row_kind: 1,
        index_key: None,
        payload: b"a",
    },
    ArtifactRow {
        row_kind: 1,
        index_key: Some(b""),
        payload: b"b",
    },
    ArtifactRow {
        row_kind: 2,
        index_key: Some(b"name"),
        payload: b"c",
    },
];

/// Publish `rows`, apply `tamper` through a raw connection with CHECK
/// constraints off, then read the artifact to the end.
fn read_tampered(rows: &[ArtifactRow<'_>], tamper: &str) -> Result<Vec<usize>, Miss> {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let key = key("unit A;");
    let write = ArtifactWrite {
        key: ArtifactKeyRef::local(&key),
        supplemental: b"s",
        rows,
    };
    store.publish_artifact(&write).unwrap();
    let Lookup::Hit(header) = store.open_artifact(ArtifactKeyRef::local(&key)).unwrap() else {
        panic!("a published artifact opens");
    };
    let connection = raw(root.path());
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    connection.execute_batch(tamper).unwrap();
    read_all(&store, &header)
}

#[test]
fn tampered_row_fields_are_misses() {
    assert_eq!(read_tampered(&MIXED, ""), Ok(vec![3]));
    for (tamper, expected) in [
        (
            "UPDATE artifact_rows SET row_kind = 3 WHERE ordinal = 2",
            Miss::ChecksumMismatch,
        ),
        // No index key and an empty one are different rows.
        (
            "UPDATE artifact_rows SET index_key = x'' WHERE ordinal = 0",
            Miss::ChecksumMismatch,
        ),
        (
            "UPDATE artifact_rows SET index_key = NULL WHERE ordinal = 1",
            Miss::ChecksumMismatch,
        ),
        (
            "UPDATE artifact_rows SET row_kind = 0 WHERE ordinal = 2",
            Miss::ChecksumMismatch,
        ),
        (
            "UPDATE artifact_rows SET row_kind = 256 WHERE ordinal = 2",
            Miss::Malformed(Malformed::Row),
        ),
        (
            "UPDATE artifact_rows SET row_kind = -1 WHERE ordinal = 2",
            Miss::Malformed(Malformed::Row),
        ),
    ] {
        assert_eq!(read_tampered(&MIXED, tamper), Err(expected), "{tamper}");
    }
}

/// Rows of `id`'s artifact, as `artifact_ref = …` SQL.
const ROWS_OF: &str = "artifact_ref = (SELECT artifact_ref FROM artifacts WHERE artifact_id = ?1)";

#[test]
fn a_row_copied_from_another_artifact_is_a_checksum_miss() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let a = publish(&store, &key("unit A;"), &small(3));
    let other: Vec<Vec<u8>> = (0..3)
        .map(|index| format!("B {index}").into_bytes())
        .collect();
    let b = publish(&store, &key("unit B;"), &other);
    raw(root.path())
        .execute(
            &format!(
                "UPDATE artifact_rows SET (row_kind, index_key, payload) = \
                 (SELECT row_kind, index_key, payload FROM artifact_rows \
                  WHERE {} AND ordinal = 1) \
                 WHERE {ROWS_OF} AND ordinal = 1",
                ROWS_OF.replace("?1", "?2")
            ),
            params![
                a.artifact_id().as_bytes().as_slice(),
                b.artifact_id().as_bytes().as_slice()
            ],
        )
        .unwrap();
    assert_eq!(read_all(&store, &a), Err(Miss::ChecksumMismatch));
    assert_eq!(read_all(&store, &b), Ok(vec![3]));
}

/// A row of another artifact moved under this one, at the ordinal of a
/// deleted row, is not this artifact's row.
#[test]
fn a_row_moved_from_another_artifact_is_a_checksum_miss() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let a = publish(&store, &key("unit A;"), &small(3));
    let other: Vec<Vec<u8>> = (0..3)
        .map(|index| format!("B {index}").into_bytes())
        .collect();
    let b = publish(&store, &key("unit B;"), &other);
    let connection = raw(root.path());
    let a_id = a.artifact_id();
    connection
        .execute(
            &format!("DELETE FROM artifact_rows WHERE {ROWS_OF} AND ordinal = 2"),
            [a_id.as_bytes().as_slice()],
        )
        .unwrap();
    connection
        .execute(
            &format!(
                "UPDATE artifact_rows SET artifact_ref = \
                 (SELECT artifact_ref FROM artifacts WHERE artifact_id = ?2) \
                 WHERE {ROWS_OF} AND ordinal = 2"
            ),
            params![
                b.artifact_id().as_bytes().as_slice(),
                a_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
    assert_eq!(read_all(&store, &a), Err(Miss::ChecksumMismatch));
    assert_eq!(read_all(&store, &b), Err(Miss::Malformed(Malformed::Count)));
}

/// Rows swapped, duplicated or missing never verify: the fact checksum
/// binds every row to its ordinal, and the ordinals and count are checked.
#[test]
fn reordered_duplicated_and_missing_rows_are_misses() {
    let rows: Vec<Vec<u8>> = small(10);
    let rows: Vec<ArtifactRow<'_>> = rows
        .iter()
        .map(|payload| ArtifactRow {
            row_kind: 1,
            index_key: Some(b"name"),
            payload,
        })
        .collect();
    assert_eq!(read_tampered(&rows, ""), Ok(vec![10]));
    for (tamper, expected) in [
        // Payloads swapped between two ordinals.
        (
            "CREATE TEMP TABLE kept AS SELECT ordinal, payload FROM artifact_rows \
             WHERE ordinal IN (2, 5); \
             UPDATE artifact_rows SET payload = \
             (SELECT payload FROM kept WHERE kept.ordinal = 7 - artifact_rows.ordinal) \
             WHERE ordinal IN (2, 5);",
            Miss::ChecksumMismatch,
        ),
        // Ordinals swapped, rows otherwise intact.
        (
            "UPDATE artifact_rows SET ordinal = -1 WHERE ordinal = 2; \
             UPDATE artifact_rows SET ordinal = 2 WHERE ordinal = 5; \
             UPDATE artifact_rows SET ordinal = 5 WHERE ordinal = -1;",
            Miss::ChecksumMismatch,
        ),
        // The last two rows in reverse order.
        (
            "UPDATE artifact_rows SET ordinal = -1 WHERE ordinal = 8; \
             UPDATE artifact_rows SET ordinal = 8 WHERE ordinal = 9; \
             UPDATE artifact_rows SET ordinal = 9 WHERE ordinal = -1;",
            Miss::ChecksumMismatch,
        ),
        // One row duplicated over its neighbour.
        (
            "UPDATE artifact_rows SET (row_kind, index_key, payload) = \
             (SELECT row_kind, index_key, payload FROM artifact_rows WHERE ordinal = 4) \
             WHERE ordinal = 5",
            Miss::ChecksumMismatch,
        ),
        // One row duplicated past the end.
        (
            "INSERT INTO artifact_rows \
             (artifact_ref, ordinal, scope_ref, row_kind, index_key, payload) \
             SELECT artifact_ref, 10, scope_ref, row_kind, index_key, payload \
             FROM artifact_rows WHERE ordinal = 9",
            Miss::Malformed(Malformed::Count),
        ),
        // A row in the middle, the first or the last missing.
        (
            "DELETE FROM artifact_rows WHERE ordinal = 4",
            Miss::Malformed(Malformed::Ordinal),
        ),
        (
            "DELETE FROM artifact_rows WHERE ordinal = 0",
            Miss::Malformed(Malformed::Ordinal),
        ),
        (
            "DELETE FROM artifact_rows WHERE ordinal = 9",
            Miss::Malformed(Malformed::Count),
        ),
    ] {
        assert_eq!(read_tampered(&rows, tamper), Err(expected), "{tamper}");
    }
}

#[test]
fn an_artifact_retired_between_batches_is_a_miss() {
    let root = private_tempdir();
    let store = open(root.path(), StoreLimits::default());
    let header = publish(&store, &key("unit A;"), &small(300));
    let mut reader = store.read_artifact(&header);
    assert!(matches!(reader.next_batch().unwrap(), Lookup::Hit(Some(_))));
    raw(root.path())
        .execute("UPDATE artifacts SET lifecycle = 2", [])
        .unwrap();
    assert_eq!(reader.next_batch().unwrap(), Lookup::Miss(Miss::Retired));
    assert!(!reader.is_verified());
    assert_eq!(
        reader.next_batch().unwrap(),
        Lookup::Miss(Miss::Retired),
        "a failed read stays failed"
    );
}
