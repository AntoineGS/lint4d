//! Member record and snapshot fixtures shared by the store's tests.

use pascal_cache::{
    ArtifactKeyRef, Binding, BindingKind, BindingOutcome, ContextKey, CoverageStatus,
    DATABASE_FILE, Digest, EvidenceTarget, EvidenceWrite, LocalArtifactKey, Lookup, LookupInputs,
    LookupKey, LookupKind, MemberCoverage, MemberHeader, MemberRecord, Miss, Namespace,
    Observation, SnapshotHeader, SnapshotMember, SnapshotSeal, SourceContent, SourceIdentity,
    Store, StoreLimits,
};
use pascal_core::facts::ArtifactFacts;
use pascal_project::{ConditionalContext, ProjectContext, SourceOrigin};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAMESPACE: u8 = 5;

pub fn open(directory: &Path) -> Store {
    Store::open(
        directory,
        &Namespace::new(Digest::from_bytes([NAMESPACE; 32])),
        StoreLimits::default(),
    )
    .unwrap()
}

/// A connection outside the store, with foreign keys off, to tamper.
pub fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    // The bundled SQLite enforces foreign keys by default.
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    connection
}

pub fn context() -> ContextKey {
    ContextKey::new(&ProjectContext::default())
}

pub fn identity(path: &str) -> SourceIdentity {
    SourceIdentity {
        canonical: path.into(),
        logical: path.into(),
        spelling: None,
        uri: None,
    }
}

pub fn unit_lookup(name: &str) -> LookupKey {
    LookupKey::new(&LookupInputs {
        kind: LookupKind::Unit,
        normalized_name: name,
        path_expression: None,
        importer_directory: None,
    })
}

/// Publish binding evidence that unit `name` was found at `target`.
pub fn binding_evidence(store: &Store, name: &str, target: &str) -> Digest {
    let observations = [Observation::Presence {
        path: PathBuf::from(target),
        entry: None,
        present: true,
    }];
    store
        .publish_evidence(
            EvidenceTarget::Binding(&unit_lookup(name)),
            &EvidenceWrite {
                context: &context(),
                observations: &observations,
            },
        )
        .unwrap()
        .evidence_id
}

/// Publish a local artifact for `text` and return its ID.
pub fn artifact(store: &Store, text: &str) -> Digest {
    let key = LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    );
    let facts = ArtifactFacts {
        slot_lengths: vec![12],
        expanded_length: 12,
        ..ArtifactFacts::default()
    };
    store
        .publish_facts(ArtifactKeyRef::local(&key), &facts)
        .unwrap();
    key.digest()
}

/// An unindexed member at `path` importing each of `targets` precisely.
pub fn member(path: &str, targets: &[&str], evidence: Digest) -> MemberRecord {
    MemberRecord {
        header: MemberHeader {
            source: identity(path),
            artifact: None,
            member_evidence: None,
            origin: SourceOrigin::ProjectCompiled,
            fallback_unit_name: None,
            coverage: MemberCoverage {
                extraction: CoverageStatus::Complete,
                resolution: CoverageStatus::Complete,
            },
        },
        bindings: targets
            .iter()
            .enumerate()
            .map(|(occurrence, target)| Binding {
                kind: BindingKind::Import,
                occurrence: occurrence as u64,
                outcome: BindingOutcome::Precise {
                    target: identity(target),
                },
                evidence: Some(evidence),
            })
            .collect(),
    }
}

/// The whole record, read and verified, or the miss that ended the read.
pub fn read_record(store: &Store, id: Digest) -> Result<MemberRecord, Miss> {
    let header = match store.open_member_record(id).unwrap() {
        Lookup::Hit(header) => header,
        Lookup::Miss(miss) => return Err(miss),
    };
    let mut reader = store.read_member_record(&header);
    let mut bindings = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => bindings.extend_from_slice(batch.bindings()),
            Lookup::Hit(None) => {
                assert!(reader.is_verified());
                return Ok(MemberRecord {
                    header: header.member().clone(),
                    bindings,
                });
            }
            Lookup::Miss(miss) => {
                assert!(!reader.is_verified());
                return Err(miss);
            }
        }
    }
}

pub const SEAL: SnapshotSeal = SnapshotSeal {
    discovery: None,
    coverage: pascal_cache::SnapshotCoverage {
        discovery: CoverageStatus::Complete,
        membership: CoverageStatus::Complete,
    },
};

/// Publish `count` binding-free members `/w/{prefix}{index}.pas`.
pub fn records(store: &Store, prefix: &str, count: usize) -> Vec<Digest> {
    (0..count)
        .map(|index| {
            let record = member(
                &format!("/w/{prefix}{index}.pas"),
                &[],
                Digest::from_bytes([0; 32]),
            );
            store.publish_member_record(&record).unwrap().record_id
        })
        .collect()
}

/// Seal a snapshot of `records` under `context()`, in batches of 256.
pub fn sealed(store: &Store, records: &[Digest]) -> SnapshotHeader {
    let mut builder = store.begin_snapshot(&context()).unwrap();
    for batch in records.chunks(256) {
        store.add_members(&mut builder, batch).unwrap();
    }
    store.seal_snapshot(builder, &SEAL).unwrap()
}

/// Every member, read and verified, or the miss that ended the read.
pub fn read_members(store: &Store, header: &SnapshotHeader) -> Result<Vec<SnapshotMember>, Miss> {
    let mut reader = store.read_snapshot(header);
    let mut members = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => members.extend_from_slice(batch.members()),
            Lookup::Hit(None) => {
                assert!(reader.is_verified());
                return Ok(members);
            }
            Lookup::Miss(miss) => {
                assert!(!reader.is_verified());
                return Err(miss);
            }
        }
    }
}
