#![cfg(unix)]

mod common;

use common::private_tempdir;
use pascal_cache::{
    ArtifactKeyRef, Bypass, ContextKey, DATABASE_FILE, Digest, EvidenceWrite, ExpandedArtifactKey,
    LocalArtifactKey, Lookup, MAX_EVIDENCE_OBSERVATIONS, Malformed, Mismatch, Miss, Namespace,
    Observation, Published, PublishedEvidence, SourceContent, Store, StoreLimits, TooLarge,
    listing_digest,
};
use pascal_core::facts::{ArtifactFacts, FactBudget};
use pascal_project::{
    ConditionalContext, ProjectContext, ProjectContextParts, ProjectPathEntry, SourceOrigin,
};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn store(directory: &Path) -> Store {
    Store::open(
        directory,
        &Namespace::new(Digest::from_bytes([5; 32])),
        StoreLimits::default(),
    )
    .unwrap()
}

fn raw(directory: &Path) -> Connection {
    let connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

fn facts() -> ArtifactFacts {
    ArtifactFacts {
        slot_lengths: vec![12],
        expanded_length: 12,
        ..ArtifactFacts::default()
    }
}

struct Keys {
    root: LocalArtifactKey,
    expanded: ExpandedArtifactKey,
    context: ContextKey,
}

fn keys(text: &str) -> Keys {
    let root = LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::Library,
    );
    let expanded = ExpandedArtifactKey::new(&root, &[]).unwrap();
    Keys {
        root,
        expanded,
        context: ContextKey::new(&ProjectContext::default()),
    }
}

impl Keys {
    fn artifact(&self) -> ArtifactKeyRef<'_> {
        ArtifactKeyRef::expanded(&self.expanded, &self.root).unwrap()
    }
}

fn observations() -> Vec<Observation> {
    let root = PathBuf::from("/w");
    vec![
        Observation::Payload {
            path: root.join("a.inc"),
            entry: ProjectPathEntry::configured(root.join("a.inc")),
            content: SourceContent::from_bytes(b"const A = 1;"),
        },
        Observation::Presence {
            path: root.join("b.inc"),
            entry: None,
            present: false,
        },
        Observation::Listing {
            path: root.clone(),
            entry: ProjectPathEntry::configured(root.clone()),
            listing: listing_digest(&[root.join("a.inc")], &[]),
        },
    ]
}

fn publish(
    store: &Store,
    keys: &Keys,
    observations: &[Observation],
) -> Result<PublishedEvidence, Bypass> {
    store.publish_facts_with_evidence(
        keys.artifact(),
        &facts(),
        &EvidenceWrite {
            context: &keys.context,
            observations,
        },
    )
}

/// Every observation of the evidence, or the miss that ended the read.
fn read_all(store: &Store, keys: &Keys) -> Result<Vec<Observation>, Miss> {
    let Lookup::Hit(headers) = store.find_evidence(&keys.context, &keys.root).unwrap() else {
        panic!("evidence is found by its subject")
    };
    assert_eq!(headers.len(), 1);
    let mut reader = store.read_evidence(&headers[0]);
    let mut read = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => {
                assert!(!reader.is_verified());
                read.extend_from_slice(batch.observations());
            }
            Lookup::Hit(None) => {
                assert!(reader.is_verified());
                return Ok(read);
            }
            Lookup::Miss(miss) => {
                assert!(!reader.is_verified());
                return Err(miss);
            }
        }
    }
}

#[test]
fn facts_and_their_evidence_publish_together_and_read_back_verified() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    let published = publish(&store, &keys, &observations()).unwrap();
    assert_eq!(
        published,
        PublishedEvidence {
            artifact: Published::Inserted,
            evidence: Published::Inserted
        }
    );
    assert_eq!(read_all(&store, &keys), Ok(observations()));
    let Lookup::Hit(headers) = store.find_evidence(&keys.context, &keys.root).unwrap() else {
        unreachable!()
    };
    assert_eq!(headers[0].artifact_id(), keys.expanded.digest());
    assert_eq!(headers[0].observation_count(), 3);
    assert_eq!(
        store
            .load_facts_by_id(headers[0].artifact_id(), &mut FactBudget::new(1 << 20))
            .unwrap(),
        Lookup::Hit(facts())
    );
    assert_eq!(
        publish(&store, &keys, &observations()).unwrap(),
        PublishedEvidence {
            artifact: Published::AlreadyPresent,
            evidence: Published::AlreadyPresent
        }
    );
    // Other evidence for the same artifact, as another observation set
    // would give, is a second candidate.
    publish(&store, &keys, &observations()[..2]).unwrap();
    let Lookup::Hit(headers) = store.find_evidence(&keys.context, &keys.root).unwrap() else {
        unreachable!()
    };
    assert_eq!(headers.len(), 2);
}

#[test]
fn evidence_is_found_only_under_its_context_and_root() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    publish(&store, &keys, &observations()).unwrap();
    let other_context = ContextKey::new(&ProjectContext::from_parts(ProjectContextParts {
        include_path_entries: vec![ProjectPathEntry::configured(PathBuf::from("/inc"))],
        ..ProjectContextParts::default()
    }));
    assert_eq!(
        store.find_evidence(&other_context, &keys.root).unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    assert_eq!(
        store
            .find_evidence(&keys.context, &self::keys("unit B;").root)
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
}

#[test]
fn a_local_artifact_takes_no_evidence() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    assert_eq!(
        store.publish_facts_with_evidence(
            ArtifactKeyRef::local(&keys.root),
            &facts(),
            &EvidenceWrite {
                context: &keys.context,
                observations: &observations(),
            },
        ),
        Err(Bypass::EvidenceForLocalArtifact)
    );
}

#[test]
fn tampered_evidence_is_never_a_verified_read() {
    let flipped = Observation::Presence {
        path: PathBuf::from("/w/b.inc"),
        entry: None,
        present: true,
    }
    .encoded();
    type Tamper = Box<dyn Fn(&Connection)>;
    let cases: [(&str, Tamper, Miss); 4] = [
        (
            "a well-formed observation swapped in",
            Box::new(move |db| {
                db.execute(
                    "UPDATE observations SET payload = ?1 WHERE ordinal = 1",
                    [&flipped],
                )
                .unwrap();
            }),
            Miss::ChecksumMismatch,
        ),
        (
            "a kind that disagrees with the payload",
            Box::new(|db| {
                db.execute("UPDATE observations SET kind = 3 WHERE ordinal = 1", [])
                    .unwrap();
            }),
            Miss::Malformed(Malformed::Evidence),
        ),
        (
            "a deleted row",
            Box::new(|db| {
                db.execute("DELETE FROM observations WHERE ordinal = 2", [])
                    .unwrap();
            }),
            Miss::Malformed(Malformed::Count),
        ),
        (
            "an appended row",
            Box::new(|db| {
                db.execute(
                    "INSERT INTO observations SELECT namespace_id, reset_epoch, evidence_id, 3, \
                     kind, path_key, payload FROM observations WHERE ordinal = 0",
                    [],
                )
                .unwrap();
            }),
            Miss::Malformed(Malformed::Count),
        ),
    ];
    for (label, tamper, miss) in cases {
        let directory = private_tempdir();
        let store = store(directory.path());
        let keys = keys("unit A;");
        publish(&store, &keys, &observations()).unwrap();
        tamper(&raw(directory.path()));
        assert_eq!(read_all(&store, &keys), Err(miss), "{label}");
    }
}

#[test]
fn oversized_evidence_is_not_persisted() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    let long = Observation::Presence {
        path: PathBuf::from(format!("/{}", "x".repeat(5000))),
        entry: None,
        present: false,
    };
    assert!(matches!(
        publish(&store, &keys, &[long]),
        Err(Bypass::TooLarge(TooLarge::IndexKey { ordinal: 0, .. }))
    ));
    let many = vec![observations()[1].clone(); MAX_EVIDENCE_OBSERVATIONS + 1];
    assert_eq!(
        publish(&store, &keys, &many),
        Err(Bypass::TooLarge(TooLarge::Observations {
            count: MAX_EVIDENCE_OBSERVATIONS + 1
        }))
    );
    assert_eq!(
        store.find_evidence(&keys.context, &keys.root).unwrap(),
        Lookup::Miss(Miss::Absent),
        "nothing was published"
    );
}

#[test]
fn an_artifact_opened_by_id_is_checked_against_its_stored_manifest() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    publish(&store, &keys, &observations()).unwrap();
    let id = keys.expanded.digest();
    assert!(matches!(
        store.open_artifact_by_id(id).unwrap(),
        Lookup::Hit(_)
    ));
    assert_eq!(
        store
            .open_artifact_by_id(Digest::from_bytes([7; 32]))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    let mut manifest = keys.expanded.key().manifest().to_vec();
    *manifest.last_mut().unwrap() ^= 1;
    raw(directory.path())
        .execute(
            "UPDATE key_manifests SET manifest = ?1 WHERE digest = ?2",
            rusqlite::params![manifest, id.as_bytes().as_slice()],
        )
        .unwrap();
    assert_eq!(
        store.open_artifact_by_id(id).unwrap(),
        Lookup::Miss(Miss::KeyMismatch(Mismatch::DigestDiffers))
    );
}
