use pascal_cache::{
    DECLARATION_ROW, EncodedFacts, FactDecodeError, FactDecoder, IMPORT_ROW, INCLUDE_ROW,
    MAX_INDEX_KEY_BYTES, MAX_ROW_BATCH_BYTES, POSTING_ROW, SEGMENT_ROW, StoredRow, TooLarge,
    encode_facts, encoded_content_bytes,
};
use pascal_core::ImportSection;
use pascal_core::facts::*;
use std::path::{Path, PathBuf};

fn at(slot: u32, start: u32, end: u32) -> FactLocation {
    FactLocation::Source {
        slot,
        range: TextRange { start, end },
    }
}

fn certain(activity: Activity, recovered: bool) -> Certainty {
    Certainty {
        activity,
        recovered,
    }
}

fn occurrence(slot: u32, start: u32, end: u32, activity: PostingActivity) -> PostingOccurrence {
    PostingOccurrence {
        slot,
        range: TextRange { start, end },
        activity,
    }
}

/// Every variant of every tagged field, with a two-include source map.
fn sample() -> ArtifactFacts {
    let kinds = [
        DeclarationKind::Unit,
        DeclarationKind::Type,
        DeclarationKind::Routine,
        DeclarationKind::Variable,
        DeclarationKind::Constant,
        DeclarationKind::Field,
        DeclarationKind::Property,
        DeclarationKind::EnumValue,
    ];
    let visibilities = [
        Visibility::Public,
        Visibility::Published,
        Visibility::Private,
        Visibility::Protected,
        Visibility::StrictPrivate,
        Visibility::StrictProtected,
    ];
    let regions = [Region::Interface, Region::Implementation, Region::Other];
    let activities = [Activity::Active, Activity::Inactive, Activity::Unknown];
    let mut declarations: Vec<_> = kinds
        .iter()
        .enumerate()
        .map(|(index, kind)| DeclarationFact {
            id: index as u32,
            name: format!("d{index}"),
            display: format!("D{index}"),
            kind: *kind,
            visibility: visibilities[index % 6],
            region: regions[index % 3],
            origin: if index % 2 == 0 {
                Origin::Declaration
            } else {
                Origin::Definition
            },
            exported: index % 2 == 0,
            parent: (index > 0).then_some(0),
            location: at(0, index as u32 * 2, index as u32 * 2 + 2),
            certainty: certain(activities[index % 3], index == 4),
            extent: at(0, index as u32 * 2, index as u32 * 2 + 6),
            shape: DeclarationShape::Constant,
            unit_scope: index % 2 == 0,
            owner: (index > 0).then(|| format!("TOwner{index}")),
        })
        .collect();
    declarations[7].location = FactLocation::Expanded {
        range: TextRange {
            start: 100,
            end: 103,
        },
    };
    let imports = [
        ImportSection::Module,
        ImportSection::Interface,
        ImportSection::Implementation,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, section)| ImportFact {
        id: index as u32,
        name: format!("system.unit{index}"),
        display: format!("System.Unit{index}"),
        section,
        location: at(1, 0, 10),
        certainty: certain(activities[index], false),
    })
    .collect();
    ArtifactFacts {
        slot_lengths: vec![120, 30, 0],
        expanded_length: 140,
        declarations,
        imports,
        postings: vec![
            PostingFact {
                name: "bar".into(),
                occurrences: vec![
                    occurrence(0, 40, 43, PostingActivity::Inactive),
                    occurrence(1, 2, 5, PostingActivity::Directive),
                ],
            },
            PostingFact {
                name: "tfoo".into(),
                occurrences: vec![
                    occurrence(0, 30, 34, PostingActivity::Unknown),
                    occurrence(0, 50, 54, PostingActivity::Active),
                ],
            },
        ],
        includes: vec![
            IncludeFact {
                slot: 1,
                parent: 0,
                directive: TextRange { start: 60, end: 70 },
                expansion: IncludeExpansion::Expanded,
                source: Some(PathBuf::from("/w/include/Types.inc")),
            },
            IncludeFact {
                slot: 2,
                parent: 0,
                directive: TextRange {
                    start: 104,
                    end: 118,
                },
                expansion: IncludeExpansion::Unresolved,
                source: None,
            },
        ],
        segments: vec![
            SegmentFact {
                expanded: TextRange { start: 0, end: 60 },
                target: SegmentTarget::Source { slot: 0, start: 0 },
            },
            SegmentFact {
                expanded: TextRange { start: 60, end: 70 },
                target: SegmentTarget::Synthetic,
            },
            SegmentFact {
                expanded: TextRange {
                    start: 70,
                    end: 100,
                },
                target: SegmentTarget::Source { slot: 1, start: 0 },
            },
            SegmentFact {
                expanded: TextRange {
                    start: 100,
                    end: 130,
                },
                target: SegmentTarget::Source { slot: 0, start: 70 },
            },
            SegmentFact {
                expanded: TextRange {
                    start: 130,
                    end: 140,
                },
                target: SegmentTarget::Synthetic,
            },
        ],
        coverage: vec![
            CoverageReason::ConditionalUnknown,
            CoverageReason::ParserRecovery,
            CoverageReason::UnresolvedInclude,
            CoverageReason::ExpansionIncomplete,
            CoverageReason::UnmappedLocation,
        ],
    }
}

fn stored(encoded: &EncodedFacts) -> Vec<StoredRow> {
    encoded
        .rows
        .iter()
        .enumerate()
        .map(|(ordinal, row)| StoredRow {
            ordinal: ordinal as u64,
            row_kind: row.row_kind,
            index_key: row.index_key.clone(),
            payload: row.payload.clone(),
        })
        .collect()
}

fn decode(rows: &[StoredRow], supplemental: &[u8]) -> Result<ArtifactFacts, FactDecodeError> {
    let mut budget = FactBudget::new(1 << 26);
    let mut decoder = FactDecoder::new(&mut budget);
    for batch in rows.chunks(2) {
        decoder.push(batch)?;
    }
    decoder.finish(supplemental)
}

#[test]
fn facts_round_trip_through_rows() {
    let facts = sample();
    let encoded = encode_facts(&facts).unwrap();
    assert_eq!(encoded_content_bytes(&facts), Ok(encoded.content_bytes()));
    let kinds: Vec<u8> = encoded.rows.iter().map(|row| row.row_kind).collect();
    let mut expected = vec![DECLARATION_ROW; 8];
    expected.extend([IMPORT_ROW; 3]);
    expected.extend([POSTING_ROW; 2]);
    expected.extend([INCLUDE_ROW; 2]);
    expected.extend([SEGMENT_ROW; 5]);
    assert_eq!(kinds, expected);
    assert_eq!(
        encoded.rows[11].index_key.as_deref(),
        Some(b"bar".as_slice())
    );
    assert_eq!(encoded.rows[13].index_key, None);
    assert_eq!(decode(&stored(&encoded), &encoded.supplemental), Ok(facts));
}

#[test]
fn declaration_rows_round_trip_extent_shape_scope_and_owner() {
    let shapes = [
        DeclarationShape::Unit,
        DeclarationShape::Class,
        DeclarationShape::Record,
        DeclarationShape::Interface,
        DeclarationShape::Enum,
        DeclarationShape::Array,
        DeclarationShape::Pointer,
        DeclarationShape::Callable,
        DeclarationShape::String,
        DeclarationShape::File,
        DeclarationShape::OtherType,
        DeclarationShape::Constructor,
        DeclarationShape::Function,
        DeclarationShape::Operator,
        DeclarationShape::Method,
        DeclarationShape::Procedure,
        DeclarationShape::Variable,
        DeclarationShape::Constant,
        DeclarationShape::Field,
        DeclarationShape::Property,
        DeclarationShape::EnumValue,
    ];
    let mut facts = sample();
    facts.declarations = shapes
        .iter()
        .enumerate()
        .map(|(index, shape)| {
            let at_index = index as u32;
            DeclarationFact {
                id: at_index,
                name: format!("d{index}"),
                display: format!("D{index}"),
                kind: DeclarationKind::Type,
                visibility: Visibility::Public,
                region: Region::Interface,
                origin: Origin::Declaration,
                exported: true,
                parent: None,
                location: at(0, at_index, at_index + 1),
                certainty: certain(Activity::Active, false),
                extent: if index % 2 == 0 {
                    at(0, at_index, at_index + 5)
                } else {
                    FactLocation::Expanded {
                        range: TextRange {
                            start: at_index,
                            end: at_index + 5,
                        },
                    }
                },
                shape: *shape,
                unit_scope: index % 3 == 0,
                owner: (index % 2 == 1).then(|| format!("TOwner{index}")),
            }
        })
        .collect();
    let encoded = encode_facts(&facts).unwrap();
    assert_eq!(encoded_content_bytes(&facts), Ok(encoded.content_bytes()));
    assert_eq!(decode(&stored(&encoded), &encoded.supplemental), Ok(facts));
}

#[test]
fn an_unknown_shape_tag_is_malformed() {
    let facts = sample();
    let mut rows = stored(&encode_facts(&facts).unwrap());
    // The first declaration has no owner, so its payload ends with the shape
    // tag, the unit-scope flag and the empty owner option.
    let payload = &mut rows[0].payload;
    let shape_at = payload.len() - 1 - 1 - 1;
    assert!((1..=21).contains(&payload[shape_at]), "{payload:?}");
    payload[shape_at] = 99;
    assert!(matches!(
        decode(&rows, &encode_facts(&facts).unwrap().supplemental),
        Err(FactDecodeError::Malformed(_))
    ));
}

/// D35: an expanded include keeps the path it read through the rows.
#[test]
fn include_sources_round_trip() {
    let facts = sample();
    let encoded = encode_facts(&facts).unwrap();
    let loaded = decode(&stored(&encoded), &encoded.supplemental).unwrap();
    assert_eq!(
        loaded.includes[0].source.as_deref(),
        Some(Path::new("/w/include/Types.inc"))
    );
    assert_eq!(loaded.includes[1].source, None);
    assert_eq!(loaded, facts);
}

/// A source on an unresolved include, or none on an expanded one, is malformed.
#[test]
fn an_include_source_must_match_its_expansion() {
    let mismatch = "include source does not match its expansion";
    let mut facts = sample();
    facts.includes[1].source = Some(PathBuf::from("/w/Missing.inc"));
    let encoded = encode_facts(&facts).unwrap();
    assert_eq!(
        decode(&stored(&encoded), &encoded.supplemental),
        Err(FactDecodeError::Malformed(mismatch))
    );
    let mut facts = sample();
    facts.includes[0].source = None;
    let encoded = encode_facts(&facts).unwrap();
    assert_eq!(
        decode(&stored(&encoded), &encoded.supplemental),
        Err(FactDecodeError::Malformed(mismatch))
    );
}

#[test]
fn a_long_posting_list_spans_rows_and_round_trips() {
    let per_row = (MAX_ROW_BATCH_BYTES - 1 - 8) / 13;
    let occurrences: Vec<_> = (0..per_row as u32 + 3)
        .map(|index| occurrence(0, index * 2, index * 2 + 1, PostingActivity::Active))
        .collect();
    let facts = ArtifactFacts {
        slot_lengths: vec![occurrences.len() as u32 * 2],
        expanded_length: occurrences.len() as u32 * 2,
        postings: vec![PostingFact {
            name: "x".into(),
            occurrences,
        }],
        ..ArtifactFacts::default()
    };
    let encoded = encode_facts(&facts).unwrap();
    assert_eq!(encoded.rows.len(), 2);
    assert!(
        encoded
            .rows
            .iter()
            .all(|row| row.payload.len() < MAX_ROW_BATCH_BYTES)
    );
    assert_eq!(decode(&stored(&encoded), &encoded.supplemental), Ok(facts));
}

#[test]
fn oversized_facts_are_refused_before_encoding() {
    let mut facts = sample();
    facts.postings[0].name = "a".repeat(MAX_INDEX_KEY_BYTES + 1);
    assert!(matches!(
        encoded_content_bytes(&facts),
        Err(TooLarge::IndexKey { bytes, .. }) if bytes == MAX_INDEX_KEY_BYTES + 1
    ));
    assert!(encode_facts(&facts).is_err());
    let mut facts = sample();
    facts.slot_lengths = vec![0; 300_000];
    assert!(matches!(
        encode_facts(&facts),
        Err(TooLarge::Supplemental { .. })
    ));
}

#[test]
fn malformed_rows_are_refused() {
    let facts = sample();
    let encoded = encode_facts(&facts).unwrap();
    let base = stored(&encoded);
    let refused = |rows: Vec<StoredRow>, supplemental: &[u8], case: &str| {
        assert!(
            matches!(
                decode(&rows, supplemental),
                Err(FactDecodeError::Malformed(_))
            ),
            "{case}"
        );
    };
    let edit = |change: &dyn Fn(&mut Vec<StoredRow>)| {
        let mut rows = base.clone();
        change(&mut rows);
        rows
    };
    refused(
        edit(&|rows| rows.swap(0, 19)),
        &encoded.supplemental,
        "kind order",
    );
    refused(
        edit(&|rows| rows[11].index_key = Some(b"BAR".to_vec())),
        &encoded.supplemental,
        "unnormalized posting name",
    );
    refused(
        edit(&|rows| rows[11].index_key = Some(b"ba".to_vec())),
        &encoded.supplemental,
        "posting range is not the name",
    );
    refused(
        edit(&|rows| rows[12].index_key = Some(b"aaa".to_vec())),
        &encoded.supplemental,
        "posting names out of order",
    );
    refused(
        edit(&|rows| rows[1].payload[0] = 5),
        &encoded.supplemental,
        "declaration id gap",
    );
    refused(
        edit(&|rows| rows[13].payload[0] = 2),
        &encoded.supplemental,
        "include slot gap",
    );
    refused(
        edit(&|rows| rows[2].payload.push(0)),
        &encoded.supplemental,
        "trailing byte",
    );
    refused(
        edit(&|rows| rows[0].payload[4 + 8 + 2] = 99),
        &encoded.supplemental,
        "unknown kind tag",
    );
    refused(
        edit(&|rows| rows[16].payload[0] = 1),
        &encoded.supplemental,
        "segment gap",
    );
    refused(
        edit(&|rows| {
            rows.pop();
        }),
        &encoded.supplemental,
        "missing row",
    );
    refused(
        edit(&|rows| rows[13].index_key = Some(b"x".to_vec())),
        &encoded.supplemental,
        "keyed include",
    );
    let mut supplemental = encoded.supplemental.clone();
    supplemental[0] += 1;
    refused(base.clone(), &supplemental, "count mismatch");
    let mut supplemental = encoded.supplemental.clone();
    supplemental.push(0);
    refused(base.clone(), &supplemental, "trailing header byte");

    let mut beyond = facts.clone();
    beyond.declarations[1].location = at(0, 118, 125);
    let encoded = encode_facts(&beyond).unwrap();
    refused(
        stored(&encoded),
        &encoded.supplemental,
        "range beyond its slot",
    );
    let mut orphan = facts.clone();
    orphan.declarations[1].parent = Some(9);
    let encoded = encode_facts(&orphan).unwrap();
    refused(
        stored(&encoded),
        &encoded.supplemental,
        "parent out of range",
    );
    let mut cycle = facts.clone();
    cycle.declarations[1].parent = Some(2);
    cycle.declarations[2].parent = Some(1);
    let encoded = encode_facts(&cycle).unwrap();
    refused(stored(&encoded), &encoded.supplemental, "parent cycle");
    let mut unflagged = facts.clone();
    unflagged
        .coverage
        .retain(|reason| *reason != CoverageReason::UnresolvedInclude);
    let encoded = encode_facts(&unflagged).unwrap();
    refused(
        stored(&encoded),
        &encoded.supplemental,
        "unresolved include without coverage",
    );
    let mut unflagged = facts.clone();
    unflagged
        .coverage
        .retain(|reason| *reason != CoverageReason::UnmappedLocation);
    let encoded = encode_facts(&unflagged).unwrap();
    refused(
        stored(&encoded),
        &encoded.supplemental,
        "expanded location without coverage",
    );
    let mut uncovered = facts.clone();
    uncovered.segments.pop();
    let encoded = encode_facts(&uncovered).unwrap();
    refused(
        stored(&encoded),
        &encoded.supplemental,
        "segments do not cover the text",
    );
}

#[test]
fn decoding_is_charged_before_facts_are_retained() {
    let facts = sample();
    let encoded = encode_facts(&facts).unwrap();
    let rows = stored(&encoded);
    let mut budget = FactBudget::new(200);
    let mut decoder = FactDecoder::new(&mut budget);
    let pushed = rows.chunks(4).try_for_each(|batch| decoder.push(batch));
    assert_eq!(pushed, Err(FactDecodeError::Budget));
    assert!(budget.used() <= 200);
}

#[cfg(unix)]
mod common;

#[cfg(unix)]
mod store {
    use super::*;
    use crate::common::private_tempdir;
    use pascal_cache::{
        ArtifactKeyRef, ArtifactRow, ArtifactWrite, Bypass, Digest, Integrity, LocalArtifactKey,
        Lookup, MAX_ROW_BATCH_BYTES, Malformed, Miss, Namespace, Published, SourceContent, Store,
        StoreLimits,
    };
    use pascal_project::{ConditionalContext, SourceOrigin};
    use std::path::Path;

    fn open(directory: &Path, limits: StoreLimits) -> Store {
        Store::open(
            directory,
            &Namespace::new(Digest::from_bytes([4; 32])),
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

    fn load(store: &Store, key: &LocalArtifactKey) -> Lookup<ArtifactFacts> {
        store
            .load_facts(ArtifactKeyRef::local(key), &mut FactBudget::new(1 << 26))
            .unwrap()
    }

    #[test]
    fn published_facts_load_back_verified_and_equal() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit A;");
        let facts = sample();
        assert_eq!(
            store.publish_facts(ArtifactKeyRef::local(&key), &facts),
            Ok(Published::Inserted)
        );
        let Lookup::Hit(loaded) = load(&store, &key) else {
            panic!("expected the published facts")
        };
        assert_eq!(loaded, facts);
        // Nothing uncertain was upgraded on the way.
        assert!(!loaded.declarations[2].certainty.is_proven());
        assert!(loaded.declarations[4].certainty.recovered);
        assert!(matches!(
            loaded.declarations[7].location,
            FactLocation::Expanded { .. }
        ));
        assert_eq!(loaded.includes[1].expansion, IncludeExpansion::Unresolved);
        assert_eq!(loaded.coverage, facts.coverage);
    }

    #[test]
    fn identical_facts_publish_once_and_different_facts_conflict() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit A;");
        let facts = sample();
        store
            .publish_facts(ArtifactKeyRef::local(&key), &facts)
            .unwrap();
        assert_eq!(
            store.publish_facts(ArtifactKeyRef::local(&key), &facts),
            Ok(Published::AlreadyPresent)
        );
        let mut other = facts.clone();
        other.declarations[0].display = "Other".into();
        assert_eq!(
            store.publish_facts(ArtifactKeyRef::local(&key), &other),
            Err(Bypass::Integrity(Integrity::ConflictingArtifact))
        );
    }

    #[test]
    fn posting_rows_are_candidates_by_binary_name() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit A;");
        store
            .publish_facts(ArtifactKeyRef::local(&key), &sample())
            .unwrap();
        let page = store.find_by_index(POSTING_ROW, b"tfoo", None, 10).unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].artifact_id, key.digest());
        assert!(
            store
                .find_by_index(POSTING_ROW, b"TFOO", None, 10)
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(
            store
                .find_by_index(DECLARATION_ROW, b"tfoo", None, 10)
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn malformed_published_facts_are_a_miss_not_empty_facts() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit A;");
        let encoded = encode_facts(&sample()).unwrap();
        let mut rows: Vec<ArtifactRow<'_>> = encoded.rows();
        rows[11].index_key = Some(b"BAR".as_slice());
        store
            .publish_artifact(&ArtifactWrite {
                key: ArtifactKeyRef::local(&key),
                supplemental: &encoded.supplemental,
                rows: &rows,
            })
            .unwrap();
        let mut budget = FactBudget::new(1 << 26);
        assert_eq!(
            store.load_facts(ArtifactKeyRef::local(&key), &mut budget),
            Ok(Lookup::Miss(Miss::Malformed(Malformed::Facts)))
        );
        assert_eq!(budget.used(), 0);
    }

    /// The rows reach the decoder before the fact checksum is checked, so
    /// a corrupted row is reported as the checksum miss it is, whatever
    /// the decoder made of it and whatever the budget.
    #[test]
    fn a_corrupted_row_is_a_checksum_miss_whatever_the_decoder_makes_of_it() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit A;");
        store
            .publish_facts(ArtifactKeyRef::local(&key), &sample())
            .unwrap();
        let connection =
            rusqlite::Connection::open(root.path().join(pascal_cache::DATABASE_FILE)).unwrap();
        connection
            .execute(
                "UPDATE artifact_rows SET payload = x'ffffffff' WHERE ordinal = 0",
                [],
            )
            .unwrap();
        for limit in [1 << 26, 64] {
            let mut budget = FactBudget::new(limit);
            assert_eq!(
                store.load_facts(ArtifactKeyRef::local(&key), &mut budget),
                Ok(Lookup::Miss(Miss::ChecksumMismatch)),
                "budget {limit}"
            );
            assert_eq!(budget.used(), 0);
            assert_eq!(store.scratch_in_use(), 0);
        }
    }

    #[test]
    fn decoded_facts_are_charged_before_they_are_retained() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit A;");
        store
            .publish_facts(ArtifactKeyRef::local(&key), &sample())
            .unwrap();
        let mut budget = FactBudget::new(64);
        assert_eq!(
            store.load_facts(ArtifactKeyRef::local(&key), &mut budget),
            Err(Bypass::FactBudget)
        );
        assert_eq!(budget.used(), 0);
        assert_eq!(store.scratch_in_use(), 0);
    }

    #[test]
    fn encoding_is_charged_to_scratch_before_it_is_built() {
        let root = private_tempdir();
        let store = open(
            root.path(),
            StoreLimits {
                scratch_bytes: MAX_ROW_BATCH_BYTES as u64,
                ..StoreLimits::default()
            },
        );
        let key = key("unit Big;");
        let occurrences: Vec<_> = (0..90_000u32)
            .map(|index| occurrence(0, index * 2, index * 2 + 1, PostingActivity::Active))
            .collect();
        let facts = ArtifactFacts {
            slot_lengths: vec![180_000],
            expanded_length: 180_000,
            postings: vec![PostingFact {
                name: "x".into(),
                occurrences,
            }],
            ..ArtifactFacts::default()
        };
        assert_eq!(
            store.publish_facts(ArtifactKeyRef::local(&key), &facts),
            Err(Bypass::ScratchExhausted)
        );
        assert_eq!(
            store.open_artifact(ArtifactKeyRef::local(&key)),
            Ok(Lookup::Miss(Miss::Absent))
        );
    }

    #[test]
    fn an_oversized_name_is_not_persisted() {
        let root = private_tempdir();
        let store = open(root.path(), StoreLimits::default());
        let key = key("unit Long;");
        let name = "a".repeat(pascal_cache::MAX_INDEX_KEY_BYTES + 1);
        let facts = ArtifactFacts {
            slot_lengths: vec![name.len() as u32],
            expanded_length: name.len() as u32,
            postings: vec![PostingFact {
                occurrences: vec![occurrence(0, 0, name.len() as u32, PostingActivity::Active)],
                name,
            }],
            ..ArtifactFacts::default()
        };
        assert!(matches!(
            store.publish_facts(ArtifactKeyRef::local(&key), &facts),
            Err(Bypass::TooLarge(TooLarge::IndexKey { .. }))
        ));
        assert_eq!(
            store.open_artifact(ArtifactKeyRef::local(&key)),
            Ok(Lookup::Miss(Miss::Absent))
        );
    }
}
