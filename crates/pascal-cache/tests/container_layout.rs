#![cfg(unix)]
//! Container 5 (l4d-zf1b.8, 13c-4): the container version is in the file
//! name, artifact rows hang off an INTEGER surrogate and observations are
//! stored once per content and shared by the evidence sets that list them.
//! Container 6 (13d-3) stores no per-row checksum: the fact checksum of the
//! whole artifact verifies its rows. Identities, fact checksums and
//! integrity checks are those of container 4.

mod common;

use common::private_tempdir;
use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, CONTAINER_VERSION, ContextKey, DATABASE_FILE,
    Digest, EvidenceHeader, EvidenceTarget, EvidenceWrite, LocalArtifactKey, Lookup, LookupInputs,
    LookupKey, LookupKind, Miss, Namespace, Observation, SourceContent, Store, StoreLimits,
};
use pascal_project::{ConditionalContext, ProjectContext, SourceOrigin};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn open(directory: &Path) -> Store {
    Store::open(
        directory,
        &Namespace::new(Digest::from_bytes([7; 32])),
        StoreLimits::default(),
    )
    .unwrap()
}

fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

fn count(directory: &Path, sql: &str) -> i64 {
    raw(directory).query_row(sql, [], |row| row.get(0)).unwrap()
}

fn presence(index: usize) -> Observation {
    Observation::Presence {
        path: PathBuf::from(format!("/w/{index}.inc")),
        entry: None,
        present: false,
    }
}

fn lookup(name: &str) -> LookupKey {
    LookupKey::new(&LookupInputs {
        kind: LookupKind::Unit,
        normalized_name: name,
        path_expression: None,
        importer_directory: None,
    })
}

fn context() -> ContextKey {
    ContextKey::new(&ProjectContext::default())
}

fn publish_set(store: &Store, name: &str, observations: &[Observation]) -> Digest {
    store
        .publish_evidence(
            EvidenceTarget::Binding(&lookup(name)),
            &EvidenceWrite {
                context: &context(),
                observations,
            },
        )
        .unwrap()
        .evidence_id
}

fn header(store: &Store, id: Digest) -> EvidenceHeader {
    match store.open_evidence(id).unwrap() {
        Lookup::Hit(header) => header,
        miss => panic!("{miss:?}"),
    }
}

/// Every observation of set `id`, or the miss that ended the read.
fn read_set(store: &Store, id: Digest) -> Result<Vec<Observation>, Miss> {
    let header = match store.open_evidence(id).unwrap() {
        Lookup::Hit(header) => header,
        Lookup::Miss(miss) => return Err(miss),
    };
    let mut reader = store.read_evidence(&header);
    let mut read = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => read.extend_from_slice(batch.observations()),
            Lookup::Hit(None) => return Ok(read),
            Lookup::Miss(miss) => return Err(miss),
        }
    }
}

#[test]
fn the_database_file_name_carries_the_container_version() {
    assert_eq!(CONTAINER_VERSION, 6);
    assert_eq!(DATABASE_FILE, "analysis-v6.sqlite3");
    assert_eq!(
        DATABASE_FILE,
        format!("analysis-v{CONTAINER_VERSION}.sqlite3")
    );
}

/// R9-3: older containers stay where they are, untouched, and the store
/// opens its own file beside them; nobody has to do anything.
#[test]
fn previous_container_files_beside_are_left_untouched() {
    let root = private_tempdir();
    let olds = [
        (root.path().join("analysis.sqlite3"), 4),
        (root.path().join("analysis-v5.sqlite3"), 5),
    ];
    let mut before = Vec::new();
    for (old, version) in &olds {
        let connection = Connection::open(old).unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = 1278493763; PRAGMA user_version = {version};
                 CREATE TABLE store_state (singleton INTEGER PRIMARY KEY);"
            ))
            .unwrap();
        drop(connection);
        before.push(std::fs::read(old).unwrap());
    }
    let store = open(root.path());
    assert_eq!(store.check_scope(), Ok(()));
    assert!(root.path().join(DATABASE_FILE).is_file());
    drop(store);
    for ((old, _), before) in olds.iter().zip(&before) {
        assert_eq!(&std::fs::read(old).unwrap(), before, "{}", old.display());
        let mut wal = old.as_os_str().to_owned();
        wal.push("-wal");
        assert!(!Path::new(&wal).exists(), "{}", old.display());
    }
}

#[test]
fn identical_observations_are_stored_once() {
    let root = private_tempdir();
    let store = open(root.path());
    let shared: Vec<Observation> = (0..10).map(presence).collect();
    let a = publish_set(&store, "a", &shared);
    let mut more = shared.clone();
    more.push(presence(10));
    let b = publish_set(&store, "b", &more);
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM observation_content"),
        11
    );
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM evidence_observations"),
        21
    );
    assert_eq!(read_set(&store, a), Ok(shared));
    assert_eq!(read_set(&store, b), Ok(more));
    // The evidence ID is that of the observations, not of their storage.
    assert_eq!(header(&store, a).evidence_id(), a);
}

/// One corrupted shared observation fails every set that lists it, and a
/// set that does not list it still verifies.
#[test]
fn a_corrupted_shared_observation_fails_every_set_that_lists_it() {
    let root = private_tempdir();
    let store = open(root.path());
    let a = publish_set(&store, "a", &[presence(0), presence(1)]);
    let b = publish_set(&store, "b", &[presence(1), presence(2)]);
    let c = publish_set(&store, "c", &[presence(3)]);
    let swapped = Observation::Presence {
        path: PathBuf::from("/w/1.inc"),
        entry: None,
        present: true,
    }
    .encoded();
    let path_key: Vec<u8> = raw(root.path())
        .query_row(
            "SELECT path_key FROM observation_content c JOIN evidence_observations o \
             ON o.observation = c.observation \
             JOIN evidence_sets e ON e.evidence_ref = o.evidence_ref \
             WHERE e.evidence_id = ?1 AND o.ordinal = 1",
            [a.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    raw(root.path())
        .execute(
            "UPDATE observation_content SET payload = ?1 WHERE path_key = ?2",
            rusqlite::params![swapped, path_key],
        )
        .unwrap();
    assert_eq!(read_set(&store, a), Err(Miss::ChecksumMismatch));
    assert_eq!(read_set(&store, b), Err(Miss::ChecksumMismatch));
    assert_eq!(read_set(&store, c), Ok(vec![presence(3)]));
}

/// Reclamation deletes the observation content no evidence set lists any
/// more, and never content another set, live or retired, still lists.
#[test]
fn reclamation_sweeps_only_unreferenced_observation_content() {
    let root = private_tempdir();
    let store = open(root.path());
    let a = publish_set(&store, "a", &[presence(0), presence(1)]);
    let b = publish_set(&store, "b", &[presence(1), presence(2)]);
    let retire = |id: Digest| {
        raw(root.path())
            .execute(
                "UPDATE evidence_sets SET lifecycle = 2 WHERE evidence_id = ?1",
                [id.as_bytes().as_slice()],
            )
            .unwrap();
    };
    retire(a);
    let reclaimed = store.reclaim(256).unwrap();
    assert!(reclaimed.complete, "{reclaimed:?}");
    // `presence(0)` went with `a`; `presence(1)` is still listed by `b`.
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM observation_content"),
        2
    );
    assert_eq!(read_set(&store, b), Ok(vec![presence(1), presence(2)]));
    let c = publish_set(&store, "c", &[presence(2), presence(5)]);
    retire(b);
    store.reclaim(256).unwrap();
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM observation_content"),
        2,
        "only c's content is left"
    );
    retire(c);
    store.reclaim(256).unwrap();
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM observation_content"),
        0
    );
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM evidence_observations"),
        0
    );
    // Published again after the sweep, the content is stored again.
    let d = publish_set(&store, "d", &[presence(0)]);
    assert_eq!(read_set(&store, d), Ok(vec![presence(0)]));
}

/// A retired scope's observation content and artifact rows are reclaimed
/// with the rest of the scope.
#[test]
fn a_reset_reclaims_shared_content_and_rows_of_the_old_scope() {
    let root = private_tempdir();
    let store = open(root.path());
    publish_set(&store, "a", &[presence(0), presence(1)]);
    let key = LocalArtifactKey::new(
        &SourceContent::from_bytes(b"unit A;"),
        &ConditionalContext::default(),
        SourceOrigin::Library,
    );
    store
        .publish_artifact(&ArtifactWrite {
            key: ArtifactKeyRef::local(&key),
            supplemental: b"",
            rows: &[ArtifactRow {
                row_kind: 1,
                index_key: Some(b"a"),
                payload: b"p",
            }],
        })
        .unwrap();
    store.reset_namespace().unwrap();
    drop(store);
    let store = open(root.path());
    while !store.reclaim(256).unwrap().complete {}
    for table in [
        "observation_content",
        "evidence_observations",
        "artifact_rows",
        "artifacts",
    ] {
        assert_eq!(
            count(root.path(), &format!("SELECT count(*) FROM {table}")),
            0,
            "{table}"
        );
    }
}

/// Stored identities and fact checksums, as container 4 computed them: the
/// layout changes of containers 5 and 6 leave every artifact ID, fact
/// checksum and evidence ID as it was. Only the storage projection
/// changed: container 6 drops the per-row checksum column.
#[test]
fn identities_and_checksums_are_those_of_container_4() {
    let root = private_tempdir();
    let store = open(root.path());
    let key = LocalArtifactKey::new(
        &SourceContent::from_bytes(b"unit A;"),
        &ConditionalContext::default(),
        SourceOrigin::Library,
    );
    store
        .publish_artifact(&ArtifactWrite {
            key: ArtifactKeyRef::local(&key),
            supplemental: b"s",
            rows: &[
                ArtifactRow {
                    row_kind: 1,
                    index_key: Some(b"a"),
                    payload: b"p",
                },
                ArtifactRow {
                    row_kind: 2,
                    index_key: None,
                    payload: b"q",
                },
            ],
        })
        .unwrap();
    publish_set(&store, "a", &[presence(0), presence(1)]);
    let lines: Vec<String> = store
        .content_for_test()
        .unwrap()
        .into_iter()
        .filter(|line| !line.starts_with("key_manifests "))
        .collect();
    let digest = blake3::hash(lines.join("\n").as_bytes())
        .to_hex()
        .to_string();
    assert_eq!(
        digest, "21e1a4b28576229aea09975f4010693e0eb0d8e5c2d1e3ababd116a0bc88e761",
        "{lines:#?}"
    );
    assert_eq!(
        columns(root.path(), "artifact_rows"),
        [
            "artifact_ref",
            "ordinal",
            "row_kind",
            "index_key",
            "payload"
        ]
    );
}

fn columns(directory: &Path, table: &str) -> Vec<String> {
    raw(directory)
        .prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The fact checksum of the whole artifact is the only stored checksum:
/// no fact table repeats one per row.
#[test]
fn only_the_artifact_header_stores_a_checksum() {
    let root = private_tempdir();
    drop(open(root.path()));
    let tables: Vec<String> = raw(root.path())
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut checksums = Vec::new();
    for table in tables {
        for column in columns(root.path(), &table) {
            if column.contains("checksum") {
                checksums.push(format!("{table}.{column}"));
            }
        }
    }
    assert_eq!(checksums, ["artifacts.fact_checksum"]);
}
