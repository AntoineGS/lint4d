use pascal_cache::{
    Digest, EvidenceSubject, Observation, ObservationError, SourceContent, decode_observation,
    evidence_id, listing_digest, observations_digest,
};
use pascal_project::{ProjectPathEntry, ProjectPathProvenance};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(if cfg!(windows) { r"C:\w" } else { "/w" })
}

fn sample() -> Vec<Observation> {
    let root = root();
    vec![
        Observation::Payload {
            path: root.join("a.inc"),
            entry: ProjectPathEntry {
                path: root.join("a.inc"),
                provenance: ProjectPathProvenance::Mapped { root: root.clone() },
            },
            content: SourceContent::from_bytes(b"const A = 1;"),
        },
        Observation::Presence {
            path: root.join("b.inc"),
            entry: Some(ProjectPathEntry::configured(root.join("b.inc"))),
            present: false,
        },
        Observation::Presence {
            path: root.join("c.inc"),
            entry: None,
            present: true,
        },
        Observation::Listing {
            path: root.clone(),
            entry: ProjectPathEntry::legacy(root.clone()),
            listing: listing_digest(&[root.join("a.inc")], &[root.join("sub")]),
        },
    ]
}

#[test]
fn observations_round_trip_through_their_encoding() {
    for observation in sample() {
        assert_eq!(
            decode_observation(&observation.encoded()),
            Ok(observation.clone()),
            "{observation:?}"
        );
    }
}

#[test]
fn malformed_observations_are_refused() {
    let payload = sample()[0].encoded();
    let presence = sample()[1].encoded();
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("truncated", payload[..payload.len() - 1].to_vec()),
        ("trailing", [payload.as_slice(), &[0]].concat()),
        ("unknown kind", [&[9u8][..], &payload[1..]].concat()),
        ("empty", Vec::new()),
    ];
    let mut field = payload.clone();
    field[1] = 7;
    cases.push(("field tag", field));
    let mut flag = presence.clone();
    *flag.last_mut().unwrap() = 2;
    cases.push(("bool", flag));
    // A payload ends with its content's encoding tag.
    let mut encoding = payload.clone();
    *encoding.last_mut().unwrap() = 9;
    cases.push(("encoding", encoding));
    let mut provenance = sample()[3].encoded();
    let tag = provenance.len() - 32 - 2 - 1;
    provenance[tag] = 9;
    cases.push(("provenance", provenance));
    for (label, bytes) in cases {
        assert!(
            matches!(decode_observation(&bytes), Err(ObservationError(_))),
            "{label}"
        );
    }
}

#[test]
fn listing_digests_name_entries_and_their_kinds_only() {
    let p = |text: &str| PathBuf::from(text);
    let listing = listing_digest(&[p("/x/A.pas"), p("/x/b.inc")], &[p("/x/sub")]);
    assert_eq!(
        listing,
        listing_digest(
            &[p("/y/b.inc"), p("/y/A.pas"), p("/y/b.inc")],
            &[p("/y/sub")]
        ),
        "order, duplicates and the parent do not matter"
    );
    assert_ne!(
        listing,
        listing_digest(&[p("/x/A.pas"), p("/x/c.inc")], &[p("/x/sub")]),
        "a renamed entry"
    );
    assert_ne!(
        listing_digest(&[p("/x/sub")], &[]),
        listing_digest(&[], &[p("/x/sub")]),
        "a file is not a directory"
    );
    assert_ne!(
        listing_digest(&[p("/x/A.pas")], &[]),
        listing_digest(&[p("/x/a.pas")], &[]),
        "names are exact"
    );
    assert_ne!(
        listing_digest(&[], &[]),
        listing,
        "an empty listing is a listing"
    );
}

#[test]
fn evidence_ids_cover_subject_conclusion_and_every_observation() {
    let subject = EvidenceSubject {
        context: Digest::from_bytes([1; 32]),
        root: Digest::from_bytes([2; 32]),
    };
    let conclusion = Digest::from_bytes([3; 32]);
    let observations = sample();
    let id = evidence_id(&subject, conclusion, &observations);
    assert_eq!(
        id,
        evidence_id(&subject, conclusion, &observations),
        "deterministic"
    );
    let mut reordered = observations.clone();
    reordered.swap(0, 1);
    let other = Digest::from_bytes([4; 32]);
    for changed in [
        evidence_id(
            &EvidenceSubject {
                context: other,
                ..subject
            },
            conclusion,
            &observations,
        ),
        evidence_id(
            &EvidenceSubject {
                root: other,
                ..subject
            },
            conclusion,
            &observations,
        ),
        evidence_id(&subject, other, &observations),
        evidence_id(&subject, conclusion, &observations[1..]),
        evidence_id(&subject, conclusion, &reordered),
    ] {
        assert_ne!(changed, id);
    }
    assert_ne!(
        observations_digest(&observations),
        observations_digest(&reordered)
    );
    assert_ne!(observations_digest(&observations), id, "domain separated");
}

#[cfg(unix)]
#[test]
fn observation_encoding_is_pinned() {
    let observation = Observation::Presence {
        path: PathBuf::from("/a"),
        entry: None,
        present: true,
    };
    assert_eq!(
        observation.encoded(),
        [
            2, 1, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, b'/', b'a', 2, 0, 0, 3, 0, 1
        ]
    );
    assert_eq!(observation.kind(), 2);
}
