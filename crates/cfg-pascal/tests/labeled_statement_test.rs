use std::collections::HashSet;

use cfg_core::{BlockId, Cfg, EdgeKind};
use cfg_pascal::build_file_cfgs;
use tree_sitter::{Parser, Tree};

fn parse(source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&cfg_pascal::LANGUAGE.into())
        .expect("failed to set Pascal language");
    parser.parse(source, None).expect("parser returned no tree")
}

fn parse_clean(source: &[u8]) -> Tree {
    let tree = parse(source);
    assert!(
        !tree.root_node().has_error(),
        "labeled-statement fixture must not contain parser errors:\n{}",
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
    cfg.graph
        .node_indices()
        .find_map(|index| {
            let block = &cfg.graph[index];
            block.stmts.iter().find_map(|stmt| {
                let stmt_text = std::str::from_utf8(&source[stmt.byte_range.clone()]).ok()?;
                (stmt.node_kind == kind && stmt_text.contains(text)).then(|| BlockId::from(index))
            })
        })
        .unwrap_or_else(|| panic!("statement {kind:?} containing {text:?} not found"))
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
fn named_and_numeric_labels_parse_as_single_statement_prefixes() {
    for source in [
        b"program NumericLabel; label 1; begin if B then 1: Work; end.".as_slice(),
        b"program NamedLabel; label WorkLabel; begin if B then WorkLabel: Work; end.".as_slice(),
    ] {
        parse_clean(source);
    }
}

#[test]
fn missing_statement_separators_remain_parse_errors() {
    for source in [
        b"program MissingSeparator; begin Work L: end.".as_slice(),
        b"program MissingGotoSeparator; begin goto L L: end.".as_slice(),
        b"program MissingRepeatSeparator; begin repeat Work L: until B end.".as_slice(),
        b"program MissingTrySeparator; begin try Work L: finally Cleanup end end.".as_slice(),
    ] {
        let tree = parse(source);
        assert!(
            tree.root_node().has_error(),
            "missing statement separator must be rejected for {:?}:\n{}",
            std::str::from_utf8(source).expect("fixture is UTF-8"),
            tree.root_node().to_sexp()
        );
    }
}

#[test]
fn valid_statement_separators_and_label_sequences_remain_parse_clean() {
    for source in [
        b"program ValidBlock; begin Work; L: end.".as_slice(),
        b"program ValidRepeat; begin repeat Work; L: until B end.".as_slice(),
        b"program ValidTry; begin try Work; L: finally Cleanup end end.".as_slice(),
        b"program ValidEmptyLabel; begin Work1: end.".as_slice(),
        b"program ValidConsecutiveLabels; begin L1: L2: Work; end.".as_slice(),
    ] {
        parse_clean(source);
    }
}

#[test]
fn forward_gotos_reach_labeled_bodies_inside_structured_statements() {
    let source = br#"
program LabeledTargets;
label 10, 20, IfTarget, WhileTarget, ForTarget, WithTarget, CaseTarget;
begin
  goto 10;
  UnreachableBeforeIf;
  if BranchCondition then
    0010: IfTarget: IfBody
  else
    IfElseBody;

  goto WhileTarget;
  UnreachableBeforeWhile;
  while LoopCondition do
    WhileTarget: WhileBody;

  goto 20;
  UnreachableBeforeFor;
  for I := 0 to 1 do
    20: ForTarget: ForBody;

  goto WithTarget;
  UnreachableBeforeWith;
  with Context do
    WithTarget: WithBody;

  goto CaseTarget;
  UnreachableBeforeCase;
  case Choice of
    1: CaseTarget: CaseBody;
  end;

  After;
end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "LabeledTargets.<main>");

    for (goto_text, body_text, unreachable_text) in [
        ("goto 10", "IfBody", "UnreachableBeforeIf"),
        ("goto WhileTarget", "WhileBody", "UnreachableBeforeWhile"),
        ("goto 20", "ForBody", "UnreachableBeforeFor"),
        ("goto WithTarget", "WithBody", "UnreachableBeforeWith"),
        ("goto CaseTarget", "CaseBody", "UnreachableBeforeCase"),
    ] {
        let goto = block_with_stmt(cfg, &source, "goto", goto_text);
        let body = block_with_stmt(cfg, &source, "statement", body_text);
        let unreachable = block_with_stmt(cfg, &source, "statement", unreachable_text);

        assert_eq!(
            successors(cfg, goto),
            vec![(body, EdgeKind::Goto)],
            "forward goto {goto_text:?} must target its labeled single-statement body"
        );
        assert!(
            !can_reach(cfg, goto, unreachable),
            "a forward goto must not fall through to {unreachable_text:?}"
        );
    }
}

#[test]
fn trailing_labeled_empty_statement_is_a_normal_fallthrough_target() {
    let source = br#"
program EmptyLabeledTarget;
label 0001;
begin
  goto 1;
  0001:
end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "EmptyLabeledTarget.<main>");
    let goto = block_with_stmt(cfg, &source, "goto", "goto 1");
    let target = block_with_stmt(cfg, &source, "label", "0001:");

    assert_eq!(
        successors(cfg, goto),
        vec![(target, EdgeKind::Goto)],
        "a goto must resolve to a trailing empty label"
    );
    assert_eq!(
        successors(cfg, target),
        vec![(cfg.exit, EdgeKind::Normal)],
        "a trailing empty label must fall through directly to the scope exit"
    );
    assert!(
        !successors(cfg, target)
            .iter()
            .any(|(_, kind)| *kind == EdgeKind::ExceptionThrow),
        "a trailing empty label must not invent an executable exception path"
    );
}
