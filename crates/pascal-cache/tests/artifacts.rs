#![cfg(unix)]

use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, Bypass, Digest, ExpandedArtifactKey, Integrity,
    LocalArtifactKey, Lookup, Malformed, Mismatch, Miss, Namespace, Published, RootMismatch,
    SourceContent, Store, StoreLimits, TooLarge,
};
use pascal_project::{ConditionalContext, SourceOrigin};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{Duration, Instant};

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

fn open(directory: &Path) -> Store {
    Store::open(directory, &namespace(1), StoreLimits::default()).unwrap()
}

fn key(text: &str) -> LocalArtifactKey {
    LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    )
}

fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join("analysis.sqlite3")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

const ROWS: [ArtifactRow<'static>; 3] = [
    ArtifactRow {
        row_kind: 1,
        index_key: Some(b"tfoo"),
        payload: b"declaration Foo",
    },
    ArtifactRow {
        row_kind: 2,
        index_key: None,
        payload: b"mapping",
    },
    ArtifactRow {
        row_kind: 1,
        index_key: Some(b"tbar"),
        payload: b"declaration Bar",
    },
];

fn write<'a>(key: &'a LocalArtifactKey, rows: &'a [ArtifactRow<'a>]) -> ArtifactWrite<'a> {
    ArtifactWrite {
        key: ArtifactKeyRef::local(key),
        supplemental: b"coverage",
        rows,
    }
}

fn header(store: &Store, key: &LocalArtifactKey) -> Lookup<pascal_cache::ArtifactHeader> {
    store.open_artifact(ArtifactKeyRef::local(key)).unwrap()
}

#[test]
fn a_published_artifact_opens_with_its_header() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    assert_eq!(
        store.publish_artifact(&write(&key, &ROWS)),
        Ok(Published::Inserted)
    );
    let Lookup::Hit(header) = header(&store, &key) else {
        panic!("expected a hit")
    };
    assert_eq!(header.artifact_id(), key.digest());
    assert_eq!(header.row_count(), 3);
    assert_eq!(header.content_bytes(), (4 + 15 + 7 + 4 + 15) as u64);
    assert_eq!(header.supplemental(), b"coverage");
}

#[test]
fn an_absent_artifact_is_a_miss() {
    let root = private_tempdir();
    assert_eq!(
        header(&open(root.path()), &key("unit A;")),
        Lookup::Miss(Miss::Absent)
    );
}

#[test]
fn identical_republication_agrees_and_conflicting_content_is_refused() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    store.publish_artifact(&write(&key, &ROWS)).unwrap();
    assert_eq!(
        store.publish_artifact(&write(&key, &ROWS)),
        Ok(Published::AlreadyPresent)
    );
    assert_eq!(
        store.publish_artifact(&write(&key, &ROWS[..2])),
        Err(Bypass::Integrity(Integrity::ConflictingArtifact))
    );
    let Lookup::Hit(header) = header(&store, &key) else {
        panic!("original kept")
    };
    assert_eq!(header.row_count(), 3);
}

#[test]
fn keys_are_scoped_to_their_namespace() {
    let root = private_tempdir();
    let key = key("unit A;");
    open(root.path())
        .publish_artifact(&write(&key, &ROWS))
        .unwrap();
    let other = Store::open(root.path(), &namespace(2), StoreLimits::default()).unwrap();
    assert_eq!(header(&other, &key), Lookup::Miss(Miss::Absent));
}

#[test]
fn expanded_artifacts_are_admitted_with_their_root_manifest() {
    let root_dir = private_tempdir();
    let store = open(root_dir.path());
    let root = key("unit Root;");
    let expanded = ExpandedArtifactKey::new(&root, &[]).unwrap();
    let reference = ArtifactKeyRef::expanded(&expanded, &root).unwrap();
    let write = ArtifactWrite {
        key: reference,
        supplemental: b"",
        rows: &ROWS,
    };
    assert_eq!(store.publish_artifact(&write), Ok(Published::Inserted));
    assert!(matches!(
        store.open_artifact(reference).unwrap(),
        Lookup::Hit(_)
    ));
    // The root's own retained manifest is compared, not only the digest the
    // expanded manifest embeds.
    raw(root_dir.path())
        .execute(
            "UPDATE key_manifests SET manifest = ?1 WHERE digest = ?2",
            params![
                key("unit Other;").key().manifest(),
                root.digest().as_bytes().as_slice()
            ],
        )
        .unwrap();
    assert_eq!(
        store.open_artifact(reference).unwrap(),
        Lookup::Miss(Miss::KeyMismatch(Mismatch::ManifestDiffers))
    );
}

#[test]
fn an_expanded_key_with_another_root_is_rejected() {
    let root = key("unit Root;");
    let expanded = ExpandedArtifactKey::new(&root, &[]).unwrap();
    assert_eq!(
        ArtifactKeyRef::expanded(&expanded, &key("unit Other;")).err(),
        Some(RootMismatch)
    );
}

#[test]
fn a_tampered_key_manifest_is_a_miss() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    store.publish_artifact(&write(&key, &ROWS)).unwrap();
    raw(root.path())
        .execute(
            "UPDATE key_manifests SET manifest = CAST(manifest || x'00' AS BLOB)",
            [],
        )
        .unwrap();
    assert_eq!(
        header(&store, &key),
        Lookup::Miss(Miss::KeyMismatch(Mismatch::ManifestDiffers))
    );
}

#[test]
fn a_retired_artifact_is_a_miss_and_is_not_republished() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    store.publish_artifact(&write(&key, &ROWS)).unwrap();
    raw(root.path())
        .execute("UPDATE artifacts SET lifecycle = 2", [])
        .unwrap();
    assert_eq!(header(&store, &key), Lookup::Miss(Miss::Retired));
    assert_eq!(
        store.publish_artifact(&write(&key, &ROWS)),
        Err(Bypass::Retiring)
    );
}

#[test]
fn tampered_headers_are_malformed_misses() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    store.publish_artifact(&write(&key, &ROWS)).unwrap();
    let connection = raw(root.path());
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    let original: (i64, i64, i64, Vec<u8>) = connection
        .query_row(
            "SELECT kind, row_count, content_bytes, fact_checksum FROM artifacts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    for tamper in [
        "UPDATE artifacts SET fact_checksum = x'00'",
        "UPDATE artifacts SET row_count = -1",
        "UPDATE artifacts SET content_bytes = 16777217",
        "UPDATE artifacts SET kind = 3",
    ] {
        // Committed, so the store's WAL readers see it.
        connection.execute(tamper, []).unwrap();
        assert_eq!(
            header(&store, &key),
            Lookup::Miss(Miss::Malformed(Malformed::Header)),
            "{tamper}"
        );
        connection
            .execute(
                "UPDATE artifacts SET kind = ?1, row_count = ?2, content_bytes = ?3, fact_checksum = ?4",
                params![original.0, original.1, original.2, original.3],
            )
            .unwrap();
        assert!(
            matches!(header(&store, &key), Lookup::Hit(_)),
            "restored after {tamper}"
        );
    }
}

#[test]
fn an_oversized_supplemental_blob_is_a_miss() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    store.publish_artifact(&write(&key, &ROWS)).unwrap();
    let connection = raw(root.path());
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    connection
        .execute("UPDATE artifacts SET supplemental = zeroblob(3145728)", [])
        .unwrap();
    assert_eq!(header(&store, &key), Lookup::Miss(Miss::Oversized));
}

#[test]
fn writes_over_the_admission_limits_are_not_persisted() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    let big = vec![0u8; 1_000_000];
    let rows: Vec<ArtifactRow<'_>> = (0..17)
        .map(|_| ArtifactRow {
            row_kind: 1,
            index_key: None,
            payload: &big,
        })
        .collect();
    assert_eq!(
        store.publish_artifact(&write(&key, &rows)),
        Err(Bypass::TooLarge(TooLarge::Content { bytes: 17_000_000 }))
    );
    let huge = vec![0u8; 1024 * 1024 + 1];
    let row = [ArtifactRow {
        row_kind: 1,
        index_key: None,
        payload: &huge,
    }];
    assert_eq!(
        store.publish_artifact(&write(&key, &row)),
        Err(Bypass::TooLarge(TooLarge::Row {
            ordinal: 0,
            bytes: 1024 * 1024 + 1
        }))
    );
    let long_key = vec![b'k'; 4097];
    let row = [ArtifactRow {
        row_kind: 1,
        index_key: Some(&long_key),
        payload: b"",
    }];
    assert_eq!(
        store.publish_artifact(&write(&key, &row)),
        Err(Bypass::TooLarge(TooLarge::IndexKey {
            ordinal: 0,
            bytes: 4097
        }))
    );
    let supplemental = vec![0u8; 1024 * 1024 + 1];
    let oversized = ArtifactWrite {
        supplemental: &supplemental,
        ..write(&key, &ROWS)
    };
    assert_eq!(
        store.publish_artifact(&oversized),
        Err(Bypass::TooLarge(TooLarge::Supplemental {
            bytes: 1024 * 1024 + 1
        }))
    );
    let row = [ArtifactRow {
        row_kind: 0,
        index_key: None,
        payload: b"",
    }];
    assert_eq!(
        store.publish_artifact(&write(&key, &row)),
        Err(Bypass::InvalidRowKind { ordinal: 0 })
    );
    assert_eq!(header(&store, &key), Lookup::Miss(Miss::Absent));
}

#[test]
fn key_manifests_are_retained_and_admitted_exactly() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    assert_eq!(store.admit_key(key.key()), Ok(Lookup::Miss(Miss::Absent)));
    assert_eq!(store.publish_key(key.key()), Ok(Published::Inserted));
    assert_eq!(store.publish_key(key.key()), Ok(Published::AlreadyPresent));
    assert_eq!(store.admit_key(key.key()), Ok(Lookup::Hit(())));
    raw(root.path())
        .execute(
            "UPDATE key_manifests SET manifest = CAST(manifest || x'00' AS BLOB)",
            [],
        )
        .unwrap();
    assert_eq!(
        store.publish_key(key.key()),
        Err(Bypass::Integrity(Integrity::ConflictingManifest))
    );
}

#[test]
fn a_full_disk_bypasses_without_a_partial_artifact() {
    let root = private_tempdir();
    let store = open(root.path());
    store.limit_growth_for_test(4).unwrap();
    let key = key("unit A;");
    let payload = vec![7u8; 200_000];
    let rows: Vec<ArtifactRow<'_>> = (0..10)
        .map(|_| ArtifactRow {
            row_kind: 1,
            index_key: None,
            payload: &payload,
        })
        .collect();
    assert_eq!(
        store.publish_artifact(&write(&key, &rows)),
        Err(Bypass::Full)
    );
    assert_eq!(header(&store, &key), Lookup::Miss(Miss::Absent));
    let rows_left: i64 = raw(root.path())
        .query_row("SELECT count(*) FROM artifact_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows_left, 0);
}

#[test]
fn a_failed_commit_leaves_no_partial_artifact() {
    let root = private_tempdir();
    let store = open(root.path());
    store.set_before_commit_for_test(|| panic!("injected failure before commit"));
    let key = key("unit A;");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.publish_artifact(&write(&key, &ROWS))
    }));
    assert!(result.is_err());
    store.set_before_commit_for_test(|| {});
    assert_eq!(header(&store, &key), Lookup::Miss(Miss::Absent));
    assert_eq!(
        store.publish_artifact(&write(&key, &ROWS)),
        Ok(Published::Inserted)
    );
}

#[test]
fn a_held_write_lock_bypasses_writes_but_not_reads() {
    let root = private_tempdir();
    let store = open(root.path());
    let a = key("unit A;");
    store.publish_artifact(&write(&a, &ROWS)).unwrap();
    let other = raw(root.path());
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(
        store.publish_artifact(&write(&key("unit B;"), &ROWS)),
        Err(Bypass::Busy)
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert!(matches!(header(&store, &a), Lookup::Hit(_)));
    other.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn a_fenced_store_neither_publishes_nor_reads() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = key("unit A;");
    store.publish_artifact(&write(&key, &ROWS)).unwrap();
    raw(root.path())
        .execute("UPDATE namespaces SET reset_epoch = reset_epoch + 1", [])
        .unwrap();
    assert_eq!(
        store.publish_artifact(&write(&key, &ROWS)),
        Err(Bypass::Fenced)
    );
    assert_eq!(
        store.open_artifact(ArtifactKeyRef::local(&key)),
        Err(Bypass::Fenced)
    );
}
