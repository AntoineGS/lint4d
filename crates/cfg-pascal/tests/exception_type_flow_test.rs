use std::collections::HashSet;

use cfg_core::{BlockId, Cfg, EdgeKind};
use cfg_pascal::build_file_cfgs;
use tree_sitter::{Parser, Tree};

fn parse_clean(source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&cfg_pascal::LANGUAGE.into())
        .expect("failed to set Pascal language");
    let tree = parser.parse(source, None).expect("parser returned no tree");
    assert!(
        !tree.root_node().has_error(),
        "exception-type fixture must not contain parser errors:\n{}",
        tree.root_node().to_sexp()
    );
    tree
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
        .iter()
        .find_map(|(target, kind)| {
            (*kind == EdgeKind::Normal && cfg.graph[target.index()].stmts.is_empty())
                .then_some(*target)
        })
        .expect("constructor raise must have a synthetic successful-raise block")
}

fn can_reach(cfg: &Cfg, from: BlockId, to: BlockId) -> bool {
    let mut pending = vec![from];
    let mut visited = HashSet::new();

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

#[test]
fn known_constructor_alias_and_inheritance_use_first_matching_handler() {
    let source = br#"
unit KnownExceptionDispatch;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TAliasError = TChildError;

procedure KnownConstructor;
begin
  try
    raise TAliasError.Create;
  except
    on E: TBaseError do
      HandleBase;
    on E: TChildError do
      HandleChild;
  else
    HandleElse;
  end;
  AfterKnown;
end;

procedure QualifiedConstructor;
begin
  try
    raise KnownExceptionDispatch.TAliasError.Create;
  except
    on E: KnownExceptionDispatch.TBaseError do
      HandleQualified;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = cfg_for(&cfgs, "KnownConstructor");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise TAliasError.Create");
    let base = block_with_stmt(cfg, &source, "statement", "HandleBase");
    let child = block_with_stmt(cfg, &source, "statement", "HandleChild");
    let fallback = block_with_stmt(cfg, &source, "statement", "HandleElse");
    let known_successors = successors(cfg, raise_stmt);
    let successful_raise = successful_raise_block(cfg, raise_stmt);
    assert_eq!(
        successors(cfg, successful_raise),
        vec![(base, EdgeKind::ExceptionThrow)],
        "a known child raised through an alias must stop at the first matching ancestor handler"
    );
    assert!(known_successors.contains(&(child, EdgeKind::ExceptionThrow)));
    assert!(known_successors.contains(&(fallback, EdgeKind::ExceptionThrow)));
    assert!(!can_reach(cfg, base, child));
    assert!(!can_reach(cfg, base, fallback));

    let qualified_cfg = cfg_for(&cfgs, "QualifiedConstructor");
    let qualified_raise = block_with_stmt(
        qualified_cfg,
        &source,
        "raise",
        "raise KnownExceptionDispatch.TAliasError.Create",
    );
    let qualified_handler = block_with_stmt(qualified_cfg, &source, "statement", "HandleQualified");
    let qualified_successors = successors(qualified_cfg, qualified_raise);
    let qualified_successful_raise = successful_raise_block(qualified_cfg, qualified_raise);
    assert_eq!(
        successors(qualified_cfg, qualified_successful_raise),
        vec![(qualified_handler, EdgeKind::ExceptionThrow)],
        "a same-module-qualified constructor must resolve to the local class"
    );
    assert!(qualified_successors.contains(&(qualified_handler, EdgeKind::ExceptionThrow)));
    assert!(qualified_successors.contains(&(qualified_cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn values_metaclasses_and_classes_without_constructors_remain_unknown() {
    let source = br#"
unit UnknownExceptionSources;
interface
implementation

type
  TKnownError = class
    constructor Create;
  end;
  TNoConstructor = class
  end;
  TMetaError = class of TKnownError;

procedure ValueReceiver;
var
  Instance: TKnownError;
begin
  try
    raise Instance.Create;
  except
    on E: TKnownError do
      HandleValueKnown;
    on E: TNoConstructor do
      HandleValueOther;
  end;
end;

procedure MissingConstructor;
begin
  try
    raise TNoConstructor.Create;
  except
    on E: TKnownError do
      HandleMissingKnown;
    on E: TNoConstructor do
      HandleMissingOther;
  end;
end;

procedure MetaclassReceiver;
begin
  try
    raise TMetaError.Create;
  except
    on E: TKnownError do
      HandleMetaKnown;
    on E: TNoConstructor do
      HandleMetaOther;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    for (procedure, raise_text, first_handler, second_handler) in [
        (
            "ValueReceiver",
            "raise Instance.Create",
            "HandleValueKnown",
            "HandleValueOther",
        ),
        (
            "MissingConstructor",
            "raise TNoConstructor.Create",
            "HandleMissingKnown",
            "HandleMissingOther",
        ),
        (
            "MetaclassReceiver",
            "raise TMetaError.Create",
            "HandleMetaKnown",
            "HandleMetaOther",
        ),
    ] {
        let cfg = cfg_for(&cfgs, procedure);
        let raise_stmt = block_with_stmt(cfg, &source, "raise", raise_text);
        let first = block_with_stmt(cfg, &source, "statement", first_handler);
        let second = block_with_stmt(cfg, &source, "statement", second_handler);
        let dispatch_block = successors(cfg, raise_stmt)
            .into_iter()
            .find_map(|(target, kind)| {
                (kind == EdgeKind::Normal && cfg.graph[target.index()].stmts.is_empty())
                    .then_some(target)
            })
            .unwrap_or(raise_stmt);
        let raise_successors = successors(cfg, dispatch_block);
        assert!(
            raise_successors.contains(&(first, EdgeKind::ExceptionThrow)),
            "{procedure} must conservatively retain its first typed handler on every path"
        );
        assert!(
            raise_successors.contains(&(second, EdgeKind::ExceptionThrow)),
            "{procedure} must conservatively retain later typed handlers on every path"
        );
        assert!(
            raise_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)),
            "{procedure} must retain the unmatched outward exception path"
        );
    }
}

#[test]
fn constructor_evaluation_has_a_separate_unknown_path_without_duplicate_source_metadata() {
    let source = br#"
unit ConstructorEvaluation;
interface
implementation

type
  TKnownError = class
    constructor Create;
  end;
  TOtherError = class
    constructor Create;
  end;

procedure ConstructorEvaluation;
begin
  try
    raise TKnownError.Create(ThrowingValue());
  except
    on E: TKnownError do
      HandleKnown;
    on E: TOtherError do
      HandleOther;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ConstructorEvaluation");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise TKnownError.Create");
    let known = block_with_stmt(cfg, &source, "statement", "HandleKnown");
    let other = block_with_stmt(cfg, &source, "statement", "HandleOther");

    let synthetic_success = successors(cfg, raise_stmt)
        .iter()
        .find_map(|(target, kind)| {
            (*kind == EdgeKind::Normal && cfg.graph[target.index()].stmts.is_empty())
                .then_some(*target)
        })
        .expect("constructor evaluation must flow to a synthetic successful-raise block");
    assert_eq!(
        successors(cfg, synthetic_success),
        vec![(known, EdgeKind::ExceptionThrow)],
        "the synthetic successful raise must retain only its proven handler"
    );
    assert!(successors(cfg, raise_stmt).contains(&(other, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, raise_stmt).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
    assert_eq!(
        cfg.graph[synthetic_success.index()].stmts.len(),
        0,
        "the synthetic block must not duplicate the source raise StmtRef"
    );
}

#[test]
fn lexical_scopes_and_handler_order_preserve_only_proven_matches() {
    let source = br#"
unit ExceptionTypeScopes;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
  end;
  TSiblingError = class(TBaseError)
    constructor Create;
  end;
  TClassFunction = class
    function Create: Integer;
  end;

procedure LocalAlias;
type
  TLocalAlias = TChildError;
begin
  try
    raise TLocalAlias.Create;
  except
    on E: TSiblingError do
      HandleSibling;
    on E: TBaseError do
      HandleBase;
  end;
end;

procedure ShadowedValue(TBaseError: Integer);
begin
  try
    raise TBaseError.Create;
  except
    on E: TBaseError do
      HandleShadowed;
  end;
end;

procedure UnknownEarly;
begin
  try
    raise TChildError.Create;
  except
    on E: ImportedError do
      HandleUnknown;
    on E: TBaseError do
      HandleKnown;
  end;
end;

procedure ClassFunction;
begin
  try
    raise TClassFunction.Create;
  except
    on E: TBaseError do
      HandleClassFunction;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let local_cfg = cfg_for(&cfgs, "LocalAlias");
    let local_raise = block_with_stmt(local_cfg, &source, "raise", "raise TLocalAlias.Create");
    let sibling = block_with_stmt(local_cfg, &source, "statement", "HandleSibling");
    let base = block_with_stmt(local_cfg, &source, "statement", "HandleBase");
    let local_successful_raise = successful_raise_block(local_cfg, local_raise);
    assert_eq!(
        successors(local_cfg, local_successful_raise),
        vec![(base, EdgeKind::ExceptionThrow)],
        "a local alias of a child must skip an unrelated sibling and stop at its base"
    );
    assert!(successors(local_cfg, local_raise).contains(&(sibling, EdgeKind::ExceptionThrow)));
    assert!(successors(local_cfg, local_raise).contains(&(base, EdgeKind::ExceptionThrow)));
    assert!(
        successors(local_cfg, local_raise).contains(&(local_cfg.exit, EdgeKind::ExceptionThrow))
    );
    assert!(!can_reach(local_cfg, base, sibling));

    let shadowed_cfg = cfg_for(&cfgs, "ShadowedValue");
    let shadowed_raise = block_with_stmt(shadowed_cfg, &source, "raise", "raise TBaseError.Create");
    let shadowed_handler = block_with_stmt(shadowed_cfg, &source, "statement", "HandleShadowed");
    assert!(successors(shadowed_cfg, shadowed_raise)
        .contains(&(shadowed_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(shadowed_cfg, shadowed_raise)
        .contains(&(shadowed_cfg.exit, EdgeKind::ExceptionThrow)));

    let unknown_cfg = cfg_for(&cfgs, "UnknownEarly");
    let unknown_raise = block_with_stmt(unknown_cfg, &source, "raise", "raise TChildError.Create");
    let unknown_handler = block_with_stmt(unknown_cfg, &source, "statement", "HandleUnknown");
    let known_handler = block_with_stmt(unknown_cfg, &source, "statement", "HandleKnown");
    let unknown_successful_raise = successful_raise_block(unknown_cfg, unknown_raise);
    assert_eq!(
        successors(unknown_cfg, unknown_successful_raise),
        vec![
            (unknown_handler, EdgeKind::ExceptionThrow),
            (known_handler, EdgeKind::ExceptionThrow),
        ],
        "the successful constructor path must retain the unknown early handler and then stop at the proven base handler"
    );
    let unknown_raise_successors = successors(unknown_cfg, unknown_raise);
    assert!(unknown_raise_successors.contains(&(unknown_handler, EdgeKind::ExceptionThrow)));
    assert!(unknown_raise_successors.contains(&(known_handler, EdgeKind::ExceptionThrow)));
    assert!(unknown_raise_successors.contains(&(unknown_cfg.exit, EdgeKind::ExceptionThrow)));
    assert!(
        unknown_raise_successors
            .iter()
            .any(|(target, kind)| *target != unknown_successful_raise
                && *kind == EdgeKind::ExceptionThrow),
        "an unknown early handler must remain an alternative before the proven base handler"
    );

    let class_function_cfg = cfg_for(&cfgs, "ClassFunction");
    let class_function_raise = block_with_stmt(
        class_function_cfg,
        &source,
        "raise",
        "raise TClassFunction.Create",
    );
    let class_function_handler = block_with_stmt(
        class_function_cfg,
        &source,
        "statement",
        "HandleClassFunction",
    );
    assert!(successors(class_function_cfg, class_function_raise)
        .contains(&(class_function_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(class_function_cfg, class_function_raise)
        .contains(&(class_function_cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn handler_variables_shadow_type_names_inside_handler_bodies() {
    let source = br#"
unit HandlerVariableScope;
interface
implementation

type
  TKnownError = class
    constructor Create;
  end;

procedure HandlerVariableScope;
begin
  try
    try
      raise TKnownError.Create;
    except
      on TKnownError: TKnownError do
        raise TKnownError.Create;
    end;
  except
    on TKnownError do
      HandleOuterKnown;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "HandlerVariableScope");
    let mut raises = blocks_with_stmt(cfg, &source, "raise", "raise TKnownError.Create");
    let outer_handler = block_with_stmt(cfg, &source, "statement", "HandleOuterKnown");

    assert_eq!(raises.len(), 2);
    raises.sort_by_key(|block| {
        cfg.graph[block.index()]
            .stmts
            .iter()
            .map(|stmt| stmt.byte_range.start)
            .min()
            .expect("raise statement source span")
    });
    assert_eq!(
        successors(cfg, successful_raise_block(cfg, raises[0])),
        vec![(raises[1], EdgeKind::ExceptionThrow)],
        "the initial known raise must enter the inner handler body"
    );
    assert!(successors(cfg, raises[0]).contains(&(outer_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, raises[0]).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, raises[1]).contains(&(outer_handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, raises[1]).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn non_constructor_create_members_do_not_inherit_constructor_facts() {
    let source = br#"
unit CreateMemberShadow;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TShadowedCreate = class(TBaseError)
    function Create: Integer;
  end;

procedure CreateMemberShadow;
begin
  try
    raise TShadowedCreate.Create;
  except
    on E: TBaseError do
      HandleBase;
    on E: TShadowedCreate do
      HandleShadowed;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "CreateMemberShadow");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise TShadowedCreate.Create");
    let base = block_with_stmt(cfg, &source, "statement", "HandleBase");
    let shadowed = block_with_stmt(cfg, &source, "statement", "HandleShadowed");
    let successful_raise = successful_raise_block(cfg, raise_stmt);

    assert!(successors(cfg, raise_stmt).contains(&(base, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, raise_stmt).contains(&(shadowed, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, raise_stmt).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful_raise).contains(&(base, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful_raise).contains(&(shadowed, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful_raise).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn omitted_parentheses_constructor_keeps_an_unknown_evaluation_path() {
    let source = br#"
unit OmittedConstructorEvaluation;
interface
implementation

type
  TKnownError = class
    constructor Create;
  end;
  TOtherError = class
    constructor Create;
  end;

procedure OmittedConstructorEvaluation;
begin
  try
    raise TKnownError.Create;
  except
    on E: TKnownError do
      HandleKnown;
    on E: TOtherError do
      HandleOther;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "OmittedConstructorEvaluation");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise TKnownError.Create");
    let known = block_with_stmt(cfg, &source, "statement", "HandleKnown");
    let other = block_with_stmt(cfg, &source, "statement", "HandleOther");
    let raise_successors = successors(cfg, raise_stmt);
    let synthetic_success = raise_successors
        .iter()
        .find_map(|(target, kind)| {
            (*kind == EdgeKind::Normal && cfg.graph[target.index()].stmts.is_empty())
                .then_some(*target)
        })
        .expect("omitted constructor evaluation must have a synthetic successful-raise block");

    assert_eq!(
        successors(cfg, synthetic_success),
        vec![(known, EdgeKind::ExceptionThrow)]
    );
    assert!(raise_successors.contains(&(other, EdgeKind::ExceptionThrow)));
    assert!(raise_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn class_var_create_members_do_not_inherit_constructor_facts() {
    let source = br#"
unit ClassVarCreateMember;
interface
implementation

type
  TOther = class
    constructor Create;
  end;
  TBase = class
    constructor Create;
  end;
  TError = class(TBase)
  public
    class var Create: TOther;
  end;

procedure ClassVarCreateMember;
begin
  try
    raise TError.Create;
  except
    on E: TError do
      HandleError;
    on E: TOther do
      HandleOther;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ClassVarCreateMember");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let other = block_with_stmt(cfg, &source, "statement", "HandleOther");
    let successful_raise = successful_raise_block(cfg, raise_stmt);

    assert!(successors(cfg, successful_raise).contains(&(other, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, successful_raise).contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn same_unit_private_constructor_remains_precise() {
    let source = br#"
unit SameUnitPrivateConstructor;
interface
implementation

type
  TError = class
  private
    constructor Create;
  end;
  TOther = class
    constructor Create;
  end;

procedure SameUnitPrivateConstructor;
begin
  try
    raise TError.Create;
  except
    on E: TError do
      HandleError;
    on E: TOther do
      HandleOther;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "SameUnitPrivateConstructor");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise TError.Create");
    let error = block_with_stmt(cfg, &source, "statement", "HandleError");
    let successful_raise = successful_raise_block(cfg, raise_stmt);

    assert_eq!(
        successors(cfg, successful_raise),
        vec![(error, EdgeKind::ExceptionThrow)]
    );
}

#[test]
fn nested_alias_uses_its_declaration_offset() {
    let source = br#"
unit NestedAliasDeclarationOffset;
interface
implementation

type
  TError = class
    constructor Create;
  end;
  TOther = class
    constructor Create;
  end;
  THost = class
  public
    type
      TAlias = TError;
      TError = TOther;
  end;

procedure NestedAliasOffsetProc;
begin
  try
    raise THost.TAlias.Create;
  except
    on E: NestedAliasDeclarationOffset.TError do
      HandleError;
    on E: NestedAliasDeclarationOffset.TOther do
      HandleOther;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NestedAliasOffsetProc");
    let raise_stmt = block_with_stmt(cfg, &source, "raise", "raise THost.TAlias.Create");
    let error = block_with_stmt(cfg, &source, "statement", "HandleError");
    let successful_raise = successful_raise_block(cfg, raise_stmt);

    assert_eq!(
        successors(cfg, successful_raise),
        vec![(error, EdgeKind::ExceptionThrow)]
    );
}
