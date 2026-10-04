use std::process::Command;

use cfg_core::{BlockId, Cfg, EdgeKind};
use cfg_pascal::{
    build_file_cfgs_in_project, prepare_source, ImportBinding, ImportTarget, IncludeBinding,
    PreparationEnvironment, PrepareSourceOptions, PreparedSource, ProjectSnapshot, ProjectSourceId,
    ProjectUnitId, ProjectUnitInput, SourceSegmentKind, SourceSnapshot, UsesSite,
};

fn source(id: &str, bytes: &[u8]) -> SourceSnapshot {
    SourceSnapshot::new(ProjectSourceId::new(id), bytes)
}

fn parse_clean(source: &[u8]) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&cfg_pascal::LANGUAGE.into()).unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    tree
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

#[test]
fn prepared_conditional_exports_restore_project_precision_while_raw_stays_conservative() {
    let definitions = br#"unit Definitions;
interface
{$IFDEF FEATURE}
type
  TError = class
    constructor Create;
  end;
{$ENDIF}
implementation
end.
"#;
    let definitions_id = ProjectSourceId::new("definitions.pas");
    let prepared_definitions = prepare_source(
        &definitions_id,
        &[source("definitions.pas", definitions)],
        &[],
        PrepareSourceOptions::new(
            ProjectSourceId::new("definitions.prepared"),
            "debug",
            PreparationEnvironment::Complete,
        )
        .with_initial_defined_symbols(["FEATURE"]),
    )
    .expect("configured definitions preparation");

    let consumer = br#"unit Consumer;
interface
uses Definitions;
implementation
procedure P;
begin
  try
    raise Definitions.TError.Create;
  except
    on Definitions.TError do
      Handle;
  end;
end;
end.
"#;
    let consumer_id = ProjectUnitId::new("consumer");
    let consumer_source_id = ProjectSourceId::new("consumer.pas");
    let consumer_tree = parse_clean(consumer);
    let uses_start = consumer
        .windows(b"Definitions".len())
        .position(|window| window == b"Definitions")
        .expect("uses module name");
    let import = ImportBinding::new(
        UsesSite::new(
            consumer_id.clone(),
            uses_start..uses_start + b"Definitions".len(),
        ),
        ImportTarget::Loaded(ProjectUnitId::new("definitions")),
        ["Definitions"],
    );
    let consumer_input = ProjectUnitInput::new(
        consumer_id.clone(),
        consumer_source_id,
        consumer_tree.clone(),
        consumer,
    );

    let prepared_snapshot = ProjectSnapshot::new(
        vec![
            consumer_input.clone(),
            ProjectUnitInput::from_prepared(
                ProjectUnitId::new("definitions"),
                prepared_definitions,
            ),
        ],
        vec![import.clone()],
    )
    .expect("prepared project snapshot");
    let prepared_cfgs = build_file_cfgs_in_project(&prepared_snapshot, &consumer_id).unwrap();
    let prepared_cfg = cfg_for(&prepared_cfgs, "P");
    let prepared_raise = block_with_text(
        prepared_cfg,
        consumer,
        "raise",
        "raise Definitions.TError.Create",
    );
    let prepared_success = successful_raise_block(prepared_cfg, prepared_raise);
    let prepared_handler = block_with_text(prepared_cfg, consumer, "statement", "Handle");
    assert_eq!(
        successors(prepared_cfg, prepared_success),
        vec![(prepared_handler, EdgeKind::ExceptionThrow)]
    );

    let raw_definitions_tree = parse_clean(definitions);
    let raw_snapshot = ProjectSnapshot::new(
        vec![
            consumer_input,
            ProjectUnitInput::new(
                ProjectUnitId::new("definitions"),
                definitions_id,
                raw_definitions_tree,
                definitions,
            ),
        ],
        vec![import],
    )
    .expect("raw project snapshot");
    let raw_cfgs = build_file_cfgs_in_project(&raw_snapshot, &consumer_id).unwrap();
    let raw_cfg = cfg_for(&raw_cfgs, "P");
    let raw_raise = block_with_text(
        raw_cfg,
        consumer,
        "raise",
        "raise Definitions.TError.Create",
    );
    let raw_success = successful_raise_block(raw_cfg, raw_raise);
    let raw_handler = block_with_text(raw_cfg, consumer, "statement", "Handle");
    assert!(successors(raw_cfg, raw_success).contains(&(raw_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(raw_cfg, raw_success).contains(&(raw_cfg.exit, EdgeKind::ExceptionThrow)));
}

fn directive_range(bytes: &[u8], directive: &[u8]) -> std::ops::Range<usize> {
    let start = bytes
        .windows(directive.len())
        .position(|window| window == directive)
        .expect("directive is present");
    start..start + directive.len()
}

#[test]
fn selected_include_expands_in_place_and_maps_back_to_the_target_snapshot() {
    let root = br#"program Demo;
begin
  RootBefore;
{$I statements.inc}
  RootAfter;
end.
"#;
    let included = b"  Included;\n";
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("statements.inc");
    let include = b"{$I statements.inc}";
    let binding = IncludeBinding::new(
        root_id.clone(),
        directive_range(root, include),
        included_id.clone(),
    );

    let prepared = prepare_source(
        &root_id,
        &[
            source(root_id.as_str(), root),
            source(included_id.as_str(), included),
        ],
        &[binding],
        options(),
    )
    .expect("selected include expansion");

    let included_start = prepared
        .bytes()
        .windows(b"Included".len())
        .position(|window| window == b"Included")
        .expect("included statement is present");
    let mapped = prepared
        .map_range(included_start..included_start + b"Included".len())
        .expect("included statement maps");
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].original.as_ref().unwrap().source_id, included_id);
    assert_ne!(mapped[0].expansion_id, cfg_pascal::ExpansionId::root());
    assert_eq!(mapped[0].original.as_ref().unwrap().byte_range, 2..10);
}

#[test]
fn include_eof_line_comment_gets_a_synthetic_separator_before_parent_text() {
    let root = b"program Demo; begin {$I body.inc} Writeln('keep'); end.";
    let included = b"// included comment";
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("body.inc");
    let include = b"{$I body.inc}";
    let prepared = prepare_source(
        &root_id,
        &[
            source(root_id.as_str(), root),
            source(included_id.as_str(), included),
        ],
        &[IncludeBinding::new(
            root_id.clone(),
            directive_range(root, include),
            included_id.clone(),
        )],
        options(),
    )
    .expect("EOF line-comment include should preserve parent text");

    let comment_end = prepared
        .bytes()
        .windows(included.len())
        .position(|window| window == included)
        .expect("included comment")
        + included.len();
    assert_eq!(prepared.bytes().get(comment_end), Some(&b'\n'));
    let separator = prepared
        .map_range(comment_end..comment_end + 1)
        .expect("synthetic include separator");
    assert_eq!(separator.len(), 1);
    assert_eq!(separator[0].kind, SourceSegmentKind::Synthetic);
    assert!(separator[0].original.is_none());

    let keep_start = prepared
        .bytes()
        .windows(b"Writeln('keep');".len())
        .position(|window| window == b"Writeln('keep');")
        .expect("parent text after include");
    assert!(keep_start > comment_end);
    assert!(prepared.tree().root_node().to_sexp().contains("exprCall"));
}

#[test]
fn include_eof_line_comment_separator_handles_cr_lf_crlf_and_no_ending() {
    let root = b"program Demo; begin {$I body.inc} Writeln('keep'); end.";
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("body.inc");
    let include = b"{$I body.inc}";
    let cases: &[(&str, &[u8], bool)] = &[
        ("CR-only", b"// included comment\r", true),
        ("LF", b"// included comment\n", false),
        ("CRLF", b"// included comment\r\n", false),
        ("no ending", b"// included comment", true),
    ];

    for (label, included, expects_synthetic_lf) in cases {
        let prepared = prepare_source(
            &root_id,
            &[
                source(root_id.as_str(), root),
                source(included_id.as_str(), included),
            ],
            &[IncludeBinding::new(
                root_id.clone(),
                directive_range(root, include),
                included_id.clone(),
            )],
            options(),
        )
        .unwrap_or_else(|error| panic!("{label} include should prepare: {error:?}"));

        let comment_end = prepared
            .bytes()
            .windows(included.len())
            .position(|window| window == *included)
            .unwrap_or_else(|| panic!("{label} included comment"))
            + included.len();
        if *expects_synthetic_lf {
            assert_eq!(
                prepared.bytes().get(comment_end),
                Some(&b'\n'),
                "{label} include needs a synthetic LF"
            );
            let separator = prepared
                .map_range(comment_end..comment_end + 1)
                .unwrap_or_else(|error| panic!("{label} synthetic include separator: {error:?}"));
            assert_eq!(separator.len(), 1);
            assert_eq!(separator[0].kind, SourceSegmentKind::Synthetic);
            assert!(separator[0].original.is_none());
        }
        assert!(
            prepared.tree().root_node().to_sexp().contains("exprCall"),
            "{label} parent statement must remain parse-visible"
        );
    }
}

#[test]
fn include_eof_keyword_gets_a_separator_before_parent_expression() {
    let root = b"program Demo; begin {$I body.inc}E.Create; end.";
    let included = b"raise";
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("body.inc");
    let include = b"{$I body.inc}";
    let prepared = prepare_source(
        &root_id,
        &[
            source(root_id.as_str(), root),
            source(included_id.as_str(), included),
        ],
        &[IncludeBinding::new(
            root_id.clone(),
            directive_range(root, include),
            included_id.clone(),
        )],
        options(),
    )
    .expect("EOF keyword include should preserve the parent token boundary");

    let raise_start = prepared
        .bytes()
        .windows(included.len())
        .position(|window| window == included)
        .expect("included keyword");
    assert!(prepared
        .bytes()
        .get(raise_start + included.len())
        .is_some_and(u8::is_ascii_whitespace));
    assert!(prepared.tree().root_node().to_sexp().contains("kRaise"));
}

#[test]
fn include_eof_separator_preserves_downstream_missing_import_uncertainty() {
    assert_downstream_missing_import_uncertainty(b"//comment");
}

#[test]
fn include_eof_cr_only_separator_preserves_downstream_missing_import_uncertainty() {
    assert_downstream_missing_import_uncertainty(b"//comment\r");
}

fn assert_downstream_missing_import_uncertainty(included: &[u8]) {
    let known =
        b"unit Known; interface type TError = class constructor Create; end; implementation end.";
    let root = br#"unit D; interface uses Known; implementation
{$I x}uses Missing;
type TAlias = TError;
procedure P; begin try raise TAlias.Create; except on Known.TError do Handle; end; end; end."#;
    let root_id = ProjectSourceId::new("root");
    let included_id = ProjectSourceId::new("inc");
    let root_unit_id = ProjectUnitId::new("d");
    let include = b"{$I x}";
    let prepared = prepare_source(
        &root_id,
        &[
            source(root_id.as_str(), root),
            source(included_id.as_str(), included),
        ],
        &[IncludeBinding::new(
            root_id.clone(),
            directive_range(root, include),
            included_id,
        )],
        options(),
    )
    .expect("include separator should preserve the following uses clause");

    let known_start = prepared
        .bytes()
        .windows(b"Known".len())
        .position(|window| window == b"Known")
        .expect("Known import");
    let missing_start = prepared
        .bytes()
        .windows(b"Missing".len())
        .position(|window| window == b"Missing")
        .expect("Missing import");
    let known_prepared =
        PreparedSource::identity(ProjectSourceId::new("known-source"), known, "debug")
            .expect("known source");
    let snapshot = ProjectSnapshot::new(
        vec![
            ProjectUnitInput::from_prepared(ProjectUnitId::new("known"), known_prepared),
            ProjectUnitInput::from_prepared(root_unit_id.clone(), prepared),
        ],
        vec![
            ImportBinding::new(
                UsesSite::new(
                    root_unit_id.clone(),
                    known_start..known_start + b"Known".len(),
                ),
                ImportTarget::Loaded(ProjectUnitId::new("known")),
                ["Known"],
            ),
            ImportBinding::new(
                UsesSite::new(
                    root_unit_id.clone(),
                    missing_start..missing_start + b"Missing".len(),
                ),
                ImportTarget::Unavailable,
                ["Missing"],
            ),
        ],
    )
    .expect("the resumed uses clause should be a real project import");

    let cfgs = build_file_cfgs_in_project(&snapshot, &root_unit_id).expect("project CFGs");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_text(
        cfg,
        snapshot.unit(&root_unit_id).unwrap().source(),
        "raise",
        "raise TAlias.Create",
    );
    let successful_raise = successful_raise_block(cfg, raise);
    let handler = block_with_text(
        cfg,
        snapshot.unit(&root_unit_id).unwrap().source(),
        "statement",
        "Handle",
    );
    assert!(successors(cfg, successful_raise).contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful_raise).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn include_define_state_flows_from_root_into_include_and_back_to_the_root() {
    let root = br#"program Demo;
{$DEFINE FROM_ROOT}
begin
{$I body.inc}
{$IFDEF FROM_INCLUDE}
  AfterInclude;
{$ENDIF}
{$IFDEF REMOVED_BY_INCLUDE}
  MustNotAppear;
{$ELSE}
  UndefWasApplied;
{$ENDIF}
end.
"#;
    let included = br#"{$IFDEF FROM_ROOT}
  SawRootDefine;
{$ENDIF}
{$DEFINE FROM_INCLUDE}
{$DEFINE REMOVED_BY_INCLUDE}
{$UNDEF REMOVED_BY_INCLUDE}
"#;
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("body.inc");
    let include = b"{$I body.inc}";
    let binding = IncludeBinding::new(
        root_id.clone(),
        directive_range(root, include),
        included_id.clone(),
    );

    let prepared = prepare_source(
        root_id,
        &[source("root.pas", root), source("body.inc", included)],
        &[binding],
        options(),
    )
    .expect("include macro state propagation");

    for text in [
        b"SawRootDefine".as_slice(),
        b"AfterInclude".as_slice(),
        b"UndefWasApplied".as_slice(),
    ] {
        assert!(
            prepared
                .bytes()
                .windows(text.len())
                .any(|window| window == text),
            "missing active text {text:?}"
        );
    }
    assert!(!prepared
        .bytes()
        .windows(b"MustNotAppear".len())
        .any(|window| window == b"MustNotAppear"));
}

#[test]
fn conditional_groups_can_cross_include_boundaries_after_textual_expansion() {
    let root = br#"program Demo;
begin
{$I body.inc}
  RootInside;
{$ENDIF}
end.
"#;
    let included = br#"{$IFDEF FEATURE}
  Included;
"#;
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("body.inc");
    let binding = IncludeBinding::new(
        root_id.clone(),
        directive_range(root, b"{$I body.inc}"),
        included_id.clone(),
    );
    let options = options().with_initial_defined_symbols(["FEATURE"]);

    let prepared = prepare_source(
        &root_id,
        &[source("root.pas", root), source("body.inc", included)],
        &[binding],
        options,
    )
    .expect("include expansion is textual for conditional nesting");
    assert!(prepared
        .bytes()
        .windows(b"Included".len())
        .any(|w| w == b"Included"));
    assert!(prepared
        .bytes()
        .windows(b"RootInside".len())
        .any(|w| w == b"RootInside"));
}

#[test]
fn repeated_includes_receive_distinct_occurrence_ids_and_cross_boundary_ranges_split() {
    let root = br#"program Demo;
begin
  RootBefore;
{$I repeated.inc}
{$I repeated.inc}
  RootAfter;
end.
"#;
    let included = b"  Included;\n";
    let root_id = ProjectSourceId::new("root.pas");
    let included_id = ProjectSourceId::new("repeated.inc");
    let include = b"{$I repeated.inc}";
    let first = directive_range(root, include);
    let second_start = first.end
        + root[first.end..]
            .windows(include.len())
            .position(|window| window == include)
            .expect("second include");
    let bindings = [
        IncludeBinding::new(root_id.clone(), first, included_id.clone()),
        IncludeBinding::new(
            root_id.clone(),
            second_start..second_start + include.len(),
            included_id,
        ),
    ];

    let prepared = prepare_source(
        root_id,
        &[source("root.pas", root), source("repeated.inc", included)],
        &bindings,
        options(),
    )
    .expect("repeated include expansion");

    let included_ranges: Vec<_> = prepared
        .bytes()
        .windows(b"Included".len())
        .enumerate()
        .filter_map(|(start, window)| (window == b"Included").then_some(start..start + 8))
        .collect();
    assert_eq!(included_ranges.len(), 2);
    let first_map = prepared.map_range(included_ranges[0].clone()).unwrap();
    let second_map = prepared.map_range(included_ranges[1].clone()).unwrap();
    assert_ne!(first_map[0].expansion_id, second_map[0].expansion_id);
    assert_eq!(first_map[0].original, second_map[0].original);

    let before = prepared
        .bytes()
        .windows(b"RootBefore".len())
        .position(|window| window == b"RootBefore")
        .unwrap();
    let after = prepared
        .bytes()
        .windows(b"RootAfter".len())
        .position(|window| window == b"RootAfter")
        .unwrap();
    let crossed = prepared
        .map_range(before..after + b"RootAfter".len())
        .unwrap();
    assert!(
        crossed.len() >= 5,
        "include boundaries must remain mappable: {crossed:?}"
    );
}

#[test]
fn active_include_requires_a_selected_target_but_definitely_inactive_include_does_not() {
    let active = br#"program Demo;
begin
{$I missing.inc}
end.
"#;
    let error = prepare_source(
        ProjectSourceId::new("root.pas"),
        &[source("root.pas", active)],
        &[],
        options(),
    )
    .expect_err("active includes cannot be guessed");
    assert!(matches!(
        error,
        cfg_pascal::PrepareSourceError::UnresolvedInclude { .. }
    ));

    let inactive = br#"program Demo;
begin
{$IFDEF OMIT}
{$I missing.inc}
{$ENDIF}
end.
"#;
    prepare_source(
        ProjectSourceId::new("root.pas"),
        &[source("root.pas", inactive)],
        &[],
        options(),
    )
    .expect("inactive includes are not resolved");
}

#[test]
fn include_bindings_validate_exact_sites_targets_and_duplicates_before_expansion() {
    let root = br#"program Demo;
begin
{$I selected.inc}
end.
"#;
    let root_id = ProjectSourceId::new("root.pas");
    let target_id = ProjectSourceId::new("selected.inc");
    let include = b"{$I selected.inc}";
    let range = directive_range(root, include);
    let snapshots = [
        source("root.pas", root),
        source("selected.inc", b"  Body;\n"),
    ];

    let missing_source = IncludeBinding::new(
        ProjectSourceId::new("other.pas"),
        range.clone(),
        target_id.clone(),
    );
    assert!(matches!(
        prepare_source(&root_id, &snapshots, &[missing_source], options()),
        Err(cfg_pascal::PrepareSourceError::IncludeBindingSourceNotLoaded { .. })
    ));

    let wrong_range = IncludeBinding::new(
        root_id.clone(),
        range.start..range.end - 1,
        target_id.clone(),
    );
    assert!(matches!(
        prepare_source(&root_id, &snapshots, &[wrong_range], options()),
        Err(cfg_pascal::PrepareSourceError::IncludeBindingNotIncludeDirective { .. })
    ));

    let missing_target = IncludeBinding::new(
        root_id.clone(),
        range.clone(),
        ProjectSourceId::new("not-loaded.inc"),
    );
    assert!(matches!(
        prepare_source(&root_id, &snapshots, &[missing_target], options()),
        Err(cfg_pascal::PrepareSourceError::IncludeTargetNotLoaded { .. })
    ));

    let duplicate = [
        IncludeBinding::new(root_id.clone(), range.clone(), target_id.clone()),
        IncludeBinding::new(root_id.clone(), range.clone(), target_id.clone()),
    ];
    assert!(matches!(
        prepare_source(&root_id, &snapshots, &duplicate, options()),
        Err(cfg_pascal::PrepareSourceError::DuplicateIncludeBinding { .. })
    ));

    let switch_root = b"program Demo; begin {$I+} end.";
    let switch_range = directive_range(switch_root, b"{$I+}");
    let switch_binding = IncludeBinding::new(
        root_id.clone(),
        switch_range,
        ProjectSourceId::new("selected.inc"),
    );
    assert!(matches!(
        prepare_source(
            &root_id,
            &[
                source("root.pas", switch_root),
                source("selected.inc", b"Body;")
            ],
            &[switch_binding],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::IncludeBindingNotIncludeDirective { .. })
    ));

    let malformed_root = b"program Demo; begin {$I 'unterminated} end.";
    let malformed_binding = IncludeBinding::new(
        root_id.clone(),
        directive_range(malformed_root, b"{$I 'unterminated}"),
        ProjectSourceId::new("selected.inc"),
    );
    assert!(matches!(
        prepare_source(
            &root_id,
            &[
                source("root.pas", malformed_root),
                source("selected.inc", b"Body;")
            ],
            &[malformed_binding],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::InvalidDirective { .. })
    ));

    let quoted_root = b"program Demo; begin {$I 'selected.inc'} {$INCLUDE \"other.inc\"} end.";
    let selected_id = ProjectSourceId::new("selected.inc");
    let other_id = ProjectSourceId::new("other.inc");
    let quoted_bindings = [
        IncludeBinding::new(
            root_id.clone(),
            directive_range(quoted_root, b"{$I 'selected.inc'}"),
            selected_id.clone(),
        ),
        IncludeBinding::new(
            root_id.clone(),
            directive_range(quoted_root, b"{$INCLUDE \"other.inc\"}"),
            other_id.clone(),
        ),
    ];
    let quoted = prepare_source(
        &root_id,
        &[
            source("root.pas", quoted_root),
            source(selected_id.as_str(), b"Selected;"),
            source(other_id.as_str(), b"Other;"),
        ],
        &quoted_bindings,
        options(),
    )
    .expect("balanced quoted include paths are supported");
    assert!(quoted
        .bytes()
        .windows(b"Selected;".len())
        .any(|w| w == b"Selected;"));
    assert!(quoted
        .bytes()
        .windows(b"Other;".len())
        .any(|w| w == b"Other;"));
}

#[test]
fn active_unsupported_effectful_directives_and_malformed_nesting_fail_strictly() {
    let unsupported = b"program Demo; {$MODE DELPHI} begin end.";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("root.pas"),
            &[source("root.pas", unsupported)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::UnsupportedDirective { .. })
    ));

    let inactive_unsupported = b"program Demo; {$IF FALSE}{$MODE DELPHI}{$ENDIF} begin end.";
    prepare_source(
        ProjectSourceId::new("root.pas"),
        &[source("root.pas", inactive_unsupported)],
        &[],
        options(),
    )
    .expect("unsupported directives in inactive branches are not effects");

    let lexical_fragment = b"program Demo; {$IFDEF_FEATURE} Drop; {$ELSE} Keep; {$ENDIF} end.";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("root.pas"),
            &[source("root.pas", lexical_fragment)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::UnsupportedDirective { .. })
    ));

    let unmatched_end = b"program Demo; {$ENDIF} begin end.";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("root.pas"),
            &[source("root.pas", unmatched_end)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::MalformedNesting { .. })
    ));

    let missing_end = b"program Demo; {$IF TRUE} begin end.";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("root.pas"),
            &[source("root.pas", missing_end)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::MalformedNesting { .. })
    ));

    let duplicate_else = b"program Demo; {$IF TRUE} {$ELSE} {$ELSE} begin end. {$ENDIF}";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("root.pas"),
            &[source("root.pas", duplicate_else)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::MalformedNesting { .. })
    ));
}

#[test]
fn active_include_cycles_are_reported_with_the_source_chain() {
    let root = b"program Demo; begin {$I a.inc} end.";
    let a = b"{$I root.pas}";
    let root_id = ProjectSourceId::new("root.pas");
    let a_id = ProjectSourceId::new("a.inc");
    let root_range = directive_range(root, b"{$I a.inc}");
    let a_range = directive_range(a, b"{$I root.pas}");
    let bindings = [
        IncludeBinding::new(root_id.clone(), root_range, a_id.clone()),
        IncludeBinding::new(a_id.clone(), a_range, root_id.clone()),
    ];

    let error = prepare_source(
        &root_id,
        &[source("root.pas", root), source("a.inc", a)],
        &bindings,
        options(),
    )
    .expect_err("recursive include must be rejected");
    match error {
        cfg_pascal::PrepareSourceError::IncludeCycle { cycle, .. } => {
            assert_eq!(cycle, vec![root_id, a_id, ProjectSourceId::new("root.pas")]);
        }
        other => panic!("unexpected cycle error: {other:?}"),
    }
}

#[test]
fn strict_preparation_rejects_empty_active_projections() {
    let empty = b"";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("empty.pas"),
            &[source("empty.pas", empty)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::NoCompleteContent { .. })
    ));

    let only_inactive = b"{$IF FALSE}never{$ENDIF}";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("empty.pas"),
            &[source("empty.pas", only_inactive)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::NoCompleteContent { .. })
    ));

    let whitespace_only = b" \r\n\t";
    assert!(matches!(
        prepare_source(
            ProjectSourceId::new("empty.pas"),
            &[source("empty.pas", whitespace_only)],
            &[],
            options(),
        ),
        Err(cfg_pascal::PrepareSourceError::NoCompleteContent { .. })
    ));
}

fn limited_options(mut limits: cfg_pascal::PreparationLimits) -> PrepareSourceOptions {
    limits.max_source_bytes = limits.max_source_bytes.max(1);
    PrepareSourceOptions::new(
        ProjectSourceId::new("limited.prepared"),
        "debug",
        PreparationEnvironment::Complete,
    )
    .with_limits(limits)
}

#[test]
fn configured_budgets_fail_deterministically_before_unbounded_growth() {
    let root = b"program Demo; begin end.";
    let root_id = ProjectSourceId::new("root.pas");
    let cases = [
        (cfg_pascal::PreparationBudget::SourceBytes, {
            cfg_pascal::PreparationLimits {
                max_source_bytes: root.len() - 1,
                ..Default::default()
            }
        }),
        (cfg_pascal::PreparationBudget::OutputBytes, {
            cfg_pascal::PreparationLimits {
                max_output_bytes: root.len() - 1,
                ..Default::default()
            }
        }),
        (cfg_pascal::PreparationBudget::Work, {
            cfg_pascal::PreparationLimits {
                max_work: root.len() - 1,
                ..Default::default()
            }
        }),
    ];
    for (budget, limits) in cases {
        let error = prepare_source(
            &root_id,
            &[source("root.pas", root)],
            &[],
            limited_options(limits),
        )
        .expect_err("configured budget must be enforced");
        assert!(
            matches!(error, cfg_pascal::PrepareSourceError::BudgetExceeded { budget: found, .. } if found == budget),
            "unexpected budget error for {budget:?}: {error:?}"
        );
    }

    let limits = cfg_pascal::PreparationLimits {
        max_directives: 0,
        ..Default::default()
    };
    let directive_root = b"program Demo; {$IF TRUE} begin end. {$ENDIF}";
    let error = prepare_source(
        &root_id,
        &[source("root.pas", directive_root)],
        &[],
        limited_options(limits),
    )
    .expect_err("directive budget must be enforced");
    assert!(matches!(
        error,
        cfg_pascal::PrepareSourceError::BudgetExceeded {
            budget: cfg_pascal::PreparationBudget::Directives,
            ..
        }
    ));

    let conditional = b"program Demo; {$IF TRUE} begin end. {$ENDIF}";
    for (budget, limits) in [
        (cfg_pascal::PreparationBudget::ConditionalDepth, {
            cfg_pascal::PreparationLimits {
                max_conditional_depth: 0,
                ..Default::default()
            }
        }),
        (cfg_pascal::PreparationBudget::ExpressionBytes, {
            cfg_pascal::PreparationLimits {
                max_expression_bytes: 0,
                ..Default::default()
            }
        }),
        (cfg_pascal::PreparationBudget::ExpressionTokens, {
            cfg_pascal::PreparationLimits {
                max_expression_tokens: 0,
                ..Default::default()
            }
        }),
    ] {
        let error = prepare_source(
            &root_id,
            &[source("root.pas", conditional)],
            &[],
            limited_options(limits),
        )
        .expect_err("conditional budget must be enforced");
        assert!(
            matches!(error, cfg_pascal::PrepareSourceError::BudgetExceeded { budget: found, .. } if found == budget),
            "unexpected conditional budget error for {budget:?}: {error:?}"
        );
    }

    let included = b"Body;\n";
    let include_root = b"program Demo; begin {$I body.inc} end.";
    let include_range = directive_range(include_root, b"{$I body.inc}");
    let limits = cfg_pascal::PreparationLimits {
        max_expanded_occurrences: 0,
        ..Default::default()
    };
    let error = prepare_source(
        &root_id,
        &[
            source("root.pas", include_root),
            source("body.inc", included),
        ],
        &[IncludeBinding::new(
            root_id.clone(),
            include_range,
            ProjectSourceId::new("body.inc"),
        )],
        limited_options(limits),
    )
    .expect_err("occurrence budget must be enforced");
    assert!(matches!(
        error,
        cfg_pascal::PrepareSourceError::BudgetExceeded {
            budget: cfg_pascal::PreparationBudget::ExpandedOccurrences,
            ..
        }
    ));

    let repeated_root = b"program Demo; {$I x}{$I x}{$I x}{$I x} begin end.";
    let repeated_target = format!("{{$DEFINE {}}}", "A".repeat(10_000));
    let repeated_root_id = ProjectSourceId::new("repeated-root.pas");
    let repeated_target_id = ProjectSourceId::new("repeated.inc");
    let repeated_bindings: Vec<_> = std::str::from_utf8(repeated_root)
        .unwrap()
        .match_indices("{$I x}")
        .map(|(start, _)| {
            IncludeBinding::new(
                repeated_root_id.clone(),
                start..start + b"{$I x}".len(),
                repeated_target_id.clone(),
            )
        })
        .collect();
    let mut repeated_options = options();
    repeated_options.limits.max_work = 20_000;
    let error = match prepare_source(
        &repeated_root_id,
        &[
            source(repeated_root_id.as_str(), repeated_root),
            source(repeated_target_id.as_str(), repeated_target.as_bytes()),
        ],
        &repeated_bindings,
        repeated_options,
    ) {
        Ok(prepared) => panic!(
            "repeated directive bodies were not charged: {} output bytes",
            prepared.bytes().len()
        ),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        cfg_pascal::PrepareSourceError::BudgetExceeded {
            budget: cfg_pascal::PreparationBudget::Work,
            ..
        }
    ));

    let limits = cfg_pascal::PreparationLimits {
        max_include_depth: 0,
        ..Default::default()
    };
    let error = prepare_source(
        &root_id,
        &[
            source("root.pas", include_root),
            source("body.inc", included),
        ],
        &[IncludeBinding::new(
            root_id.clone(),
            directive_range(include_root, b"{$I body.inc}"),
            ProjectSourceId::new("body.inc"),
        )],
        limited_options(limits),
    )
    .expect_err("depth budget must be enforced");
    assert!(matches!(
        error,
        cfg_pascal::PrepareSourceError::BudgetExceeded {
            budget: cfg_pascal::PreparationBudget::IncludeDepth,
            ..
        }
    ));
}

#[test]
fn nested_include_fragments_parse_as_one_buffer_and_keep_hierarchical_origins() {
    let root = br#"program Demo;
begin
{$I outer/common.inc}
end.
"#;
    let outer = br#"  OuterBefore;
{$I nested/common.inc}
  OuterAfter;
"#;
    let inner = b"  Inner;\n";
    let root_id = ProjectSourceId::new("root.pas");
    let outer_id = ProjectSourceId::new("outer/common.inc");
    let inner_id = ProjectSourceId::new("nested/common.inc");
    let root_range = directive_range(root, b"{$I outer/common.inc}");
    let outer_range = directive_range(outer, b"{$I nested/common.inc}");
    let bindings = [
        IncludeBinding::new(root_id.clone(), root_range, outer_id.clone()),
        IncludeBinding::new(outer_id.clone(), outer_range, inner_id.clone()),
    ];

    let prepared = prepare_source(
        root_id,
        &[
            source("root.pas", root),
            source("outer/common.inc", outer),
            source("nested/common.inc", inner),
        ],
        &bindings,
        options(),
    )
    .expect("nested structural fragments");

    for text in [
        b"OuterBefore".as_slice(),
        b"Inner".as_slice(),
        b"OuterAfter".as_slice(),
    ] {
        assert!(prepared
            .bytes()
            .windows(text.len())
            .any(|window| window == text));
    }
    let inner_start = prepared
        .bytes()
        .windows(b"Inner".len())
        .position(|window| window == b"Inner")
        .unwrap();
    let mapped = prepared.map_range(inner_start..inner_start + 5).unwrap();
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].original.as_ref().unwrap().source_id, inner_id);
    assert!(mapped[0]
        .expansion_id
        .as_str()
        .contains("include-1/include-2"));
}

#[test]
fn caller_selected_same_basename_sources_remain_distinct() {
    let root = br#"program Demo;
begin
{$I common.inc}
{$I common.inc}
end.
"#;
    let first = b"First;\n";
    let second = b"Second;\n";
    let root_id = ProjectSourceId::new("root.pas");
    let first_id = ProjectSourceId::new("one/common.inc");
    let second_id = ProjectSourceId::new("two/common.inc");
    let include = b"{$I common.inc}";
    let first_range = directive_range(root, include);
    let second_start = first_range.end
        + root[first_range.end..]
            .windows(include.len())
            .position(|window| window == include)
            .unwrap();
    let bindings = [
        IncludeBinding::new(root_id.clone(), first_range, first_id.clone()),
        IncludeBinding::new(
            root_id.clone(),
            second_start..second_start + include.len(),
            second_id.clone(),
        ),
    ];

    let prepared = prepare_source(
        root_id,
        &[
            source("root.pas", root),
            source("one/common.inc", first),
            source("two/common.inc", second),
        ],
        &bindings,
        options(),
    )
    .expect("same basenames are selected by source identity");
    let first_start = prepared
        .bytes()
        .windows(b"First".len())
        .position(|window| window == b"First")
        .unwrap();
    let second_start = prepared
        .bytes()
        .windows(b"Second".len())
        .position(|window| window == b"Second")
        .unwrap();
    assert_eq!(
        prepared.map_range(first_start..first_start + 5).unwrap()[0]
            .original
            .as_ref()
            .unwrap()
            .source_id,
        first_id
    );
    assert_eq!(
        prepared.map_range(second_start..second_start + 6).unwrap()[0]
            .original
            .as_ref()
            .unwrap()
            .source_id,
        second_id
    );
}

fn options() -> PrepareSourceOptions {
    PrepareSourceOptions::new(
        ProjectSourceId::new("demo.prepared"),
        "debug",
        PreparationEnvironment::Complete,
    )
}

#[test]
fn complete_ifdef_masks_the_inactive_branch_and_keeps_the_active_source_mapped() {
    let bytes = br#"program Demo;
{$IFDEF FEATURE}
begin
  Keep;
end.
{$ELSE}
begin
  Drop;
end.
{$ENDIF}
"#;
    let mut options = options();
    options.initial_defined_symbols.push("FEATURE".to_string());

    let prepared = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        options,
    )
    .expect("complete conditional preparation");

    assert!(prepared
        .bytes()
        .windows(b"Keep".len())
        .any(|w| w == b"Keep"));
    assert!(!prepared
        .bytes()
        .windows(b"Drop".len())
        .any(|w| w == b"Drop"));
    assert!(prepared.bytes().windows(b"{$".len()).all(|w| w != b"{$"));
    assert_eq!(prepared.bytes().len(), bytes.len());

    let keep_start = bytes
        .windows(b"Keep".len())
        .position(|window| window == b"Keep")
        .expect("Keep source range");
    let mapped = prepared
        .map_range(keep_start..keep_start + b"Keep".len())
        .expect("active source maps");
    assert_eq!(mapped.len(), 1);
    assert_eq!(
        mapped[0].original.as_ref().unwrap().source_id.as_str(),
        "demo.pas"
    );
    assert_eq!(
        mapped[0].original.as_ref().unwrap().byte_range,
        keep_start..keep_start + 4
    );
}

#[test]
fn boolean_if_accepts_a_parenthesized_expression_without_keyword_whitespace() {
    let bytes = br#"program Demo;
{$IF(DEFINED(FEATURE) OR FALSE)}
begin
  Keep;
end.
{$ELSE}
begin
  Drop;
end.
{$ENDIF}
"#;
    let mut options = options();
    options.initial_defined_symbols.push("FEATURE".to_string());

    let prepared = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        options,
    )
    .expect("parenthesized boolean condition");

    assert!(prepared
        .bytes()
        .windows(b"Keep".len())
        .any(|w| w == b"Keep"));
    assert!(!prepared
        .bytes()
        .windows(b"Drop".len())
        .any(|w| w == b"Drop"));
}

#[test]
fn partial_environment_keeps_absent_symbols_unknown_but_complete_environment_knows_false() {
    let bytes = br#"program Demo;
{$IFDEF EXTERNAL_FEATURE}
begin
  External;
end.
{$ENDIF}
begin
end.
"#;

    let partial = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        PrepareSourceOptions::new(
            ProjectSourceId::new("demo.prepared"),
            "debug",
            PreparationEnvironment::Partial,
        ),
    )
    .expect_err("an active unknown condition is not strict-complete");
    assert!(
        matches!(
            partial,
            cfg_pascal::PrepareSourceError::UnknownActiveCondition { .. }
        ),
        "unexpected partial error: {partial:?}"
    );

    let complete = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        PrepareSourceOptions::new(
            ProjectSourceId::new("demo.prepared"),
            "debug",
            PreparationEnvironment::Complete,
        ),
    )
    .expect("complete environment treats absent symbols as false");
    assert!(!complete
        .bytes()
        .windows(b"External".len())
        .any(|window| window == b"External"));
}

#[test]
fn partial_boolean_short_circuit_can_prove_a_condition_without_knowing_a_symbol() {
    let bytes = br#"program Demo;
{$IF FALSE AND Defined(MISSING)}
begin
  DropAnd;
end.
{$ELSEIF TRUE OR Defined(OTHER_MISSING)}
begin
  KeepOr;
end.
{$ELSE}
begin
  DropElse;
end.
{$ENDIF}
"#;
    let prepared = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        PrepareSourceOptions::new(
            ProjectSourceId::new("demo.prepared"),
            "debug",
            PreparationEnvironment::Partial,
        ),
    )
    .expect("short-circuiting conditions are known");

    assert!(prepared
        .bytes()
        .windows(b"KeepOr".len())
        .any(|w| w == b"KeepOr"));
    for text in [b"DropAnd".as_slice(), b"DropElse".as_slice()] {
        assert!(!prepared.bytes().windows(text.len()).any(|w| w == text));
    }
}

#[test]
fn long_boolean_chains_are_evaluated_without_recursive_ast_walks() {
    let expression = format!("TRUE{}", " AND TRUE".repeat(16_000));
    let bytes =
        format!("program Demo; {{$IF {expression}}} begin Keep; end. {{$ENDIF}}").into_bytes();
    let root_id = ProjectSourceId::new("long-condition.pas");
    let mut options = options();
    options.limits.max_source_bytes = bytes.len();
    options.limits.max_expression_bytes = bytes.len();
    options.limits.max_work = bytes.len() * 4;

    let prepared = prepare_source(&root_id, &[source(root_id.as_str(), &bytes)], &[], options)
        .expect("a long bounded boolean chain should evaluate successfully");
    assert!(prepared
        .bytes()
        .windows(b"Keep".len())
        .any(|w| w == b"Keep"));
}

#[test]
fn large_flat_boolean_chain_is_stack_safe_under_default_limits_in_a_subprocess() {
    const STACK_PROBE: &str = "CFG_PASCAL_PREPARATION_STACK_PROBE";
    if std::env::var_os(STACK_PROBE).is_some() {
        let joined = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let expression = format!("TRUE{}", " AND TRUE".repeat(100_000));
                let bytes =
                    format!("program Demo; {{$IF {expression}}} begin Keep; end. {{$ENDIF}}");
                let prepared = prepare_source(
                    ProjectSourceId::new("large-condition.pas"),
                    &[source("large-condition.pas", bytes.as_bytes())],
                    &[],
                    options(),
                )
                .expect("default limits should not abort on a flat boolean chain");
                assert!(prepared
                    .bytes()
                    .windows(b"Keep".len())
                    .any(|w| w == b"Keep"));
            })
            .expect("spawn small-stack probe")
            .join();
        assert!(joined.is_ok(), "small-stack probe panicked: {joined:?}");
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "large_flat_boolean_chain_is_stack_safe_under_default_limits_in_a_subprocess",
            "--nocapture",
        ])
        .env(STACK_PROBE, "1")
        .output()
        .expect("run small-stack subprocess");
    assert!(
        output.status.success(),
        "small-stack subprocess failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_tokens_are_rejected_even_when_boolean_short_circuit_would_hide_them() {
    let bytes = br#"program Demo;
{$IF FALSE AND NOT_A_SUPPORTED_OPERAND}
begin
  Never;
end.
{$ENDIF}
"#;
    let error = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        PrepareSourceOptions::new(
            ProjectSourceId::new("demo.prepared"),
            "debug",
            PreparationEnvironment::Partial,
        ),
    )
    .expect_err("the full condition syntax must be validated");
    assert!(matches!(
        error,
        cfg_pascal::PrepareSourceError::InvalidDirective { .. }
    ));
}

#[test]
fn source_order_define_and_explicit_undef_change_later_conditions() {
    let bytes = br#"program Demo;
{$DEFINE FEATURE}
{$IFDEF FEATURE}
const First = 1;
{$ELSE}
const First = 2;
{$ENDIF}
{$UNDEF FEATURE}
{$IFDEF FEATURE}
const Second = 1;
{$ELSE}
const Second = 2;
{$ENDIF}
begin
end.
"#;
    let prepared = prepare_source(
        ProjectSourceId::new("demo.pas"),
        &[source("demo.pas", bytes)],
        &[],
        options(),
    )
    .expect("source-order symbol mutations");

    assert!(prepared
        .bytes()
        .windows(b"const First = 1".len())
        .any(|w| w == b"const First = 1"));
    assert!(prepared
        .bytes()
        .windows(b"const Second = 2".len())
        .any(|w| w == b"const Second = 2"));
    assert!(!prepared
        .bytes()
        .windows(b"const First = 2".len())
        .any(|w| w == b"const First = 2"));
    assert!(!prepared
        .bytes()
        .windows(b"const Second = 1".len())
        .any(|w| w == b"const Second = 1"));
}

#[test]
fn directives_inside_strings_comments_and_line_comments_are_not_executed() {
    let bytes = br#"program Demo;
const
  StringBrace = '{$IFDEF NOT_A_DIRECTIVE}';
  StringParen = '(*$IFDEF NOT_A_DIRECTIVE*)';
  { (*$IFDEF NOT_A_DIRECTIVE*) }
  (* ordinary {$IFDEF NOT_A_DIRECTIVE} *)
  // {$IFDEF NOT_A_DIRECTIVE}
begin
{$IFDEF FEATURE}
  Keep;
{$ENDIF}
(*$IFDEF FEATURE*)
  KeepCommentStyle;
(*$ENDIF*)
end.
"#;
    let mut options = options();
    options.initial_defined_symbols.push("FEATURE".to_string());

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&cfg_pascal::LANGUAGE.into()).unwrap();
    let raw_tree = parser.parse(bytes, None).unwrap();
    assert!(
        !raw_tree.root_node().has_error(),
        "{}",
        raw_tree.root_node().to_sexp()
    );

    let prepared = prepare_source(
        ProjectSourceId::new("comments.pas"),
        &[source("comments.pas", bytes)],
        &[],
        options,
    )
    .expect("only real directives are executed");

    assert!(prepared
        .bytes()
        .windows(b"StringBrace = '{$IFDEF NOT_A_DIRECTIVE}'".len())
        .any(|window| window == b"StringBrace = '{$IFDEF NOT_A_DIRECTIVE}'"));
    assert!(prepared
        .bytes()
        .windows(b"StringParen = '(*$IFDEF NOT_A_DIRECTIVE*)'".len())
        .any(|window| window == b"StringParen = '(*$IFDEF NOT_A_DIRECTIVE*)'"));
    assert!(prepared
        .bytes()
        .windows(b"Keep;".len())
        .any(|w| w == b"Keep;"));
    assert!(prepared
        .bytes()
        .windows(b"KeepCommentStyle;".len())
        .any(|w| w == b"KeepCommentStyle;"));
}
