use pascal_cache::{
    Binding, BindingKind, BindingOutcome, CoverageStatus, Digest, IncompleteReason,
    MANIFEST_DIGEST_CONTEXT, MAX_AMBIGUOUS_CANDIDATES, MemberCoverage, MemberError, MemberHeader,
    MemberRecord, RecordDecodeError, SnapshotCoverage, SourceIdentity, decode_binding,
    decode_member_header, decode_snapshot_coverage,
};
use pascal_project::SourceOrigin;

fn identity(path: &str) -> SourceIdentity {
    SourceIdentity {
        canonical: path.into(),
        logical: path.into(),
        spelling: None,
        uri: None,
    }
}

fn header() -> MemberHeader {
    MemberHeader {
        source: SourceIdentity {
            canonical: "/w/Main.pas".into(),
            logical: "/w/./Main.pas".into(),
            spelling: Some("Main".into()),
            uri: Some("file:///w/Main.pas".into()),
        },
        artifact: Some(Digest::from_bytes([1; 32])),
        member_evidence: Some(Digest::from_bytes([2; 32])),
        origin: SourceOrigin::ProjectCompiled,
        fallback_unit_name: Some("Main".into()),
        coverage: MemberCoverage {
            extraction: CoverageStatus::Complete,
            resolution: CoverageStatus::Incomplete(IncompleteReason::CompiledProvider),
        },
    }
}

fn record() -> MemberRecord {
    let evidence = Some(Digest::from_bytes([3; 32]));
    MemberRecord {
        header: header(),
        bindings: vec![
            Binding {
                kind: BindingKind::Import,
                occurrence: 0,
                outcome: BindingOutcome::Precise {
                    target: identity("/w/Shapes.pas"),
                },
                evidence,
            },
            Binding {
                kind: BindingKind::Import,
                occurrence: 1,
                outcome: BindingOutcome::Unavailable,
                evidence,
            },
            Binding {
                kind: BindingKind::Include,
                occurrence: 0,
                outcome: BindingOutcome::Ambiguous {
                    candidates: vec![identity("/a/X.inc"), identity("/b/X.inc")],
                    overflow: false,
                },
                evidence,
            },
            Binding {
                kind: BindingKind::Include,
                occurrence: 4,
                outcome: BindingOutcome::Incomplete(IncompleteReason::Unsupported),
                evidence: None,
            },
        ],
    }
}

#[test]
fn records_round_trip_through_their_encodings() {
    let record = record();
    assert_eq!(record.validate(), Ok(()));
    assert_eq!(
        decode_member_header(&record.header.encoded()),
        Ok(record.header.clone())
    );
    for binding in &record.bindings {
        assert_eq!(decode_binding(&binding.encoded()), Ok(binding.clone()));
    }
    let coverage = SnapshotCoverage {
        discovery: CoverageStatus::Incomplete(IncompleteReason::Unauthorized),
        membership: CoverageStatus::Complete,
    };
    assert_eq!(decode_snapshot_coverage(&coverage.encoded()), Ok(coverage));
    for reason in [
        IncompleteReason::Unsupported,
        IncompleteReason::UnknownConditional,
        IncompleteReason::CompiledProvider,
        IncompleteReason::OverSizeLimit,
        IncompleteReason::Unauthorized,
    ] {
        assert_eq!(IncompleteReason::from_tag(reason.tag()), Some(reason));
    }
    assert_eq!(IncompleteReason::from_tag(0), None);
    assert_eq!(
        BindingKind::from_tag(BindingKind::Include.tag()),
        Some(BindingKind::Include)
    );
}

#[test]
fn the_record_id_is_the_digest_of_the_streamed_manifest() {
    let record = record();
    let header = record.header.encoded();
    let mut manifest = vec![b'L', b'4', b'D', b'C', 1, 0, 0, 0, 9]; // header, MemberRecord
    manifest.extend([1, 0]);
    manifest.extend((header.len() as u64).to_le_bytes());
    manifest.extend(&header);
    manifest.extend([2, 0]);
    manifest.extend((record.bindings.len() as u64).to_le_bytes());
    for binding in &record.bindings {
        manifest.extend(binding.encoded());
    }
    assert_eq!(
        *record.id().unwrap().as_bytes(),
        blake3::derive_key(MANIFEST_DIGEST_CONTEXT, &manifest)
    );
}

#[test]
fn record_ids_cover_every_field() {
    let base = record().id().unwrap();
    let mut changes: Vec<MemberRecord> = Vec::new();
    let mut change = |edit: fn(&mut MemberRecord)| {
        let mut changed = record();
        edit(&mut changed);
        changes.push(changed);
    };
    change(|r| r.header.source.logical = "/w/Main.pas".into());
    change(|r| r.header.source.spelling = None);
    change(|r| r.header.artifact = Some(Digest::from_bytes([9; 32])));
    change(|r| r.header.member_evidence = None);
    change(|r| r.header.origin = SourceOrigin::Library);
    change(|r| r.header.fallback_unit_name = None);
    change(|r| r.header.coverage.resolution = CoverageStatus::Complete);
    change(|r| r.bindings[3].occurrence = 7);
    change(|r| r.bindings[1].outcome = BindingOutcome::Incomplete(IncompleteReason::OverSizeLimit));
    change(|r| r.bindings[1].evidence = Some(Digest::from_bytes([4; 32])));
    change(|r| {
        r.bindings.pop();
    });
    for changed in changes {
        assert_ne!(changed.id().unwrap(), base, "{changed:?}");
    }
}

#[test]
fn invalid_records_are_refused() {
    let check = |edit: fn(&mut MemberRecord)| {
        let mut changed = record();
        edit(&mut changed);
        let validated = changed.validate();
        assert_eq!(
            changed.id().err(),
            validated.err(),
            "no ID for an invalid record"
        );
        validated.unwrap_err()
    };
    assert_eq!(
        check(|r| r.header.artifact = None),
        MemberError::EvidenceWithoutArtifact
    );
    assert_eq!(
        check(|r| r.bindings.swap(0, 1)),
        MemberError::BindingOrder { ordinal: 1 }
    );
    assert_eq!(
        check(|r| r.bindings[1].occurrence = 0),
        MemberError::BindingOrder { ordinal: 1 },
        "one binding per occurrence"
    );
    assert_eq!(
        check(|r| r.bindings[1].evidence = None),
        MemberError::MissingEvidence { ordinal: 1 }
    );
    assert_eq!(
        check(|r| {
            r.bindings[2].outcome = BindingOutcome::Ambiguous {
                candidates: vec![identity("/a/X.inc")],
                overflow: false,
            }
        }),
        MemberError::Candidates { ordinal: 2 }
    );
    assert_eq!(
        check(|r| {
            r.bindings[2].outcome = BindingOutcome::Ambiguous {
                candidates: vec![identity("/a/X.inc"), identity("/b/X.inc")],
                overflow: true,
            }
        }),
        MemberError::Candidates { ordinal: 2 },
        "overflow only past a full candidate list"
    );
    assert_eq!(
        check(|r| {
            r.bindings[2].outcome = BindingOutcome::Ambiguous {
                candidates: (0..=MAX_AMBIGUOUS_CANDIDATES)
                    .map(|index| identity(&format!("/{index}/X.inc")))
                    .collect(),
                overflow: true,
            }
        }),
        MemberError::Candidates { ordinal: 2 }
    );
    let mut full = record();
    full.bindings[2].outcome = BindingOutcome::Ambiguous {
        candidates: (0..MAX_AMBIGUOUS_CANDIDATES)
            .map(|index| identity(&format!("/{index}/X.inc")))
            .collect(),
        overflow: true,
    };
    assert_eq!(full.validate(), Ok(()));
}

#[cfg(unix)]
#[test]
fn binding_encoding_is_pinned() {
    let binding = Binding {
        kind: BindingKind::Import,
        occurrence: 3,
        outcome: BindingOutcome::Unavailable,
        evidence: Some(Digest::from_bytes([7; 32])),
    };
    let mut expected = vec![1, 0, 1]; // kind Import
    expected.extend([2, 0, 3, 0, 0, 0, 0, 0, 0, 0]); // occurrence
    expected.extend([3, 0, 3]); // outcome Unavailable
    expected.extend([4, 0, 1]); // evidence
    expected.extend([7; 32]);
    assert_eq!(binding.encoded(), expected);
}

#[cfg(unix)]
#[test]
fn member_header_encoding_is_pinned() {
    let header = MemberHeader {
        source: identity("/a"),
        artifact: None,
        member_evidence: None,
        origin: SourceOrigin::Library,
        fallback_unit_name: None,
        coverage: MemberCoverage {
            extraction: CoverageStatus::Complete,
            resolution: CoverageStatus::Incomplete(IncompleteReason::Unsupported),
        },
    };
    let mut expected = vec![1, 0]; // source
    expected.extend([1, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, b'/', b'a']); //   canonical
    expected.extend([2, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, b'/', b'a']); //   logical
    expected.extend([3, 0, 0, 4, 0, 0]); //   spelling, uri
    expected.extend([2, 0, 0]); // artifact
    expected.extend([3, 0, 0]); // member evidence
    expected.extend([4, 0, 2]); // origin Library
    expected.extend([5, 0, 0]); // fallback name
    expected.extend([6, 0, 1, 0, 1, 2, 0, 2, 1]); // coverage
    assert_eq!(header.encoded(), expected);
}

#[test]
fn malformed_encodings_are_refused() {
    let binding = record().bindings[0].encoded();
    let header = record().header.encoded();
    let mut trailing = binding.clone();
    trailing.push(0);
    let mut unknown_outcome = record().bindings[1].encoded();
    unknown_outcome[15] = 9; // the outcome variant after kind, occurrence and its tag
    let mut missing_evidence = record().bindings[1].encoded();
    missing_evidence.truncate(18);
    missing_evidence.push(0);
    for (label, bytes) in [
        ("truncated", binding[..binding.len() - 1].to_vec()),
        ("trailing", trailing),
        ("unknown outcome", unknown_outcome),
        ("evidence-less unavailable binding", missing_evidence),
        ("empty", Vec::new()),
    ] {
        assert!(
            matches!(decode_binding(&bytes), Err(RecordDecodeError(_))),
            "{label}"
        );
    }
    assert!(decode_member_header(&header[..header.len() - 1]).is_err());
    let mut no_artifact = record();
    no_artifact.header.artifact = None;
    assert!(
        decode_member_header(&no_artifact.header.encoded()).is_err(),
        "member evidence requires an artifact"
    );
}
