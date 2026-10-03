use cfg_core::{BasicBlockKind, BlockId, Cfg, EdgeKind};
use cfg_pascal::build_file_cfgs;
use std::collections::HashSet;
use tree_sitter::{Parser, Tree};

fn parse_clean(source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&cfg_pascal::LANGUAGE.into())
        .expect("failed to set Pascal language");
    let tree = parser.parse(source, None).expect("parser returned no tree");
    assert!(
        !tree.root_node().has_error(),
        "exception-reraise fixture must not contain parser errors:\n{}",
        tree.root_node().to_sexp()
    );
    tree
}

fn cfg_for<'a>(cfgs: &'a [Cfg], name: &str) -> &'a Cfg {
    cfgs.iter()
        .find(|cfg| cfg.proc_name == name)
        .unwrap_or_else(|| panic!("CFG for {name:?} not found"))
}

fn block_with_exact_stmt(cfg: &Cfg, source: &[u8], kind: &str, expected: &str) -> BlockId {
    let blocks = blocks_with_exact_stmt(cfg, source, kind, expected);
    assert_eq!(
        blocks.len(),
        1,
        "expected one {kind:?} statement equal to {expected:?}, got {blocks:?}"
    );
    blocks[0]
}

fn blocks_with_exact_stmt(cfg: &Cfg, source: &[u8], kind: &str, expected: &str) -> Vec<BlockId> {
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
                            .is_ok_and(|text| text.trim() == expected)
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
fn typed_handler_reraise_retains_a_conservative_bound_after_constructor_evaluation() {
    let source = br#"
unit TypedReraise;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TSiblingError = class(TBaseError)
    constructor Create;
  end;

procedure TypedReraise;
begin
  try
    try
      raise TChildError.Create;
    except
      on E: TBaseError do
        raise;
    end;
  except
    on E: TSiblingError do
      HandleSibling;
    on E: TBaseError do
      HandleBase;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "TypedReraise");
    let reraise = block_with_exact_stmt(cfg, &source, "raise", "raise;");
    let sibling = block_with_exact_stmt(cfg, &source, "statement", "HandleSibling;");
    let base = block_with_exact_stmt(cfg, &source, "statement", "HandleBase;");

    assert_eq!(
        successors(cfg, reraise),
        vec![
            (sibling, EdgeKind::ExceptionThrow),
            (base, EdgeKind::ExceptionThrow),
        ],
        "constructor evaluation makes the typed handler's re-raised fact conservative"
    );
    assert_ne!(sibling, base);
}

#[test]
fn typed_handler_reraise_uses_a_subtype_bound_for_mixed_inputs() {
    let source = br#"
unit MixedReraise;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TSiblingError = class(TBaseError)
    constructor Create;
  end;

procedure MixedReraise;
begin
  try
    try
      if ChooseKnown then
        raise TChildError.Create
      else
        raise UnknownError;
    except
      on E: TBaseError do
        raise;
    end;
  except
    on E: TSiblingError do
      HandleSibling;
    on E: TBaseError do
      HandleBase;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "MixedReraise");
    let reraise = block_with_exact_stmt(cfg, &source, "raise", "raise;");
    let sibling = block_with_exact_stmt(cfg, &source, "statement", "HandleSibling;");
    let base = block_with_exact_stmt(cfg, &source, "statement", "HandleBase;");

    assert_eq!(
        successors(cfg, reraise),
        vec![
            (sibling, EdgeKind::ExceptionThrow),
            (base, EdgeKind::ExceptionThrow),
        ],
        "mixed inputs must retain the typed handler's conservative subtype bound"
    );
}

#[test]
fn plain_except_reraise_remains_unknown() {
    let source = br#"
unit PlainReraise;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TSiblingError = class(TBaseError)
    constructor Create;
  end;

procedure PlainReraise;
begin
  try
    try
      raise TChildError.Create;
    except
      raise;
    end;
  except
    on E: TSiblingError do
      HandleSibling;
    on E: TBaseError do
      HandleBase;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "PlainReraise");
    let reraise = block_with_exact_stmt(cfg, &source, "raise", "raise;");
    let sibling = block_with_exact_stmt(cfg, &source, "statement", "HandleSibling;");
    let base = block_with_exact_stmt(cfg, &source, "statement", "HandleBase;");

    assert_eq!(
        successors(cfg, reraise),
        vec![
            (sibling, EdgeKind::ExceptionThrow),
            (base, EdgeKind::ExceptionThrow),
            (cfg.exit, EdgeKind::ExceptionThrow),
        ],
        "a bare except must not make a re-raised exception look typed"
    );
}

#[test]
fn nested_handler_contexts_restore_after_inner_reraise() {
    let source = br#"
unit NestedReraise;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TSiblingError = class(TBaseError)
    constructor Create;
  end;

procedure NestedReraise;
begin
  try
    try
      raise TChildError.Create;
    except
      on E: TBaseError do
      begin
        try
          if ThrowNested then
            raise TSiblingError.Create;
          NestedWork;
        except
          on E: TSiblingError do
            raise;
        end;
        raise;
      end;
    end;
  except
    on E: TSiblingError do
      HandleSibling;
    on E: TBaseError do
      HandleBase;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NestedReraise");
    let mut reraises = blocks_with_exact_stmt(cfg, &source, "raise", "raise;");
    let sibling = block_with_exact_stmt(cfg, &source, "statement", "HandleSibling;");
    let base = block_with_exact_stmt(cfg, &source, "statement", "HandleBase;");

    assert_eq!(reraises.len(), 2);
    reraises.sort_by_key(|block| {
        cfg.graph[block.index()]
            .stmts
            .iter()
            .map(|stmt| stmt.byte_range.start)
            .min()
            .expect("raise statement source span")
    });
    assert_eq!(
        successors(cfg, reraises[0]),
        vec![(sibling, EdgeKind::ExceptionThrow)],
        "the nested handler must re-raise using its own sibling subtype context"
    );
    assert_eq!(
        successors(cfg, reraises[1]),
        vec![
            (sibling, EdgeKind::ExceptionThrow),
            (base, EdgeKind::ExceptionThrow),
        ],
        "the outer handler context must be restored after the nested handler"
    );
}

#[test]
fn exceptions_raised_in_handlers_route_outward_not_to_later_siblings() {
    let source = br#"
unit OutwardHandlerRaise;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;

procedure OutwardHandlerRaise;
begin
  try
    try
      raise TChildError.Create;
    except
      on E: TBaseError do
        raise TBaseError.Create;
      on E: TChildError do
        HandleInnerChild;
    end;
  except
    on E: TBaseError do
      HandleOuterBase;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "OutwardHandlerRaise");
    let initial_raise = block_with_exact_stmt(cfg, &source, "raise", "raise TChildError.Create;");
    let handler_raise = block_with_exact_stmt(cfg, &source, "raise", "raise TBaseError.Create;");
    let inner_child = block_with_exact_stmt(cfg, &source, "statement", "HandleInnerChild;");
    let outer_base = block_with_exact_stmt(cfg, &source, "statement", "HandleOuterBase;");

    assert!(!successors(cfg, successful_raise_block(cfg, initial_raise))
        .contains(&(inner_child, EdgeKind::ExceptionThrow)));
    let handler_success = successful_raise_block(cfg, handler_raise);
    assert_eq!(
        successors(cfg, handler_success),
        vec![(outer_base, EdgeKind::ExceptionThrow)],
        "an exception from a handler must bypass later handlers in the same except"
    );
}

#[test]
fn typed_reraise_preserves_its_fact_through_finally_cleanup() {
    let source = br#"
unit ReraiseFinally;
interface
implementation

type
  TBaseError = class
    constructor Create;
  end;
  TChildError = class(TBaseError)
    constructor Create;
  end;
  TSiblingError = class(TBaseError)
    constructor Create;
  end;

procedure ReraiseFinally;
begin
  try
    try
      try
        raise TChildError.Create;
      except
        on E: TBaseError do
          raise;
      end;
    finally
      begin
      end;
    end;
  except
    on E: TSiblingError do
      HandleSibling;
    on E: TBaseError do
      HandleBase;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ReraiseFinally");
    let base = block_with_exact_stmt(cfg, &source, "statement", "HandleBase;");

    assert!(
        cfg.graph.node_indices().map(BlockId::from).any(|block| {
            cfg.graph[block.index()].kind == BasicBlockKind::FinallyHandler
                && can_reach(cfg, block, base)
        }),
        "the re-raised exception must survive finally cleanup to the outer base handler"
    );
}
