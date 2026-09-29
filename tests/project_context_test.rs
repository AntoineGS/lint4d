use std::{fmt::Write as _, ops::Range};

use cfg_core::{BasicBlockKind, BlockId, Cfg, EdgeKind};
use cfg_pascal::{
    build_file_cfgs, build_file_cfgs_in_project, ImportBinding, ImportTarget, ProjectBuildError,
    ProjectSnapshot, ProjectSnapshotError, ProjectSourceId, ProjectUnitId, ProjectUnitInput,
    UsesSite,
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
        "project fixture must not contain parser errors:\n{}",
        tree.root_node().to_sexp()
    );
    tree
}

fn unit(id: &str, source: &str) -> (ProjectUnitInput, Tree, Vec<u8>) {
    let source = source.as_bytes().to_vec();
    let tree = parse_clean(&source);
    let input = ProjectUnitInput::new(
        ProjectUnitId::new(id),
        ProjectSourceId::new(format!("{id}.pas")),
        tree.clone(),
        source.clone(),
    );
    (input, tree, source)
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

fn import(
    importer: &str,
    importer_tree: &Tree,
    index: usize,
    target: ImportTarget,
    qualifiers: &[&str],
) -> ImportBinding {
    ImportBinding::new(
        UsesSite::new(
            ProjectUnitId::new(importer),
            uses_spans(importer_tree)[index].clone(),
        ),
        target,
        qualifiers.iter().copied(),
    )
}

fn cfg_for<'a>(cfgs: &'a [Cfg], name: &str) -> &'a Cfg {
    cfgs.iter()
        .find(|cfg| cfg.proc_name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = cfgs.iter().map(|cfg| cfg.proc_name.as_str()).collect();
            panic!("CFG for {name:?} not found; available procedures: {names:?}")
        })
}

fn block_with_stmt(cfg: &Cfg, source: &[u8], kind: &str, text: &str) -> BlockId {
    let blocks = blocks_with_stmt(cfg, source, kind, text);
    assert_eq!(
        blocks.len(),
        1,
        "expected one {kind:?} statement containing {text:?}, got {blocks:?}"
    );
    blocks[0]
}

fn blocks_with_stmt(cfg: &Cfg, source: &[u8], kind: &str, text: &str) -> Vec<BlockId> {
    cfg.graph
        .node_indices()
        .filter_map(|index| {
            let block = &cfg.graph[index];
            block
                .stmts
                .iter()
                .any(|stmt| {
                    stmt.node_kind == kind
                        && std::str::from_utf8(&source[stmt.byte_range.clone()])
                            .is_ok_and(|stmt_text| stmt_text.contains(text))
                })
                .then(|| BlockId::from(index))
        })
        .collect()
}

fn successors(cfg: &Cfg, from: BlockId) -> Vec<(BlockId, EdgeKind)> {
    cfg.graph
        .edge_indices()
        .filter_map(|edge| {
            let (source, target) = cfg.graph.edge_endpoints(edge)?;
            (source == from.index()).then(|| (BlockId::from(target), cfg.graph[edge].clone()))
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
        .expect("constructor raise must have a synthetic successful-raise block")
}

fn can_reach(cfg: &Cfg, from: BlockId, to: BlockId) -> bool {
    let mut pending = vec![from];
    let mut visited = std::collections::HashSet::new();

    while let Some(block) = pending.pop() {
        if block == to {
            return true;
        }
        if visited.insert(block) {
            pending.extend(cfg.graph.neighbors(block.index()).map(BlockId::from));
        }
    }

    false
}

type BlockSignature = (BasicBlockKind, Vec<(Range<usize>, String)>);
type CfgSignature = (
    String,
    Range<usize>,
    Vec<BlockSignature>,
    Vec<(usize, usize, EdgeKind)>,
);

fn cfg_signature(cfg: &Cfg) -> CfgSignature {
    let nodes = cfg
        .graph
        .node_indices()
        .map(|index| {
            let block = &cfg.graph[index];
            (
                block.kind.clone(),
                block
                    .stmts
                    .iter()
                    .map(|stmt| (stmt.byte_range.clone(), stmt.node_kind.clone()))
                    .collect(),
            )
        })
        .collect();
    let edges = cfg
        .graph
        .edge_indices()
        .map(|edge| {
            let (source, target) = cfg.graph.edge_endpoints(edge).expect("edge endpoints");
            (source.index(), target.index(), cfg.graph[edge].clone())
        })
        .collect();
    (cfg.proc_name.clone(), cfg.byte_range.clone(), nodes, edges)
}

#[test]
fn project_snapshot_rejects_duplicate_ids_sites_and_dangling_loaded_targets() {
    let (consumer, consumer_tree, _) = unit(
        "consumer",
        "unit Consumer; interface uses Missing; implementation end.",
    );
    let (same_id, _, _) = unit("consumer", "unit Other; interface implementation end.");
    let same_source_tree = parse_clean(b"unit Other; interface implementation end.");
    let same_source = ProjectUnitInput::new(
        ProjectUnitId::new("other"),
        ProjectSourceId::new("consumer.pas"),
        same_source_tree,
        b"unit Other; interface implementation end.",
    );

    assert!(matches!(
        ProjectSnapshot::new(vec![consumer.clone(), same_id], Vec::new()),
        Err(ProjectSnapshotError::DuplicateUnitId(_))
    ));
    assert!(matches!(
        ProjectSnapshot::new(vec![consumer.clone(), same_source], Vec::new()),
        Err(ProjectSnapshotError::DuplicateSourceId(_))
    ));

    let duplicate_site = import(
        "consumer",
        &consumer_tree,
        0,
        ImportTarget::Unavailable,
        &["Missing"],
    );
    assert!(matches!(
        ProjectSnapshot::new(
            vec![consumer.clone()],
            vec![duplicate_site.clone(), duplicate_site]
        ),
        Err(ProjectSnapshotError::DuplicateImportSite { .. })
    ));

    let dangling = import(
        "consumer",
        &consumer_tree,
        0,
        ImportTarget::Loaded(ProjectUnitId::new("does-not-exist")),
        &["Missing"],
    );
    assert!(matches!(
        ProjectSnapshot::new(vec![consumer], vec![dangling]),
        Err(ProjectSnapshotError::DanglingLoadedTarget { .. })
    ));
}

#[test]
fn project_snapshot_rejects_non_uses_and_out_of_bounds_binding_sites() {
    let (consumer, consumer_tree, source) =
        unit("consumer", "unit Consumer; interface implementation end.");
    let truncated_source = ProjectUnitInput::new(
        ProjectUnitId::new("truncated"),
        ProjectSourceId::new("truncated.pas"),
        consumer_tree,
        Vec::new(),
    );
    assert!(matches!(
        ProjectSnapshot::new(vec![truncated_source], Vec::new()),
        Err(ProjectSnapshotError::InvalidTreeSource { .. })
    ));

    let not_a_uses_site = ImportBinding::new(
        UsesSite::new(ProjectUnitId::new("consumer"), 0..4),
        ImportTarget::Unavailable,
        ["Consumer"],
    );
    assert!(matches!(
        ProjectSnapshot::new(vec![consumer.clone()], vec![not_a_uses_site]),
        Err(ProjectSnapshotError::InvalidImportSite { .. })
    ));

    let out_of_bounds = ImportBinding::new(
        UsesSite::new(
            ProjectUnitId::new("consumer"),
            source.len()..source.len() + 1,
        ),
        ImportTarget::Unavailable,
        ["Consumer"],
    );
    assert!(matches!(
        ProjectSnapshot::new(vec![consumer], vec![out_of_bounds]),
        Err(ProjectSnapshotError::InvalidImportSite { .. })
    ));
}

#[test]
fn project_snapshot_accepts_utf8_bom_before_tree_root() {
    let source = b"\xef\xbb\xbfunit Bom; interface implementation end.";
    let tree = parse_clean(source);
    let root = tree.root_node();
    assert_eq!(root.start_byte(), 3);
    assert_eq!(root.end_byte(), source.len());

    let input = ProjectUnitInput::new(
        ProjectUnitId::new("bom"),
        ProjectSourceId::new("bom.pas"),
        tree,
        source,
    );
    ProjectSnapshot::new(vec![input], Vec::new())
        .expect("UTF-8 BOM bytes are part of the source snapshot");
}

#[test]
fn project_snapshot_validates_many_sites_in_a_large_unit_without_dropping_any_binding() {
    const IMPORT_COUNT: usize = 400;
    const STATEMENT_COUNT: usize = 20_000;

    let mut consumer_source = String::with_capacity(160 * 1024);
    consumer_source.push_str("unit Consumer; interface uses ");
    for index in 0..IMPORT_COUNT {
        if index > 0 {
            consumer_source.push_str(", ");
        }
        write!(consumer_source, "External{index}").expect("write import name");
    }
    consumer_source.push_str("; implementation procedure P; begin\n");
    for index in 0..STATEMENT_COUNT {
        writeln!(consumer_source, "  Statement{index};").expect("write statement");
    }
    consumer_source.push_str("end; end.");
    let consumer_source = consumer_source.into_bytes();
    let consumer_tree = parse_clean(&consumer_source);
    let use_sites = uses_spans(&consumer_tree);
    assert_eq!(use_sites.len(), IMPORT_COUNT);

    let mut units = Vec::with_capacity(IMPORT_COUNT + 1);
    for index in 0..IMPORT_COUNT {
        let target_id = format!("target-{index}");
        let target_source = format!("unit External{index}; interface implementation end.");
        units.push(unit(&target_id, &target_source).0);
    }
    units.push(ProjectUnitInput::new(
        ProjectUnitId::new("consumer"),
        ProjectSourceId::new("consumer.pas"),
        consumer_tree,
        consumer_source,
    ));

    let imports = use_sites
        .into_iter()
        .enumerate()
        .map(|(index, byte_range)| {
            ImportBinding::new(
                UsesSite::new(ProjectUnitId::new("consumer"), byte_range),
                ImportTarget::Loaded(ProjectUnitId::new(format!("target-{index}"))),
                [format!("External{index}")],
            )
        })
        .collect();

    let snapshot = ProjectSnapshot::new(units, imports).expect("large project snapshot");
    assert_eq!(snapshot.units().len(), IMPORT_COUNT + 1);
    assert_eq!(snapshot.imports().len(), IMPORT_COUNT);
    assert_eq!(
        snapshot
            .unit(&ProjectUnitId::new("consumer"))
            .unwrap()
            .id()
            .as_str(),
        "consumer"
    );
}

#[test]
fn project_build_has_singleton_raw_source_parity() {
    let source = br#"
unit SingletonParity;
interface
implementation
procedure P;
begin
  try
    raise E.Create;
  except
    on E do Handle;
  end;
end;
end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let raw = build_file_cfgs(&tree, &source);
    let input = ProjectUnitInput::new(
        ProjectUnitId::new("singleton"),
        ProjectSourceId::new("singleton.pas"),
        tree.clone(),
        source.clone(),
    );
    let snapshot = ProjectSnapshot::new(vec![input], Vec::new()).expect("valid singleton snapshot");
    assert!(matches!(
        build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("missing")),
        Err(ProjectBuildError::UnitNotFound(_))
    ));
    let project = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("singleton"))
        .expect("singleton project build");

    assert_eq!(raw.len(), project.len());
    for (raw_cfg, project_cfg) in raw.iter().zip(&project) {
        assert_eq!(cfg_signature(raw_cfg), cfg_signature(project_cfg));
    }
}

#[test]
fn cross_unit_alias_and_ancestry_use_explicit_imports_and_stable_type_identity() {
    let (base, _, _) = unit(
        "base",
        r#"
unit Base.Errors;
interface
type
  TBaseError = class
    constructor Create;
  end;
implementation
end.
"#,
    );
    let (child, child_tree, _) = unit(
        "child",
        r#"
unit Child.Errors;
interface
uses Base.Errors;
type
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TAliasError = TChildError;
implementation
end.
"#,
    );
    let (other, _, _) = unit(
        "other",
        r#"
unit Other.Errors;
interface
type
  TChildError = class
    constructor Create;
  end;
implementation
end.
"#,
    );
    let (consumer, consumer_tree, consumer_source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Child.Errors, Base.Errors, Other.Errors;
implementation





procedure P;
begin
  try
    raise ChildAlias.TAliasError.Create;
  except
    on BaseAlias.TBaseError do
      HandleBase;
    on OtherAlias.TChildError do
      HandleOther;
  end;
end;
end.
"#,
    );

    let imports = vec![
        import(
            "child",
            &child_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("base")),
            &["Base.Errors", "BaseAlias"],
        ),
        import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("child")),
            &["Child.Errors", "ChildAlias"],
        ),
        import(
            "consumer",
            &consumer_tree,
            1,
            ImportTarget::Loaded(ProjectUnitId::new("base")),
            &["Base.Errors", "BaseAlias"],
        ),
        import(
            "consumer",
            &consumer_tree,
            2,
            ImportTarget::Loaded(ProjectUnitId::new("other")),
            &["Other.Errors", "OtherAlias"],
        ),
    ];
    let snapshot = ProjectSnapshot::new(
        vec![other.clone(), consumer.clone(), child.clone(), base.clone()],
        imports.clone(),
    )
    .expect("valid cross-unit snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("consumer project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(
        cfg,
        &consumer_source,
        "raise",
        "raise ChildAlias.TAliasError.Create",
    );
    let successful = successful_raise_block(cfg, raise);
    let base_handler = block_with_stmt(cfg, &consumer_source, "statement", "HandleBase");
    let other_handler = block_with_stmt(cfg, &consumer_source, "statement", "HandleOther");

    assert_eq!(
        successors(cfg, successful),
        vec![(base_handler, EdgeKind::ExceptionThrow)],
        "the imported child must resolve through the child unit's alias and base unit's ancestry"
    );
    assert!(!can_reach(cfg, base_handler, other_handler));

    let reordered_snapshot = ProjectSnapshot::new(vec![base, child, other, consumer], imports)
        .expect("valid reordered cross-unit snapshot");
    let reordered_cfgs =
        build_file_cfgs_in_project(&reordered_snapshot, &ProjectUnitId::new("consumer"))
            .expect("reordered project build");
    assert_eq!(
        cfgs.iter().map(cfg_signature).collect::<Vec<_>>(),
        reordered_cfgs.iter().map(cfg_signature).collect::<Vec<_>>(),
        "stable unit IDs must make project builds independent of input order"
    );
}

#[test]
fn import_precedence_is_later_before_earlier_and_unavailable_high_priority_blocks_fallback() {
    let (low, _, _) = unit(
        "low",
        r#"
unit Low;
interface
type TError = class constructor Create; end;
implementation
end.
"#,
    );
    let (high, _, _) = unit(
        "high",
        r#"
unit High;
interface
type TError = class constructor Create; end;
implementation
end.
"#,
    );
    let (other, _, _) = unit(
        "other",
        r#"
unit Other;
interface
type TError = class constructor Create; end;
implementation
end.
"#,
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Low, Other, High;
implementation
procedure P;
begin
  try
    raise TError.Create;
  except
    on Low.TError do LowHandler;
    on High.TError do HighHandler;
    on Other.TError do OtherHandler;
  end;
end;
end.
"#,
    );
    let imports = vec![
        import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("low")),
            &["Low"],
        ),
        import(
            "consumer",
            &consumer_tree,
            1,
            ImportTarget::Loaded(ProjectUnitId::new("other")),
            &["Other"],
        ),
        import(
            "consumer",
            &consumer_tree,
            2,
            ImportTarget::Loaded(ProjectUnitId::new("high")),
            &["High"],
        ),
    ];
    let snapshot = ProjectSnapshot::new(vec![consumer, other, high, low.clone()], imports)
        .expect("valid precedence snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("precedence project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let successful = successful_raise_block(cfg, raise);
    let low_handler = block_with_stmt(cfg, &source, "statement", "LowHandler");
    let high_handler = block_with_stmt(cfg, &source, "statement", "HighHandler");
    let other_handler = block_with_stmt(cfg, &source, "statement", "OtherHandler");
    assert_eq!(
        successors(cfg, successful),
        vec![(high_handler, EdgeKind::ExceptionThrow)],
        "the later loaded uses entry must win for an unqualified type"
    );
    assert!(!successors(cfg, successful).contains(&(low_handler, EdgeKind::ExceptionThrow)));
    assert!(!successors(cfg, successful).contains(&(other_handler, EdgeKind::ExceptionThrow)));

    let (incomplete, _, _) = unit(
        "incomplete",
        r#"
unit Incomplete;
interface
{$IFDEF NOT_CONFIGURED}
{$ENDIF}
implementation
end.
"#,
    );
    let (unavailable_consumer, unavailable_tree, unavailable_source) = unit(
        "unavailable-consumer",
        r#"
unit UnavailableConsumer;
interface
uses Low, Incomplete, Missing;
implementation
procedure P;
begin
  try
    raise TError.Create;
  except
    on Low.TError do LowHandler;
    on Incomplete.TError do IncompleteHandler;
  end;
end;
end.
"#,
    );
    let unavailable_imports = vec![
        import(
            "unavailable-consumer",
            &unavailable_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("low")),
            &["Low"],
        ),
        import(
            "unavailable-consumer",
            &unavailable_tree,
            1,
            ImportTarget::Loaded(ProjectUnitId::new("incomplete")),
            &["Incomplete"],
        ),
        import(
            "unavailable-consumer",
            &unavailable_tree,
            2,
            ImportTarget::Unavailable,
            &["Missing"],
        ),
    ];
    let unavailable_snapshot = ProjectSnapshot::new(
        vec![unavailable_consumer, incomplete, low],
        unavailable_imports,
    )
    .expect("valid unavailable snapshot");
    let unavailable_cfgs = build_file_cfgs_in_project(
        &unavailable_snapshot,
        &ProjectUnitId::new("unavailable-consumer"),
    )
    .expect("unavailable project build");
    let unavailable_cfg = cfg_for(&unavailable_cfgs, "P");
    let unavailable_raise = block_with_stmt(
        unavailable_cfg,
        &unavailable_source,
        "raise",
        "raise TError.Create",
    );
    let unavailable_successful = successful_raise_block(unavailable_cfg, unavailable_raise);
    let unavailable_low = block_with_stmt(
        unavailable_cfg,
        &unavailable_source,
        "statement",
        "LowHandler",
    );
    let unavailable_incomplete = block_with_stmt(
        unavailable_cfg,
        &unavailable_source,
        "statement",
        "IncompleteHandler",
    );
    assert!(successors(unavailable_cfg, unavailable_successful)
        .contains(&(unavailable_low, EdgeKind::ExceptionThrow)));
    assert!(successors(unavailable_cfg, unavailable_successful)
        .contains(&(unavailable_incomplete, EdgeKind::ExceptionThrow)));
    assert!(successors(unavailable_cfg, unavailable_successful)
        .contains(&(unavailable_cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn loaded_but_unimported_units_are_not_visible() {
    let (visible, _, _) = unit(
        "visible",
        r#"
unit Visible;
interface
type TError = class constructor Create; end;
implementation
end.
"#,
    );
    let (hidden, _, _) = unit(
        "hidden",
        r#"
unit Hidden;
interface
type TError = class constructor Create; end;
implementation
end.
"#,
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Visible;
implementation
procedure P;
begin
  try
    raise Hidden.TError.Create;
  except
    on Hidden.TError do HiddenHandler;
    on Visible.TError do VisibleHandler;
  end;
end;
end.
"#,
    );
    let imports = vec![import(
        "consumer",
        &consumer_tree,
        0,
        ImportTarget::Loaded(ProjectUnitId::new("visible")),
        &["Visible"],
    )];
    let snapshot = ProjectSnapshot::new(vec![consumer, hidden, visible], imports)
        .expect("valid visibility snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("visibility project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise Hidden.TError.Create");
    let successful = successful_raise_block(cfg, raise);
    let hidden_handler = block_with_stmt(cfg, &source, "statement", "HiddenHandler");
    let visible_handler = block_with_stmt(cfg, &source, "statement", "VisibleHandler");

    assert!(successors(cfg, successful).contains(&(hidden_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful).contains(&(visible_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn imported_constructor_proof_requires_an_accessible_constructor_member() {
    let (definitions, _, _) = unit(
        "definitions",
        r#"
unit Definitions;
interface
type
  TPrivateError = class
  private
    constructor Create;
  end;
  TProtectedError = class
  protected
    constructor Create;
  end;
  TPublicError = class
  public
    constructor Create;
  end;
  TFunctionCreate = class
  public
    function Create: Integer;
  end;
implementation
end.
"#,
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Definitions;
implementation
procedure PrivateP;
begin
  try
    raise Definitions.TPrivateError.Create;
  except
    on Definitions.TPrivateError do PrivateHandler;
    on Definitions.TPublicError do PublicHandler;
  end;
end;
procedure PublicP;
begin
  try
    raise Definitions.TPublicError.Create;
  except
    on Definitions.TPrivateError do PrivateHandler;
    on Definitions.TPublicError do PublicHandler;
  end;
end;
procedure FunctionP;
begin
  try
    raise Definitions.TFunctionCreate.Create;
  except
    on Definitions.TFunctionCreate do FunctionHandler;
    on Definitions.TPublicError do PublicHandler;
  end;
end;
end.
"#,
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid constructor visibility snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("constructor visibility project build");

    let private_cfg = cfg_for(&cfgs, "PrivateP");
    let private_raise = block_with_stmt(
        private_cfg,
        &source,
        "raise",
        "raise Definitions.TPrivateError.Create",
    );
    let private_successful = successful_raise_block(private_cfg, private_raise);
    let private_handler = block_with_stmt(private_cfg, &source, "statement", "PrivateHandler");
    let public_handler = block_with_stmt(private_cfg, &source, "statement", "PublicHandler");
    assert!(successors(private_cfg, private_successful)
        .contains(&(private_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(private_cfg, private_successful)
        .contains(&(public_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(private_cfg, private_successful)
        .contains(&(private_cfg.exit, EdgeKind::ExceptionThrow)));

    let public_cfg = cfg_for(&cfgs, "PublicP");
    let public_raise = block_with_stmt(
        public_cfg,
        &source,
        "raise",
        "raise Definitions.TPublicError.Create",
    );
    let public_successful = successful_raise_block(public_cfg, public_raise);
    let public_handler = block_with_stmt(public_cfg, &source, "statement", "PublicHandler");
    let private_handler = block_with_stmt(public_cfg, &source, "statement", "PrivateHandler");
    assert_eq!(
        successors(public_cfg, public_successful),
        vec![(public_handler, EdgeKind::ExceptionThrow)]
    );
    assert!(!successors(public_cfg, public_successful)
        .contains(&(private_handler, EdgeKind::ExceptionThrow)));

    let function_cfg = cfg_for(&cfgs, "FunctionP");
    let function_raise = block_with_stmt(
        function_cfg,
        &source,
        "raise",
        "raise Definitions.TFunctionCreate.Create",
    );
    let function_successful = successful_raise_block(function_cfg, function_raise);
    let function_handler = block_with_stmt(function_cfg, &source, "statement", "FunctionHandler");
    let function_public = block_with_stmt(function_cfg, &source, "statement", "PublicHandler");
    assert!(successors(function_cfg, function_successful)
        .contains(&(function_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(function_cfg, function_successful)
        .contains(&(function_public, EdgeKind::ExceptionThrow)));
    assert!(successors(function_cfg, function_successful)
        .contains(&(function_cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn target_declaration_offsets_and_import_sections_not_caller_offsets() {
    let (definitions, _, _) = unit(
        "definitions",
        r#"
unit Definitions;
interface
type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TImplementationOnly)
    constructor Create;
  end;
implementation
type
  TImplementationOnly = TBaseError;
end.
"#,
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        &format!(
            "unit Consumer;\ninterface\nuses Definitions;\nimplementation\n{}procedure P;\nbegin\n  try\n    raise Definitions.TChildError.Create;\n  except\n    on Definitions.TBaseError do BaseHandler;\n  end;\nend;\nend.\n",
            "\n".repeat(2048)
        ),
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid offset snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("offset project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(
        cfg,
        &source,
        "raise",
        "raise Definitions.TChildError.Create",
    );
    let successful = successful_raise_block(cfg, raise);
    let handler = block_with_stmt(cfg, &source, "statement", "BaseHandler");

    assert!(successors(cfg, successful).contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn cyclic_cross_unit_ancestry_stays_unknown() {
    let (a, a_tree, _) = unit(
        "a",
        r#"
unit A;
interface
uses B;
type
  TA = class(TB)
    constructor Create;
  end;
implementation
end.
"#,
    );
    let (b, b_tree, _) = unit(
        "b",
        r#"
unit B;
interface
uses A;
type
  TB = class(TA)
    constructor Create;
  end;
implementation
end.
"#,
    );
    let (other, _, _) = unit(
        "other",
        r#"
unit Other;
interface
type TOther = class constructor Create; end;
implementation
end.
"#,
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses A, B, Other;
implementation
procedure P;
begin
  try
    raise A.TA.Create;
  except
    on Other.TOther do OtherHandler;
  end;
end;
end.
"#,
    );
    let imports = vec![
        import(
            "a",
            &a_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("b")),
            &["B"],
        ),
        import(
            "b",
            &b_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("a")),
            &["A"],
        ),
        import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("a")),
            &["A"],
        ),
        import(
            "consumer",
            &consumer_tree,
            1,
            ImportTarget::Loaded(ProjectUnitId::new("b")),
            &["B"],
        ),
        import(
            "consumer",
            &consumer_tree,
            2,
            ImportTarget::Loaded(ProjectUnitId::new("other")),
            &["Other"],
        ),
    ];
    let snapshot =
        ProjectSnapshot::new(vec![other, consumer, b, a], imports).expect("valid cyclic snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("cyclic project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise A.TA.Create");
    let successful = successful_raise_block(cfg, raise);
    let handler = block_with_stmt(cfg, &source, "statement", "OtherHandler");

    assert!(successors(cfg, successful).contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn imported_exception_facts_survive_finally_cleanup() {
    let (definitions, _, _) = unit(
        "definitions",
        r#"
unit Definitions;
interface
type
  TError = class constructor Create; end;
  TOtherError = class constructor Create; end;
implementation
end.
"#,
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Definitions;
implementation
procedure P;
begin
  try
    try
      raise Definitions.TError.Create;
    finally
      Cleanup;
    end;
  except
    on Definitions.TError do Handle;
    on Definitions.TOtherError do Other;
  end;
end;
end.
"#,
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid cleanup snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("cleanup project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise Definitions.TError.Create");
    let successful = successful_raise_block(cfg, raise);
    let cleanups = blocks_with_stmt(cfg, &source, "statement", "Cleanup");
    let handler = block_with_stmt(cfg, &source, "statement", "Handle");
    let other = block_with_stmt(cfg, &source, "statement", "Other");

    assert!(cleanups
        .iter()
        .any(|cleanup| can_reach(cfg, successful, *cleanup)));
    assert!(can_reach(cfg, successful, handler));
    // Cleanup statements may themselves throw, so an unresolved alternative
    // can still reach the second typed handler. The known imported fact must
    // not add a direct edge from the successful raise to that handler.
    assert!(!successors(cfg, successful).contains(&(other, EdgeKind::ExceptionThrow)));
}

#[test]
fn project_class_var_create_members_block_inherited_constructor_proof() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TOther = class constructor Create; end; TBase = class constructor Create; end; TError = class(TBase) public class var Create: TOther; end; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Definitions; implementation procedure P; begin try raise Definitions.TError.Create; except on Definitions.TError do HandleError; on Definitions.TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid class-var project snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("class-var project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise Definitions.TError.Create");
    let successful = successful_raise_block(cfg, raise);
    let other = block_with_stmt(cfg, &source, "statement", "HandleOther");

    assert!(successors(cfg, successful).contains(&(other, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn omitted_project_uses_binding_is_an_unknown_namespace() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TError = class constructor Create; end; TOther = class constructor Create; end; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Definitions, Missing; implementation procedure P; begin try raise TError.Create; except on Definitions.TError do HandleError; on Definitions.TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid omitted-binding snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("omitted-binding project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let successful = successful_raise_block(cfg, raise);

    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn unresolved_project_generic_method_owner_blocks_import_fallback() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TError = class constructor Create; end; TOther = class constructor Create; end; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Definitions; type THost<T> = class TError: TObject; procedure P; end; implementation procedure THost<T>.P; begin try raise TError.Create; except on Definitions.TError do HandleError; on Definitions.TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid unresolved-owner snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("unresolved-owner project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let successful = successful_raise_block(cfg, raise);

    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn unresolved_generic_method_owner_keeps_local_bindings_visible() {
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Definitions;
type
  THost<T> = class
    procedure P;
  end;
implementation

procedure THost<T>.P;
type
  TLocalError = class
    constructor Create;
  end;
begin
  try
    raise TLocalError.Create;
  except
    on TLocalError do
      HandleLocal;
  end;
end;

end.
"#,
    );
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TLocalError = class constructor Create; end; implementation end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid local-binding snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("local-binding project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise TLocalError.Create");
    let successful = successful_raise_block(cfg, raise);
    let local_handler = block_with_stmt(cfg, &source, "statement", "HandleLocal");

    assert_eq!(
        successors(cfg, successful),
        vec![(local_handler, EdgeKind::ExceptionThrow)]
    );
}

#[test]
fn imported_value_shadowing_a_qualified_head_blocks_unit_fallback() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TError = class constructor Create; end; TOther = class constructor Create; end; implementation end.",
    );
    let (shadow, _, _) = unit(
        "shadow",
        "unit Shadow; interface var Definitions: TObject; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Definitions, Shadow; implementation procedure P; begin try raise Definitions.TError.Create; except on TError do HandleError; on TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions, shadow],
        vec![
            import(
                "consumer",
                &consumer_tree,
                0,
                ImportTarget::Loaded(ProjectUnitId::new("definitions")),
                &["Definitions"],
            ),
            import(
                "consumer",
                &consumer_tree,
                1,
                ImportTarget::Loaded(ProjectUnitId::new("shadow")),
                &["Shadow"],
            ),
        ],
    )
    .expect("valid imported-shadow snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("imported-shadow project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise Definitions.TError.Create");
    let successful = successful_raise_block(cfg, raise);

    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn imported_private_nested_type_remains_inaccessible() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type THost = class private type TError = class public constructor Create; end; end; TOther = class constructor Create; end; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Definitions; implementation procedure P; begin try raise Definitions.THost.TError.Create; except on Definitions.THost.TError do HandleError; on Definitions.TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid private-nested-type snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("private-nested-type project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(
        cfg,
        &source,
        "raise",
        "raise Definitions.THost.TError.Create",
    );
    let successful = successful_raise_block(cfg, raise);

    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn incomplete_loaded_namespace_blocks_a_lower_priority_complete_import() {
    let (complete, _, _) = unit(
        "complete",
        "unit Complete; interface type TError = class constructor Create; end; implementation end.",
    );
    let (incomplete, _, _) = unit(
        "incomplete",
        "unit Incomplete; interface {$IFDEF NOT_CONFIGURED} type TError = class constructor Create; end; {$ENDIF} implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Complete, Incomplete; implementation procedure P; begin try raise TError.Create; except on Complete.TError do HandleComplete; on Incomplete.TError do HandleIncomplete; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, complete, incomplete],
        vec![
            import(
                "consumer",
                &consumer_tree,
                0,
                ImportTarget::Loaded(ProjectUnitId::new("complete")),
                &["Complete"],
            ),
            import(
                "consumer",
                &consumer_tree,
                1,
                ImportTarget::Loaded(ProjectUnitId::new("incomplete")),
                &["Incomplete"],
            ),
        ],
    )
    .expect("valid incomplete-namespace snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("incomplete-namespace project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let successful = successful_raise_block(cfg, raise);

    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn project_exact_and_subtype_reraises_survive_finally_cleanup() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TBaseError = class constructor Create; end; TChildError = class(TBaseError) constructor Create; end; TSiblingError = class(TBaseError) constructor Create; end; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        r#"
unit Consumer;
interface
uses Definitions;
implementation

procedure ExactReraise;
begin
  try
    try
      raise Definitions.TChildError.Create;
    finally
      begin
      end;
    end;
  except
    on Definitions.TSiblingError do
      HandleExactSibling;
    on Definitions.TBaseError do
      HandleExactBase;
  end;
end;

procedure MixedReraise;
begin
  try
    try
      try
        if ChooseKnown then
          raise Definitions.TChildError.Create
        else
          raise UnknownError;
      except
        on Definitions.TBaseError do
          raise;
      end;
    finally
      begin
      end;
    end;
  except
    on Definitions.TSiblingError do
      HandleMixedSibling;
    on Definitions.TBaseError do
      HandleMixedBase;
  end;
end;

end.
"#,
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid project-reraise snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("project-reraise build");

    let exact = cfg_for(&cfgs, "ExactReraise");
    let exact_raise = block_with_stmt(
        exact,
        &source,
        "raise",
        "raise Definitions.TChildError.Create",
    );
    let exact_successful = successful_raise_block(exact, exact_raise);
    let exact_sibling = block_with_stmt(exact, &source, "statement", "HandleExactSibling");
    let exact_base = block_with_stmt(exact, &source, "statement", "HandleExactBase");
    assert!(can_reach(exact, exact_successful, exact_base));
    assert!(!can_reach(exact, exact_successful, exact_sibling));
    assert!(exact.graph.node_indices().map(BlockId::from).any(|block| {
        exact.graph[block.index()].kind == BasicBlockKind::FinallyHandler
            && can_reach(exact, exact_successful, block)
    }));

    let mixed = cfg_for(&cfgs, "MixedReraise");
    let mixed_reraise = block_with_stmt(mixed, &source, "raise", "raise;");
    let mixed_sibling = block_with_stmt(mixed, &source, "statement", "HandleMixedSibling");
    let mixed_base = block_with_stmt(mixed, &source, "statement", "HandleMixedBase");
    assert!(can_reach(mixed, mixed_reraise, mixed_sibling));
    assert!(can_reach(mixed, mixed_reraise, mixed_base));
    assert!(mixed.graph.node_indices().map(BlockId::from).any(|block| {
        mixed.graph[block.index()].kind == BasicBlockKind::FinallyHandler
            && can_reach(mixed, mixed_reraise, block)
    }));
}

#[test]
fn matching_import_head_binding_blocks_qualified_unit_lookup() {
    let (definitions, _, _) = unit(
        "definitions",
        "unit Definitions; interface type TError = class constructor Create; end; TOther = class constructor Create; end; TOtherClass = class of TOther; TFactory = class public TError: TOtherClass; end; var Definitions: TFactory; implementation end.",
    );
    let (consumer, consumer_tree, source) = unit(
        "consumer",
        "unit Consumer; interface uses Definitions; implementation procedure P; begin try raise Definitions.TError.Create; except on TError do HandleError; on TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(
        vec![consumer, definitions],
        vec![import(
            "consumer",
            &consumer_tree,
            0,
            ImportTarget::Loaded(ProjectUnitId::new("definitions")),
            &["Definitions"],
        )],
    )
    .expect("valid matching-head snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("matching-head project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise Definitions.TError.Create");
    let successful = successful_raise_block(cfg, raise);

    assert!(successors(cfg, successful).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn same_unit_protected_constructor_remains_precise_in_project() {
    let (consumer, _, source) = unit(
        "consumer",
        "unit Consumer; interface type TError = class protected constructor Create; end; TOther = class constructor Create; end; implementation procedure P; begin try raise TError.Create; except on TError do HandleError; on TOther do HandleOther; end; end; end.",
    );
    let snapshot = ProjectSnapshot::new(vec![consumer], Vec::new())
        .expect("valid same-unit protected snapshot");
    let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("consumer"))
        .expect("same-unit protected project build");
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let successful = successful_raise_block(cfg, raise);
    let error = block_with_stmt(cfg, &source, "statement", "HandleError");

    assert_eq!(
        successors(cfg, successful),
        vec![(error, EdgeKind::ExceptionThrow)]
    );
}
