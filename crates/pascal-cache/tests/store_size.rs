#![cfg(unix)]
//! Bytes per stored record on a synthetic store shaped like a measured
//! workspace store (rootcause2 §U6): artifact rows of about 86 payload
//! bytes with short name keys, and evidence sets that repeat the same
//! search-path observations.

mod common;

use common::private_tempdir;
use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, ContextKey, DATABASE_FILE, Digest, EvidenceTarget,
    EvidenceWrite, LocalArtifactKey, LookupInputs, LookupKey, LookupKind, Namespace, Observation,
    SourceContent, Store, StoreLimits,
};
use pascal_project::{ConditionalContext, ProjectContext, SourceOrigin};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const ARTIFACTS: usize = 40;
const ROWS_PER_ARTIFACT: usize = 500;
const SETS: usize = 40;
const OBSERVATIONS_PER_SET: usize = 450;
/// Distinct observations across all sets: most sets repeat the same ones.
const DISTINCT_OBSERVATIONS: usize = 600;

/// Pages in use after a full checkpoint.
fn used_bytes(directory: &Path) -> u64 {
    let connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let pragma = |name: &str| -> u64 {
        connection
            .pragma_query_value(None, name, |row| row.get::<_, i64>(0))
            .unwrap() as u64
    };
    (pragma("page_count") - pragma("freelist_count")) * pragma("page_size")
}

fn payload(artifact: usize, row: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(86);
    let mut state = (artifact * 1_000_003 + row) as u64 | 1;
    while bytes.len() < 86 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(86);
    bytes
}

fn observation(index: usize) -> Observation {
    Observation::Presence {
        path: PathBuf::from(format!(
            "/workspace/library/search/path/{index:04}/Unit.pas"
        )),
        entry: None,
        present: index.is_multiple_of(3),
    }
}

/// Per artifact row and per observation, in bytes.
fn measure() -> (f64, f64) {
    let directory = private_tempdir();
    let store = Store::open(
        directory.path(),
        &Namespace::new(Digest::from_bytes([9; 32])),
        StoreLimits::default(),
    )
    .unwrap();
    let empty = used_bytes(directory.path());
    for artifact in 0..ARTIFACTS {
        let key = LocalArtifactKey::new(
            &SourceContent::from_bytes(format!("unit U{artifact};").as_bytes()),
            &ConditionalContext::default(),
            SourceOrigin::Library,
        );
        let payloads: Vec<Vec<u8>> = (0..ROWS_PER_ARTIFACT)
            .map(|row| payload(artifact, row))
            .collect();
        let names: Vec<Vec<u8>> = (0..ROWS_PER_ARTIFACT)
            .map(|row| format!("tname{}", row % 97).into_bytes())
            .collect();
        let rows: Vec<ArtifactRow<'_>> = payloads
            .iter()
            .zip(&names)
            .map(|(payload, name)| ArtifactRow {
                row_kind: 1,
                index_key: Some(name),
                payload,
            })
            .collect();
        store
            .publish_artifact(&ArtifactWrite {
                key: ArtifactKeyRef::local(&key),
                supplemental: b"s",
                rows: &rows,
            })
            .unwrap();
    }
    let with_artifacts = used_bytes(directory.path());
    let context = ContextKey::new(&ProjectContext::default());
    for set in 0..SETS {
        let lookup = LookupKey::new(&LookupInputs {
            kind: LookupKind::Unit,
            normalized_name: &format!("unit{set}"),
            path_expression: None,
            importer_directory: None,
        });
        // Each set takes a window of the shared observations.
        let observations: Vec<Observation> = (0..OBSERVATIONS_PER_SET)
            .map(|index| observation((set * 4 + index) % DISTINCT_OBSERVATIONS))
            .collect();
        store
            .publish_evidence(
                EvidenceTarget::Binding(&lookup),
                &EvidenceWrite {
                    context: &context,
                    observations: &observations,
                },
            )
            .unwrap();
    }
    let with_evidence = used_bytes(directory.path());
    drop(store);
    let rows = (ARTIFACTS * ROWS_PER_ARTIFACT) as f64;
    let observations = (SETS * OBSERVATIONS_PER_SET) as f64;
    (
        (with_artifacts - empty) as f64 / rows,
        (with_evidence - with_artifacts) as f64 / observations,
    )
}

/// Container 4 measured 408.4 bytes per artifact row and 441.9 per
/// observation on this store (l4d-zf1b.8, 13c-4). The INTEGER artifact
/// surrogate with its slim name index, and observations shared by
/// content, must keep well under those. Container 5 measured 177.8 per
/// row; container 6 (13d-3) stores no 32-byte checksum per row (140.3).
/// Container 7 (l4d-rpfc) adds the scope surrogate to each row and its
/// name index entry: 142.1.
#[test]
fn records_take_a_fraction_of_the_container_4_size() {
    let (per_row, per_observation) = measure();
    eprintln!("bytes per artifact row {per_row:.1}, per observation {per_observation:.1}");
    assert!(per_row < 0.55 * 408.4, "per artifact row {per_row:.1}");
    assert!(per_row < 177.8 - 30.0, "per artifact row {per_row:.1}");
    assert!(
        per_observation < 0.15 * 441.9,
        "per observation {per_observation:.1}"
    );
}
