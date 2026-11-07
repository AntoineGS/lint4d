#![cfg(unix)]

mod common;

use common::private_tempdir;
use pascal_cache::{
    ArtifactKeyRef, Bypass, ContextKey, DATABASE_FILE, Digest, DiscoveryInputs, DiscoveryScope,
    EvidenceHeader, EvidencePurpose, EvidenceTarget, EvidenceWrite, ExpandedArtifactKey,
    LocalArtifactKey, Lookup, LookupInputs, LookupKey, LookupKind, MAX_EVIDENCE_CANDIDATES,
    MAX_EVIDENCE_OBSERVATIONS, MAX_ROW_BATCH_BYTES, Malformed, Mismatch, Miss, Namespace,
    Observation, Published, PublishedEvidence, ScannedDirectory, SourceContent, Store, StoreLimits,
    TooLarge, decode_observation, evidence_id, listing_digest,
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
    assert_eq!(headers[0].artifact_id(), Some(keys.expanded.digest()));
    assert_eq!(headers[0].purpose(), EvidencePurpose::Artifact);
    assert_eq!(headers[0].observation_count(), 3);
    assert_eq!(
        store
            .load_facts_by_id(
                headers[0].artifact_id().unwrap(),
                &mut FactBudget::new(1 << 20)
            )
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
                    "UPDATE observation_content SET payload = ?1 WHERE observation = \
                     (SELECT observation FROM evidence_observations WHERE ordinal = 1)",
                    [&flipped],
                )
                .unwrap();
            }),
            Miss::ChecksumMismatch,
        ),
        (
            "a kind that disagrees with the payload",
            Box::new(|db| {
                db.execute(
                    "UPDATE observation_content SET kind = 3 WHERE observation = \
                     (SELECT observation FROM evidence_observations WHERE ordinal = 1)",
                    [],
                )
                .unwrap();
            }),
            Miss::Malformed(Malformed::Evidence),
        ),
        (
            "a deleted row",
            Box::new(|db| {
                db.execute("DELETE FROM evidence_observations WHERE ordinal = 2", [])
                    .unwrap();
            }),
            Miss::Malformed(Malformed::Count),
        ),
        (
            "an appended row",
            Box::new(|db| {
                db.execute(
                    "INSERT INTO evidence_observations SELECT evidence_ref, 3, observation \
                     FROM evidence_observations WHERE ordinal = 0",
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

#[test]
fn evidence_retired_or_deleted_between_batches_is_a_miss() {
    for (label, change, miss) in [
        (
            "retired",
            "UPDATE evidence_sets SET lifecycle = 2",
            Miss::Retired,
        ),
        (
            "deleted",
            "DELETE FROM evidence_observations; DELETE FROM evidence_sets",
            Miss::Vanished,
        ),
    ] {
        let directory = private_tempdir();
        let store = store(directory.path());
        let keys = keys("unit A;");
        // More than one batch of observations.
        let many: Vec<Observation> = (0..300)
            .map(|index| Observation::Presence {
                path: PathBuf::from(format!("/w/{index}.inc")),
                entry: None,
                present: false,
            })
            .collect();
        publish(&store, &keys, &many).unwrap();
        let Lookup::Hit(headers) = store.find_evidence(&keys.context, &keys.root).unwrap() else {
            panic!("published")
        };
        let mut reader = store.read_evidence(&headers[0]);
        let Lookup::Hit(Some(first)) = reader.next_batch().unwrap() else {
            panic!("{label}: first batch")
        };
        assert_eq!(first.observations().len(), 256);
        raw(directory.path()).execute_batch(change).unwrap();
        assert_eq!(reader.next_batch().unwrap(), Lookup::Miss(miss), "{label}");
        assert!(!reader.is_verified());
        assert_eq!(
            reader.next_batch().unwrap(),
            Lookup::Miss(miss),
            "{label}: sticky"
        );
    }
}

#[test]
fn a_row_past_the_declared_count_is_a_count_miss_even_when_malformed_or_oversized() {
    for (label, path_key, payload) in [
        ("malformed", "X'00'", "X'00'"),
        ("oversized", "X'00'", "zeroblob(1048576)"),
    ] {
        let directory = private_tempdir();
        let store = store(directory.path());
        let keys = keys("unit A;");
        publish(&store, &keys, &observations()).unwrap();
        raw(directory.path())
            .execute_batch(&format!(
                "INSERT INTO observation_content (namespace_id, reset_epoch, \
                     content_digest, kind, path_key, payload) SELECT namespace_id, reset_epoch, \
                     zeroblob(32), 3, {path_key}, {payload} FROM observation_content LIMIT 1; \
                     INSERT INTO evidence_observations SELECT evidence_ref, 3, last_insert_rowid() \
                     FROM evidence_observations WHERE ordinal = 0"
            ))
            .unwrap();
        assert_eq!(
            read_all(&store, &keys),
            Err(Miss::Malformed(Malformed::Count)),
            "{label}"
        );
    }
}

/// l4d-e8bl: a subject accumulates sets, and an ID-ordered cap of 16 can
/// hide the valid one forever. Lookups take the most recent first.
#[test]
fn a_republished_set_is_never_hidden_by_the_candidate_cap() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    let sets: Vec<Vec<Observation>> = (0..=MAX_EVIDENCE_CANDIDATES)
        .map(|index| {
            vec![Observation::Presence {
                path: PathBuf::from(format!("/w/{index}.inc")),
                entry: None,
                present: false,
            }]
        })
        .collect();
    for set in &sets {
        publish(&store, &keys, set).unwrap();
    }
    // The set whose ID sorts last: an ID-ordered cap always leaves it out.
    let payload: Vec<u8> = raw(directory.path())
        .query_row(
            "SELECT c.payload FROM observation_content c \
             JOIN evidence_observations o ON o.observation = c.observation \
             JOIN evidence_sets e ON e.evidence_ref = o.evidence_ref \
             WHERE e.evidence_id = (SELECT max(evidence_id) FROM evidence_sets)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let hidden = vec![decode_observation(&payload).unwrap()];
    // Republishing an identical set makes it the most recent publication.
    assert_eq!(
        publish(&store, &keys, &hidden).unwrap().evidence,
        Published::AlreadyPresent
    );
    let first = |store: &Store| {
        let Lookup::Hit(headers) = store.find_evidence(&keys.context, &keys.root).unwrap() else {
            panic!("published")
        };
        assert_eq!(headers.len(), MAX_EVIDENCE_CANDIDATES);
        let mut reader = store.read_evidence(&headers[0]);
        let Lookup::Hit(Some(batch)) = reader.next_batch().unwrap() else {
            panic!("readable")
        };
        batch.observations().to_vec()
    };
    assert_eq!(first(&store), hidden);
    // A new set is newer still.
    let newest = vec![Observation::Presence {
        path: PathBuf::from("/w/new.inc"),
        entry: None,
        present: false,
    }];
    publish(&store, &keys, &newest).unwrap();
    assert_eq!(first(&store), newest);
}

fn lookup(name: &str) -> LookupKey {
    LookupKey::new(&LookupInputs {
        kind: LookupKind::Unit,
        normalized_name: name,
        path_expression: None,
        importer_directory: None,
    })
}

/// Every observation of one set, or the miss that ended the read.
fn read_set(store: &Store, header: &EvidenceHeader) -> Result<Vec<Observation>, Miss> {
    let mut reader = store.read_evidence(header);
    let mut read = Vec::new();
    loop {
        match reader.next_batch().unwrap() {
            Lookup::Hit(Some(batch)) => read.extend_from_slice(batch.observations()),
            Lookup::Hit(None) => {
                assert!(reader.is_verified());
                return Ok(read);
            }
            Lookup::Miss(miss) => return Err(miss),
        }
    }
}

#[test]
fn binding_and_discovery_evidence_publish_and_read_back_verified() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    let shapes = lookup("shapes");
    let scope = DiscoveryScope::new(&DiscoveryInputs {
        directories: &[ScannedDirectory {
            path: Path::new("/w"),
            recursive: true,
        }],
        exclusions: &[],
    });
    let observations = observations();
    let write = EvidenceWrite {
        context: &keys.context,
        observations: &observations,
    };
    let binding = store
        .publish_evidence(EvidenceTarget::Binding(&shapes), &write)
        .unwrap();
    let discovery = store
        .publish_evidence(EvidenceTarget::Discovery(&scope), &write)
        .unwrap();
    assert_eq!(binding.published, Published::Inserted);
    assert_ne!(binding.evidence_id, discovery.evidence_id);
    assert_eq!(
        store
            .publish_evidence(EvidenceTarget::Binding(&shapes), &write)
            .unwrap()
            .published,
        Published::AlreadyPresent
    );
    for (target, published, purpose) in [
        (
            EvidenceTarget::Binding(&shapes),
            binding,
            EvidencePurpose::Binding,
        ),
        (
            EvidenceTarget::Discovery(&scope),
            discovery,
            EvidencePurpose::Discovery,
        ),
    ] {
        let Lookup::Hit(headers) = store.find_subject_evidence(&keys.context, target).unwrap()
        else {
            panic!("{purpose:?} evidence is found by its subject")
        };
        assert_eq!(headers.len(), 1);
        let header = &headers[0];
        assert_eq!(
            (header.evidence_id(), header.purpose(), header.artifact_id()),
            (published.evidence_id, purpose, None)
        );
        assert_eq!(
            header.evidence_id(),
            evidence_id(&header.subject(), None, &observations)
        );
        assert_eq!(read_set(&store, header), Ok(observations.clone()));
        assert_eq!(
            store.open_evidence(published.evidence_id).unwrap(),
            Lookup::Hit(header.clone())
        );
    }
    assert_eq!(
        store
            .find_subject_evidence(&keys.context, EvidenceTarget::Binding(&lookup("other")))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    assert_eq!(
        store.find_evidence(&keys.context, &keys.root).unwrap(),
        Lookup::Miss(Miss::Absent),
        "binding evidence is not artifact evidence"
    );
}

#[test]
fn subject_lookups_take_the_newest_publications_first() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    let shapes = lookup("shapes");
    let set = |name: &str| {
        vec![Observation::Presence {
            path: PathBuf::from(format!("/w/{name}.pas")),
            entry: None,
            present: true,
        }]
    };
    let publish_set = |observations: &[Observation]| {
        store
            .publish_evidence(
                EvidenceTarget::Binding(&shapes),
                &EvidenceWrite {
                    context: &keys.context,
                    observations,
                },
            )
            .unwrap()
    };
    let ids: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|name| publish_set(&set(name)).evidence_id)
        .collect();
    let headers = |store: &Store| {
        let Lookup::Hit(headers) = store
            .find_subject_evidence(&keys.context, EvidenceTarget::Binding(&shapes))
            .unwrap()
        else {
            panic!("published")
        };
        assert!(
            headers
                .windows(2)
                .all(|pair| pair[0].publication_seq() > pair[1].publication_seq())
        );
        headers
            .iter()
            .map(EvidenceHeader::evidence_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(headers(&store), [ids[2], ids[1], ids[0]]);
    assert_eq!(publish_set(&set("a")).published, Published::AlreadyPresent);
    assert_eq!(
        headers(&store),
        [ids[0], ids[2], ids[1]],
        "republishing bumps"
    );
}

#[test]
fn evidence_opened_by_id_is_checked_against_its_manifests() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    let shapes = lookup("shapes");
    let observations = observations();
    let published = store
        .publish_evidence(
            EvidenceTarget::Binding(&shapes),
            &EvidenceWrite {
                context: &keys.context,
                observations: &observations,
            },
        )
        .unwrap();
    assert!(matches!(
        store.open_evidence(published.evidence_id).unwrap(),
        Lookup::Hit(header) if header.purpose() == EvidencePurpose::Binding
    ));
    assert_eq!(
        store.open_evidence(Digest::from_bytes([7; 32])).unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    let mut manifest = shapes.key().manifest().to_vec();
    *manifest.last_mut().unwrap() ^= 1;
    raw(directory.path())
        .execute(
            "UPDATE key_manifests SET manifest = ?1 WHERE digest = ?2",
            rusqlite::params![manifest, shapes.digest().as_bytes().as_slice()],
        )
        .unwrap();
    assert_eq!(
        store.open_evidence(published.evidence_id).unwrap(),
        Lookup::Miss(Miss::KeyMismatch(Mismatch::DigestDiffers))
    );
    raw(directory.path())
        .execute("UPDATE evidence_sets SET lifecycle = 2", [])
        .unwrap();
    assert_eq!(
        store.open_evidence(published.evidence_id).unwrap(),
        Lookup::Miss(Miss::Retired)
    );
}

/// Spec §10: evidence is bounded from its inputs and charged before it is
/// encoded, for artifact evidence and for binding evidence alike.
#[test]
fn evidence_whose_bound_exceeds_the_scratch_is_refused_before_encoding() {
    let long = Observation::Presence {
        path: PathBuf::from(format!("/{}", "x".repeat(5000))),
        entry: None,
        present: false,
    };
    let many = vec![long; 128];
    let keys = keys("unit A;");
    let small = private_tempdir();
    let limited = Store::open(
        small.path(),
        &Namespace::new(Digest::from_bytes([5; 32])),
        StoreLimits {
            scratch_bytes: MAX_ROW_BATCH_BYTES as u64,
            ..StoreLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        publish(&limited, &keys, &many),
        Err(Bypass::ScratchExhausted)
    );
    assert_eq!(
        limited
            .publish_evidence(
                EvidenceTarget::Binding(&lookup("a")),
                &EvidenceWrite {
                    context: &keys.context,
                    observations: &many,
                },
            )
            .err(),
        Some(Bypass::ScratchExhausted)
    );
    assert_eq!(limited.scratch_in_use(), 0);
    let roomy = private_tempdir();
    let default = store(roomy.path());
    assert!(matches!(
        publish(&default, &keys, &many),
        Err(Bypass::TooLarge(TooLarge::IndexKey { ordinal: 0, .. }))
    ));
    assert_eq!(
        default.scratch_in_use(),
        0,
        "the refusal releases its scratch"
    );
}

/// Spec §10: the candidate headers are charged to the scratch before their
/// vector is allocated; with no scratch left the lookups bypass.
#[test]
fn evidence_lookups_reserve_their_headers_before_allocating_them() {
    let directory = private_tempdir();
    let store = store(directory.path());
    let keys = keys("unit A;");
    publish(&store, &keys, &observations()).unwrap();
    let binding = lookup("a");
    let write = EvidenceWrite {
        context: &keys.context,
        observations: &observations(),
    };
    store
        .publish_evidence(EvidenceTarget::Binding(&binding), &write)
        .unwrap();
    let free = store.limits().scratch_bytes - store.scratch_in_use();
    let hold = store.hold_scratch_for_test(free).unwrap();
    assert_eq!(
        store.find_evidence(&keys.context, &keys.root).err(),
        Some(Bypass::ScratchExhausted)
    );
    assert_eq!(
        store
            .find_subject_evidence(&keys.context, EvidenceTarget::Binding(&binding))
            .err(),
        Some(Bypass::ScratchExhausted)
    );
    drop(hold);
    assert!(matches!(
        store.find_evidence(&keys.context, &keys.root),
        Ok(Lookup::Hit(_))
    ));
    assert!(matches!(
        store.find_subject_evidence(&keys.context, EvidenceTarget::Binding(&binding)),
        Ok(Lookup::Hit(_))
    ));
    assert_eq!(store.scratch_in_use(), 0, "nothing stays reserved");
}
