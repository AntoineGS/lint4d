//! Member record and snapshot fixtures shared by the store's tests.

use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, Binding, BindingKind, BindingOutcome, Bypass,
    ContextKey, CoverageStatus, DATABASE_FILE, Digest, EvidenceTarget, EvidenceWrite,
    LocalArtifactKey, Lookup, LookupInputs, LookupKey, LookupKind, MaintenanceLimits,
    MemberCoverage, MemberHeader, MemberRecord, Miss, Namespace, Observation, Published,
    SnapshotHeader, SnapshotMember, SnapshotSeal, SourceContent, SourceIdentity, Store,
    StoreLimits,
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

/// A member at `path` whose one import is ambiguous between `candidates`.
pub fn ambiguous(
    path: &str,
    candidates: &[&str],
    overflow: bool,
    evidence: Digest,
) -> MemberRecord {
    let mut record = member(path, &[], evidence);
    record.bindings.push(Binding {
        kind: BindingKind::Import,
        occurrence: 0,
        outcome: BindingOutcome::Ambiguous {
            candidates: candidates.iter().map(|c| identity(c)).collect(),
            overflow,
        },
        evidence: Some(evidence),
    });
    record
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

/// Publish the local artifact for `text` with `rows` rows of `row_bytes`
/// bytes each.
pub fn publish_bulk(
    store: &Store,
    text: &str,
    rows: usize,
    row_bytes: usize,
) -> Result<Published, Bypass> {
    let key = LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    );
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

/// Retirement without the recency window, so tests need not publish 65,536
/// rows to age one.
pub const EAGER: MaintenanceLimits = MaintenanceLimits {
    recency_window: 0,
    ..MaintenanceLimits::DEFAULT
};

/// Rows that break the reachability invariant (phase 6 Decision 11): a
/// live record over a retired or missing artifact or evidence set, a
/// building or sealed snapshot over a retired or missing record or
/// discovery evidence, live artifact evidence over a retired or missing
/// artifact. Must always be 0.
pub fn check_invariant(directory: &Path) -> i64 {
    const LIVE_ARTIFACT: &str = "EXISTS (SELECT 1 FROM artifacts a WHERE a.namespace_id = r.namespace_id \
        AND a.reset_epoch = r.reset_epoch AND a.artifact_id = r.artifact_id AND a.lifecycle = 1)";
    let live_evidence = |column: &str, alias: &str| {
        format!(
            "EXISTS (SELECT 1 FROM evidence_sets e WHERE e.namespace_id = {alias}.namespace_id \
             AND e.reset_epoch = {alias}.reset_epoch AND e.evidence_id = {alias}.{column} AND e.lifecycle = 1)"
        )
    };
    let queries = [
        format!(
            "SELECT count(*) FROM member_records r WHERE r.lifecycle = 1 AND \
             ((r.artifact_id IS NOT NULL AND NOT {LIVE_ARTIFACT}) \
              OR (r.member_evidence IS NOT NULL AND NOT {}))",
            live_evidence("member_evidence", "r")
        ),
        format!(
            "SELECT count(*) FROM member_bindings b JOIN member_records r \
             ON r.namespace_id = b.namespace_id AND r.reset_epoch = b.reset_epoch AND r.record_id = b.record_id \
             WHERE r.lifecycle = 1 AND b.evidence_id IS NOT NULL AND NOT {}",
            live_evidence("evidence_id", "b")
        ),
        "SELECT count(*) FROM snapshot_members m JOIN snapshots s \
         ON s.namespace_id = m.namespace_id AND s.reset_epoch = m.reset_epoch AND s.snapshot_id = m.snapshot_id \
         WHERE s.state IN (1, 2) AND NOT EXISTS (SELECT 1 FROM member_records r \
         WHERE r.namespace_id = m.namespace_id AND r.reset_epoch = m.reset_epoch \
         AND r.record_id = m.record_id AND r.lifecycle = 1)"
            .to_string(),
        format!(
            "SELECT count(*) FROM snapshots s WHERE s.state = 2 AND s.discovery_evidence IS NOT NULL AND NOT {}",
            live_evidence("discovery_evidence", "s")
        ),
        "SELECT count(*) FROM evidence_sets e WHERE e.lifecycle = 1 AND e.artifact_id IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM artifacts a WHERE a.namespace_id = e.namespace_id \
         AND a.reset_epoch = e.reset_epoch AND a.artifact_id = e.artifact_id AND a.lifecycle = 1)"
            .to_string(),
    ];
    let connection = raw(directory);
    queries
        .iter()
        .map(|sql| {
            connection
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .unwrap()
        })
        .sum()
}

/// Two lease renewals a renewal interval apart (each one due): everything
/// published before now is behind every floor of this process's lease.
pub fn age_past_grace(store: &Store) {
    for _ in 0..2 {
        store.advance_clock_for_test(pascal_cache::LEASE_RENEWAL);
        store.renew_lease().unwrap();
    }
}

/// A member at `path` over the local artifact for `text`, importing
/// `target` under `evidence`.
pub fn local_member(
    store: &Store,
    path: &str,
    text: &str,
    target: &str,
    evidence: Digest,
) -> MemberRecord {
    let mut record = member(path, &[target], evidence);
    record.header.artifact = Some(artifact(store, text));
    record
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
