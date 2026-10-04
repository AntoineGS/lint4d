use std::{ops::Range, sync::Arc};

use cfg_core::{BlockId, Cfg, EdgeKind, StmtRef};
use cfg_pascal::{
    build_file_cfgs, build_file_cfgs_in_project, ExpansionId, ImportBinding, ImportTarget,
    PreparationFidelity, PreparationProvenance, PreparedSource, ProjectSnapshot,
    ProjectSnapshotError, ProjectSourceId, ProjectUnitId, ProjectUnitInput, SourceMap,
    SourceMapError, SourceMapSegment, SourceSegmentKind, SourceSnapshot, SourceSpan, UsesSite,
};
use tree_sitter::{Node, Parser, Tree};

fn parse_clean(source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&cfg_pascal::LANGUAGE.into())
        .expect("failed to set Pascal language");
    let tree = parser.parse(source, None).expect("parser returned no tree");
    assert!(
        !tree.root_node().has_error(),
        "fixture must be parse-clean:\n{}",
        tree.root_node().to_sexp()
    );
    tree
}

fn source(id: &str, bytes: &[u8]) -> SourceSnapshot {
    SourceSnapshot::new(ProjectSourceId::new(id), bytes)
}

fn copied(
    prepared: Range<usize>,
    source_id: &str,
    original: Range<usize>,
    expansion: &str,
) -> SourceMapSegment {
    SourceMapSegment::copied(
        prepared,
        ProjectSourceId::new(source_id),
        original,
        ExpansionId::new(expansion),
    )
}

fn masked(
    prepared: Range<usize>,
    source_id: &str,
    original: Range<usize>,
    expansion: &str,
) -> SourceMapSegment {
    SourceMapSegment::masked(
        prepared,
        ProjectSourceId::new(source_id),
        original,
        ExpansionId::new(expansion),
    )
}

fn uses_spans(tree: &Tree) -> Vec<Range<usize>> {
    fn collect(node: Node<'_>, in_uses: bool, out: &mut Vec<Range<usize>>) {
        let in_uses = in_uses || node.kind() == "declUses";
        if in_uses && node.kind() == "moduleName" {
            out.push(node.start_byte()..node.end_byte());
            return;
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect(child, in_uses, out);
        }
    }

    let mut spans = Vec::new();
    collect(tree.root_node(), false, &mut spans);
    spans
}

fn imported_unit(importer: &str, tree: &Tree, target: &str) -> ImportBinding {
    ImportBinding::new(
        UsesSite::new(ProjectUnitId::new(importer), uses_spans(tree)[0].clone()),
        ImportTarget::Loaded(ProjectUnitId::new(target)),
        [target],
    )
}

fn cfg_for<'a>(cfgs: &'a [Cfg], name: &str) -> &'a Cfg {
    cfgs.iter()
        .find(|cfg| cfg.proc_name == name)
        .unwrap_or_else(|| panic!("CFG {name:?} was not found"))
}

fn block_with_text(cfg: &Cfg, source: &[u8], kind: &str, text: &str) -> BlockId {
    cfg.graph
        .node_indices()
        .map(BlockId::from)
        .find(|block| {
            cfg.graph[block.index()].stmts.iter().any(|stmt| {
                stmt.node_kind == kind
                    && std::str::from_utf8(&source[stmt.byte_range.clone()])
                        .is_ok_and(|value| value.contains(text))
            })
        })
        .unwrap_or_else(|| panic!("{kind:?} statement {text:?} was not found"))
}

fn successors(cfg: &Cfg, from: BlockId) -> Vec<(BlockId, EdgeKind)> {
    cfg.graph
        .edge_indices()
        .filter_map(|edge| {
            let (source, target) = cfg.graph.edge_endpoints(edge)?;
            (source == from.index()).then_some((BlockId::from(target), cfg.graph[edge].clone()))
        })
        .collect()
}

fn successful_raise_block(cfg: &Cfg, raise: BlockId) -> BlockId {
    successors(cfg, raise)
        .into_iter()
        .find_map(|(target, kind)| {
            (kind == EdgeKind::Normal && cfg.graph[target.index()].stmts.is_empty())
                .then_some(target)
        })
        .expect("raise must have a successful-raise block")
}

fn masked_projection(
    original: &[u8],
    source_id: &str,
    masked_ranges: &[Range<usize>],
) -> (Vec<u8>, Vec<SourceMapSegment>) {
    let mut prepared = original.to_vec();
    for range in masked_ranges {
        for byte in &mut prepared[range.clone()] {
            if !matches!(*byte, b'\r' | b'\n') {
                *byte = b' ';
            }
        }
    }

    let mut segments = Vec::new();
    let mut cursor = 0;
    for range in masked_ranges {
        if cursor < range.start {
            segments.push(copied(
                cursor..range.start,
                source_id,
                cursor..range.start,
                "root",
            ));
        }
        segments.push(masked(range.clone(), source_id, range.clone(), "root"));
        cursor = range.end;
    }
    if cursor < original.len() {
        segments.push(copied(
            cursor..original.len(),
            source_id,
            cursor..original.len(),
            "root",
        ));
    }
    (prepared, segments)
}

#[test]
fn source_map_preserves_repeated_nested_include_occurrences_and_cross_file_ranges() {
    let map = SourceMap::new(
        b"main;one;two;tail",
        vec![
            source("main.pas", b"main;tail"),
            source("inc.pas", b"one;two;"),
        ],
        vec![
            copied(0..5, "main.pas", 0..5, "root"),
            copied(5..9, "inc.pas", 0..4, "include-1"),
            copied(9..13, "inc.pas", 4..8, "include-1/nested"),
            copied(13..17, "main.pas", 5..9, "root"),
        ],
    )
    .expect("valid repeated include map");

    let statement = StmtRef {
        byte_range: 3..15,
        node_kind: "statement".into(),
    };
    let mapped = map
        .map_range(statement.byte_range.clone())
        .expect("range should be mappable");
    assert_eq!(mapped.len(), 4, "cross-file ranges retain every segment");
    assert_eq!(mapped[0].prepared_range, 3..5);
    assert_eq!(mapped[0].original.as_ref().unwrap().byte_range, 3..5);
    assert_eq!(mapped[0].expansion_id.as_str(), "root");
    assert_eq!(
        mapped[1].original.as_ref().unwrap().source_id.as_str(),
        "inc.pas"
    );
    assert_eq!(mapped[1].original.as_ref().unwrap().byte_range, 0..4);
    assert_eq!(mapped[1].expansion_id.as_str(), "include-1");
    assert_eq!(mapped[2].original.as_ref().unwrap().byte_range, 4..8);
    assert_eq!(mapped[2].expansion_id.as_str(), "include-1/nested");
    assert_eq!(mapped[3].prepared_range, 13..15);
    assert_eq!(mapped[3].original.as_ref().unwrap().byte_range, 5..7);
    assert_eq!(mapped[3].expansion_id.as_str(), "root");

    let repeated = SourceMap::new(
        b"one;one;",
        vec![source("inc.pas", b"one;")],
        vec![
            copied(0..4, "inc.pas", 0..4, "occurrence-1"),
            copied(4..8, "inc.pas", 0..4, "occurrence-2"),
        ],
    )
    .expect("repeated source ranges are valid when occurrence IDs differ");
    let repeated_spans = repeated.map_range(0..8).unwrap();
    assert_eq!(
        repeated_spans[0].expansion_id,
        ExpansionId::new("occurrence-1")
    );
    assert_eq!(
        repeated_spans[1].expansion_id,
        ExpansionId::new("occurrence-2")
    );
    assert_eq!(repeated_spans[0].original, repeated_spans[1].original);
}

#[test]
fn source_map_rejects_reused_origin_ranges_and_mixed_expansion_sources() {
    let same_occurrence = SourceMap::new(
        b"one;one;",
        vec![source("inc.pas", b"one;")],
        vec![
            copied(0..4, "inc.pas", 0..4, "same-occurrence"),
            copied(4..8, "inc.pas", 0..4, "same-occurrence"),
        ],
    );
    let error = same_occurrence.expect_err("one occurrence must not reuse an origin range");
    assert!(matches!(
        error,
        SourceMapError::OverlappingOriginalRanges { .. }
    ));

    let partial_overlap = SourceMap::new(
        b"abbc",
        vec![source("inc.pas", b"abc")],
        vec![
            copied(0..2, "inc.pas", 0..2, "same-occurrence"),
            copied(2..4, "inc.pas", 1..3, "same-occurrence"),
        ],
    );
    let error = partial_overlap.expect_err("partially overlapping origins must be rejected");
    assert!(matches!(
        error,
        SourceMapError::OverlappingOriginalRanges { .. }
    ));

    let containing_overlap = SourceMap::new(
        b"abcb",
        vec![source("inc.pas", b"abc")],
        vec![
            copied(0..3, "inc.pas", 0..3, "same-occurrence"),
            copied(3..4, "inc.pas", 1..2, "same-occurrence"),
        ],
    );
    let error = containing_overlap.expect_err("contained origins must be rejected");
    assert!(matches!(
        error,
        SourceMapError::OverlappingOriginalRanges { .. }
    ));

    let masked_overlap = SourceMap::new(
        b"  ",
        vec![source("inc.pas", b"ab")],
        vec![
            masked(0..1, "inc.pas", 0..1, "same-occurrence"),
            masked(1..2, "inc.pas", 0..1, "same-occurrence"),
        ],
    );
    let error = masked_overlap.expect_err("masked origins must not overlap either");
    assert!(matches!(
        error,
        SourceMapError::OverlappingOriginalRanges { .. }
    ));

    let mixed_sources = SourceMap::new(
        b"one;two;",
        vec![source("inc.pas", b"one;"), source("other.pas", b"two;")],
        vec![
            copied(0..4, "inc.pas", 0..4, "same-occurrence"),
            copied(4..8, "other.pas", 0..4, "same-occurrence"),
        ],
    );
    let error = mixed_sources.expect_err("one occurrence must not span source files");
    assert!(matches!(
        error,
        SourceMapError::ExpansionSourceMismatch { .. }
    ));
}

#[test]
fn source_map_accepts_fine_grained_and_reversed_disjoint_original_ranges() {
    let segment_count = 160_000;
    let bytes = vec![b'x'; segment_count];
    let segments = (0..segment_count)
        .map(|offset| copied(offset..offset + 1, "fine.pas", offset..offset + 1, "root"))
        .collect();
    let map = SourceMap::new(&bytes, vec![source("fine.pas", &bytes)], segments)
        .expect("fine-grained disjoint mappings are valid");
    assert_eq!(map.segments().len(), segment_count);

    let reversed = SourceMap::new(
        b"ba",
        vec![source("reversed.pas", b"ab")],
        vec![
            copied(0..1, "reversed.pas", 1..2, "root"),
            copied(1..2, "reversed.pas", 0..1, "root"),
        ],
    )
    .expect("disjoint original ranges remain valid in source-order reversal");
    assert_eq!(
        reversed.map_range(0..2).unwrap()[0].original,
        Some(SourceSpan::new(ProjectSourceId::new("reversed.pas"), 1..2))
    );
}

#[test]
fn source_map_identity_and_empty_ranges_are_safe() {
    let snapshot = source("unit.pas", b"abc\n");
    let map = SourceMap::identity(snapshot.clone()).expect("identity map");
    assert_eq!(map.prepared_len(), 4);
    assert_eq!(map.original_sources(), &[snapshot]);
    assert_eq!(map.segments().len(), 1);
    assert_eq!(map.map_range(1..1).unwrap(), Vec::new());
    assert_eq!(
        map.map_range(0..4).unwrap()[0].kind,
        SourceSegmentKind::Copied
    );

    let empty = SourceMap::identity(source("empty.pas", b"")).expect("empty identity map");
    assert_eq!(empty.prepared_len(), 0);
    assert!(empty.segments().is_empty());
    assert!(empty.map_range(0..0).unwrap().is_empty());
}

#[test]
fn source_map_rejects_gaps_overlaps_bad_origins_and_byte_mismatches() {
    let original = source("one.pas", b"abcdef");

    assert!(matches!(
        SourceMap::new(
            b"abcdef",
            vec![original.clone()],
            vec![
                copied(0..2, "one.pas", 0..2, "root"),
                copied(3..6, "one.pas", 3..6, "root"),
            ]
        ),
        Err(SourceMapError::IncompleteCoverage { .. })
    ));
    assert!(matches!(
        SourceMap::new(
            b"abcdef",
            vec![original.clone()],
            vec![
                copied(0..4, "one.pas", 0..4, "root"),
                copied(3..6, "one.pas", 3..6, "root"),
            ]
        ),
        Err(SourceMapError::OverlappingSegments { .. })
    ));
    assert!(matches!(
        SourceMap::new(
            b"abc",
            vec![original.clone()],
            vec![copied(0..3, "one.pas", 0..2, "root")]
        ),
        Err(SourceMapError::LengthMismatch { .. })
    ));
    assert!(matches!(
        SourceMap::new(
            b"abq",
            vec![original.clone()],
            vec![copied(0..3, "one.pas", 0..3, "root")]
        ),
        Err(SourceMapError::CopiedBytesMismatch { .. })
    ));
    assert!(matches!(
        SourceMap::new(
            b"abc",
            vec![original.clone()],
            vec![copied(0..3, "missing.pas", 0..3, "root")]
        ),
        Err(SourceMapError::UnknownSourceId(_))
    ));
    assert!(matches!(
        SourceMap::new(
            b"abcdef",
            vec![original.clone(), source("one.pas", b"abcdef")],
            vec![copied(0..6, "one.pas", 0..6, "root")]
        ),
        Err(SourceMapError::DuplicateSourceId(_))
    ));
    assert!(matches!(
        SourceMap::new(
            b"abcdef",
            vec![original.clone(), source("one.pas", b"different")],
            vec![copied(0..6, "one.pas", 0..6, "root")]
        ),
        Err(SourceMapError::ConflictingSourceId(_))
    ));
    assert!(matches!(
        SourceMap::new(
            b"abc",
            vec![original.clone()],
            vec![copied(0..3, "one.pas", 4..8, "root")]
        ),
        Err(SourceMapError::OriginalRangeOutOfBounds { .. })
    ));
    let missing_origin = SourceMapSegment {
        prepared_range: 0..3,
        original: None,
        expansion_id: ExpansionId::new("root"),
        kind: SourceSegmentKind::Copied,
    };
    assert!(matches!(
        SourceMap::new(b"abc", vec![original.clone()], vec![missing_origin]),
        Err(SourceMapError::MissingOrigin { .. })
    ));
    assert!(matches!(
        SourceMap::new(
            b"abc",
            vec![original.clone()],
            vec![copied(0..3, "one.pas", 0..3, "")]
        ),
        Err(SourceMapError::EmptyExpansionId)
    ));

    let synthetic_with_origin = SourceMapSegment {
        prepared_range: 0..1,
        original: Some(cfg_pascal::SourceSpan::new(
            ProjectSourceId::new("one.pas"),
            0..1,
        )),
        expansion_id: ExpansionId::new("generated"),
        kind: SourceSegmentKind::Synthetic,
    };
    assert!(matches!(
        SourceMap::new(b"x", vec![original], vec![synthetic_with_origin]),
        Err(SourceMapError::SyntheticHasOrigin { .. })
    ));
    let synthetic = SourceMap::new(
        b"generated",
        Vec::new(),
        vec![SourceMapSegment::synthetic(
            0..9,
            ExpansionId::new("generated"),
        )],
    )
    .expect("synthetic bytes have an explicit absent origin");
    let synthetic_span = &synthetic.map_range(0..9).unwrap()[0];
    assert_eq!(synthetic_span.kind, SourceSegmentKind::Synthetic);
    assert!(synthetic_span.original.is_none());
    assert!(matches!(
        synthetic.map_range(0..10),
        Err(SourceMapError::MapRangeOutOfBounds { .. })
    ));
    assert!(matches!(
        synthetic.map_range(Range { start: 2, end: 1 }),
        Err(SourceMapError::InvalidRange { .. })
    ));
}

#[test]
fn source_map_allows_only_length_preserving_whitespace_masks() {
    let original = source("one.pas", b"abc\n");
    let valid = SourceMap::new(
        b"   \n",
        vec![original.clone()],
        vec![masked(0..4, "one.pas", 0..4, "root")],
    )
    .expect("whitespace mask");
    assert_eq!(valid.segments()[0].kind, SourceSegmentKind::Masked);

    assert!(matches!(
        SourceMap::new(
            b"  x\n",
            vec![original.clone()],
            vec![masked(0..4, "one.pas", 0..4, "root")]
        ),
        Err(SourceMapError::MaskedBytesNotWhitespace { .. })
    ));
    assert!(matches!(
        SourceMap::new(
            b"    ",
            vec![original],
            vec![masked(0..4, "one.pas", 0..4, "root")]
        ),
        Err(SourceMapError::MaskedLineBreakMismatch { .. })
    ));
}

#[test]
fn prepared_source_requires_explicit_complete_clean_input_and_retains_provenance() {
    let bytes = b"unit Demo; interface implementation end.";
    let snapshot = source("demo.pas", bytes);
    let map = SourceMap::identity(snapshot.clone()).unwrap();
    let prepared = PreparedSource::new(
        ProjectSourceId::new("demo.prepared"),
        bytes,
        map.clone(),
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .expect("clean complete preparation");
    assert_eq!(prepared.bytes(), bytes);
    assert_eq!(prepared.source_id().as_str(), "demo.prepared");
    assert_eq!(prepared.configuration_id(), "debug");
    assert_eq!(prepared.fidelity(), PreparationFidelity::Complete);
    assert_eq!(prepared.provenance(), PreparationProvenance::Configured);
    assert_eq!(prepared.original_sources(), &[snapshot]);
    assert_eq!(prepared.source_map(), &map);
    assert!(!prepared.tree().root_node().has_error());

    for fidelity in [
        PreparationFidelity::Unresolved,
        PreparationFidelity::Lossy,
        PreparationFidelity::Incomplete,
    ] {
        let result = PreparedSource::new(
            ProjectSourceId::new("demo.prepared"),
            bytes,
            map.clone(),
            "debug",
            fidelity,
            PreparationProvenance::Configured,
        );
        assert!(matches!(
            result,
            Err(cfg_pascal::PreparedSourceError::RejectedFidelity(_))
        ));
    }

    let broken = b"unit Demo; interface uses;";
    let broken_map = SourceMap::identity(source("broken.pas", broken)).unwrap();
    assert!(matches!(
        PreparedSource::new(
            ProjectSourceId::new("broken.prepared"),
            broken,
            broken_map,
            "debug",
            PreparationFidelity::Complete,
            PreparationProvenance::Configured,
        ),
        Err(cfg_pascal::PreparedSourceError::ParserErrors { .. })
    ));
}

#[test]
fn identity_rejects_unresolved_preprocessor_content_instead_of_claiming_complete() {
    let source = b"program Demo; begin\n{$I missing.inc}\nend.";
    let result = PreparedSource::identity(ProjectSourceId::new("demo.pas"), source, "debug");
    let error = result.expect_err("an unresolved include must not become complete");
    assert!(matches!(
        &error,
        cfg_pascal::PreparedSourceError::UnresolvedPreprocessor { .. }
    ));
    assert!(error.to_string().contains("strict prepared source"));

    let raw_tree = parse_clean(source);
    assert!(!build_file_cfgs(&raw_tree, source).is_empty());

    let clean_identity = PreparedSource::identity(
        ProjectSourceId::new("clean.pas"),
        b"program Clean; begin end.",
        "debug",
    )
    .expect("a parse-clean source without preprocessor nodes is a valid identity");
    assert_eq!(clean_identity.provenance(), PreparationProvenance::Raw);

    let comment_style = b"program Demo; begin\n(*$I missing.inc*)\nend.";
    let error = PreparedSource::identity(
        ProjectSourceId::new("comment-style.pas"),
        comment_style,
        "debug",
    )
    .expect_err("comment-style include directives must not become complete");
    assert!(matches!(
        error,
        cfg_pascal::PreparedSourceError::UnresolvedPreprocessor { .. }
    ));

    let ordinary_comment = PreparedSource::identity(
        ProjectSourceId::new("ordinary-comment.pas"),
        b"program Comment; begin (* ordinary comment *) end.",
        "debug",
    )
    .expect("ordinary comments are not preprocessor directives");
    assert_eq!(ordinary_comment.provenance(), PreparationProvenance::Raw);

    let directive_like_string = PreparedSource::identity(
        ProjectSourceId::new("directive-string.pas"),
        b"program StringLiteral; begin WriteLn('(*$I missing.inc*)'); end.",
        "debug",
    )
    .expect("directive-like text inside strings is not a preprocessor directive");
    assert_eq!(
        directive_like_string.provenance(),
        PreparationProvenance::Raw
    );
}

#[test]
fn every_strict_prepared_constructor_rejects_preprocessor_nodes_but_not_literals_or_comments() {
    let unresolved = [
        (
            "brace-include.pas",
            b"program Brace; begin {$I body.inc} end.".as_slice(),
        ),
        (
            "paren-include.pas",
            b"program Paren; begin (*$I body.inc*) end.".as_slice(),
        ),
        (
            "nested-include.pas",
            b"program Nested; {$IF TRUE} (*$I body.inc*) {$ENDIF} begin end.".as_slice(),
        ),
    ];

    for (source_id, bytes) in unresolved {
        let snapshot = source(source_id, bytes);
        let map = SourceMap::identity(snapshot.clone()).expect("identity source map");
        let new_result = PreparedSource::new(
            ProjectSourceId::new(format!("{source_id}.prepared")),
            bytes,
            map,
            "debug",
            PreparationFidelity::Complete,
            PreparationProvenance::Configured,
        );
        assert!(
            matches!(
                new_result,
                Err(cfg_pascal::PreparedSourceError::UnresolvedPreprocessor { .. })
            ),
            "new accepted unresolved preprocessor content from {source_id}"
        );

        let from_segments_result = PreparedSource::from_segments(
            ProjectSourceId::new(format!("{source_id}.prepared")),
            bytes,
            vec![snapshot],
            vec![copied(0..bytes.len(), source_id, 0..bytes.len(), "root")],
            "debug",
            PreparationFidelity::Complete,
            PreparationProvenance::Configured,
        );
        assert!(
            matches!(
                from_segments_result,
                Err(cfg_pascal::PreparedSourceError::UnresolvedPreprocessor { .. })
            ),
            "from_segments accepted unresolved preprocessor content from {source_id}"
        );
    }

    let clean = b"program Clean; begin WriteLn('{$I body.inc}'); (* ordinary comment *) end.";
    let snapshot = source("clean.pas", clean);
    let map = SourceMap::identity(snapshot.clone()).expect("clean identity source map");
    PreparedSource::new(
        ProjectSourceId::new("clean.prepared"),
        clean,
        map,
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .expect("directive-like strings and ordinary comments are not preprocessor nodes");
    PreparedSource::from_segments(
        ProjectSourceId::new("clean.prepared"),
        clean,
        vec![snapshot],
        vec![copied(0..clean.len(), "clean.pas", 0..clean.len(), "root")],
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .expect("from_segments preserves the same clean-input policy");
}

#[test]
fn prepared_units_keep_maps_and_project_rejects_incompatible_metadata() {
    let bytes = b"unit A; interface implementation end.";
    let map = SourceMap::identity(source("a.pas", bytes)).unwrap();
    let prepared = PreparedSource::new(
        ProjectSourceId::new("a.prepared"),
        bytes,
        map,
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .unwrap();
    let unit = ProjectUnitInput::from_prepared(ProjectUnitId::new("a"), prepared);
    assert_eq!(unit.source_map().unwrap().prepared_len(), bytes.len());
    assert_eq!(unit.configuration_id(), Some("debug"));
    assert_eq!(
        unit.preparation_fidelity(),
        Some(PreparationFidelity::Complete)
    );
    assert_eq!(
        unit.preparation_provenance(),
        Some(PreparationProvenance::Configured)
    );

    let other_bytes = b"unit B; interface implementation end.";
    let other_map = SourceMap::identity(source("b.pas", other_bytes)).unwrap();
    let other = PreparedSource::new(
        ProjectSourceId::new("b.prepared"),
        other_bytes,
        other_map,
        "release",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .unwrap();
    let result = ProjectSnapshot::new(
        vec![
            unit,
            ProjectUnitInput::from_prepared(ProjectUnitId::new("b"), other),
        ],
        Vec::new(),
    );
    assert!(matches!(
        result,
        Err(ProjectSnapshotError::IncompatibleConfigurationIds { .. })
    ));
}

#[test]
fn project_rejects_conflicting_shared_original_source_snapshots() {
    let a_bytes = b"unit A; interface implementation end.";
    let b_bytes = b"unit B; interface implementation end.";

    let a = PreparedSource::new(
        ProjectSourceId::new("a.prepared"),
        a_bytes,
        SourceMap::identity(source("shared.pas", a_bytes)).unwrap(),
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .unwrap();
    let b = PreparedSource::new(
        ProjectSourceId::new("b.prepared"),
        b_bytes,
        SourceMap::identity(source("shared.pas", b_bytes)).unwrap(),
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .unwrap();

    let result = ProjectSnapshot::new(
        vec![
            ProjectUnitInput::from_prepared(ProjectUnitId::new("a"), a),
            ProjectUnitInput::from_prepared(ProjectUnitId::new("b"), b),
        ],
        Vec::new(),
    );
    assert!(matches!(
        result,
        Err(ProjectSnapshotError::ConflictingOriginalSource { .. })
    ));
}

#[test]
fn configured_conditional_projection_restores_precision_without_changing_cfg_coordinates() {
    let definition_source = br#"unit Definitions;
interface
{$IFDEF FEATURE}
type
  TError = class constructor Create; end;
{$ENDIF}
implementation
end.
"#;
    let ifdef_start = definition_source
        .windows(b"{$IFDEF FEATURE}".len())
        .position(|window| window == b"{$IFDEF FEATURE}")
        .unwrap();
    let ifdef_end = ifdef_start + b"{$IFDEF FEATURE}".len();
    let endif_start = definition_source
        .windows(b"{$ENDIF}".len())
        .position(|window| window == b"{$ENDIF}")
        .unwrap();
    let endif_end = endif_start + b"{$ENDIF}".len();
    let (prepared_bytes, segments) = masked_projection(
        definition_source,
        "definitions.pas",
        &[ifdef_start..ifdef_end, endif_start..endif_end],
    );
    let prepared = PreparedSource::new(
        ProjectSourceId::new("definitions.prepared"),
        &prepared_bytes,
        SourceMap::new(
            &prepared_bytes,
            vec![source("definitions.pas", definition_source)],
            segments,
        )
        .unwrap(),
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .expect("masked directives produce a clean explicit projection");
    let prepared_unit =
        ProjectUnitInput::from_prepared(ProjectUnitId::new("definitions"), prepared);

    let raw_tree = {
        let mut parser = Parser::new();
        parser.set_language(&cfg_pascal::LANGUAGE.into()).unwrap();
        parser.parse(definition_source, None).unwrap()
    };
    assert!(!raw_tree.root_node().has_error());
    let raw_unit = ProjectUnitInput::new(
        ProjectUnitId::new("definitions"),
        ProjectSourceId::new("definitions.raw"),
        raw_tree,
        definition_source,
    );

    let consumer_source = br#"unit Consumer;
interface
uses Definitions;
implementation
procedure P;
begin
  try
    raise Definitions.TError.Create;
  except
    on Definitions.TError do Handle;
  end;
end;
end.
"#;
    let consumer_tree = parse_clean(consumer_source);
    let import = imported_unit("consumer", &consumer_tree, "definitions");
    let consumer = ProjectUnitInput::new(
        ProjectUnitId::new("consumer"),
        ProjectSourceId::new("consumer.pas"),
        consumer_tree.clone(),
        consumer_source,
    );

    let prepared_snapshot =
        ProjectSnapshot::new(vec![consumer.clone(), prepared_unit], vec![import.clone()]).unwrap();
    let prepared_cfgs =
        build_file_cfgs_in_project(&prepared_snapshot, &ProjectUnitId::new("consumer")).unwrap();
    let prepared_cfg = cfg_for(&prepared_cfgs, "P");
    let prepared_raise = block_with_text(
        prepared_cfg,
        consumer_source,
        "raise",
        "raise Definitions.TError.Create",
    );
    let prepared_success = successful_raise_block(prepared_cfg, prepared_raise);
    let prepared_handler = block_with_text(prepared_cfg, consumer_source, "statement", "Handle");
    assert_eq!(
        successors(prepared_cfg, prepared_success),
        vec![(prepared_handler, EdgeKind::ExceptionThrow)]
    );
    assert_eq!(
        prepared_cfg
            .graph
            .node_indices()
            .map(BlockId::from)
            .filter_map(|block| {
                prepared_cfg.graph[block.index()]
                    .stmts
                    .iter()
                    .find(|stmt| stmt.node_kind == "raise")
                    .map(|stmt| stmt.byte_range.clone())
            })
            .collect::<Vec<_>>(),
        vec![prepared_cfg.graph[prepared_raise.index()].stmts[0]
            .byte_range
            .clone()]
    );

    let raw_snapshot = ProjectSnapshot::new(vec![consumer, raw_unit], vec![import]).unwrap();
    let raw_cfgs =
        build_file_cfgs_in_project(&raw_snapshot, &ProjectUnitId::new("consumer")).unwrap();
    let raw_cfg = cfg_for(&raw_cfgs, "P");
    let raw_raise = block_with_text(
        raw_cfg,
        consumer_source,
        "raise",
        "raise Definitions.TError.Create",
    );
    let raw_success = successful_raise_block(raw_cfg, raw_raise);
    let raw_handler = block_with_text(raw_cfg, consumer_source, "statement", "Handle");
    assert!(successors(raw_cfg, raw_success).contains(&(raw_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(raw_cfg, raw_success).contains(&(raw_cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn prepared_source_uses_the_crate_parser_language() {
    let bytes = Arc::<[u8]>::from(b"unit Demo; interface implementation end.".as_slice());
    let snapshot = SourceSnapshot::new(ProjectSourceId::new("demo.pas"), bytes.clone());
    let map = SourceMap::identity(snapshot).unwrap();
    let prepared = PreparedSource::new(
        ProjectSourceId::new("demo.prepared"),
        bytes,
        map,
        "debug",
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .unwrap();
    assert_eq!(
        prepared.bytes(),
        b"unit Demo; interface implementation end."
    );
}
