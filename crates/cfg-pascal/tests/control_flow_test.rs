use std::collections::HashSet;

use cfg_core::{BlockId, Cfg, EdgeKind};
use cfg_pascal::build_file_cfgs;
use tree_sitter::{Parser, Tree};

fn parse_clean(source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    let language = tree_sitter_pascal::LANGUAGE;
    parser
        .set_language(&language.into())
        .expect("failed to set Pascal language");

    let tree = parser.parse(source, None).expect("parser returned no tree");
    assert!(
        !tree.root_node().has_error(),
        "regression fixture must not contain parser errors:\n{}",
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

fn block_of_kind(cfg: &Cfg, kind: cfg_core::BasicBlockKind) -> BlockId {
    cfg.graph
        .node_indices()
        .find_map(|index| (cfg.graph[index].kind == kind).then(|| BlockId::from(index)))
        .unwrap_or_else(|| panic!("block of kind {kind:?} not found"))
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

fn target_of(cfg: &Cfg, from: BlockId, edge_kind: EdgeKind) -> BlockId {
    let targets: Vec<BlockId> = successors(cfg, from)
        .into_iter()
        .filter_map(|(target, kind)| (kind == edge_kind).then_some(target))
        .collect();
    assert_eq!(
        targets.len(),
        1,
        "expected one {edge_kind:?} successor from {from:?}, got {targets:?}"
    );
    targets[0]
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
fn branch_controls_route_to_the_active_loop_targets() {
    let source = br#"
unit ControlFlow;
interface
implementation

procedure BranchBreak;
var I: Integer;
begin
  for I := 0 to 10 do
    if I = 5 then
      Break;
  I := 1;
end;

procedure BranchContinue;
var I: Integer;
begin
  while I < 10 do
    if I = 5 then
      Continue;
  I := 1;
end;

procedure NestedBranchBreak;
var Outer, Inner: Integer;
begin
  while Outer < 10 do
    for Inner := 0 to 10 do
      if Inner = 5 then
        Break;
  Outer := 1;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let break_cfg = cfg_for(&cfgs, "BranchBreak");
    let break_stmt = block_with_stmt(break_cfg, &source, "statement", "Break");
    let for_condition = block_with_stmt(break_cfg, &source, "for", "for I := 0 to 10");
    let for_after = target_of(break_cfg, for_condition, EdgeKind::LoopExit);
    assert_eq!(
        successors(break_cfg, break_stmt),
        vec![(for_after, EdgeKind::Normal)],
        "Break in an unbraced if branch must leave its loop directly"
    );
    assert!(!can_reach(break_cfg, break_stmt, for_condition));

    let continue_cfg = cfg_for(&cfgs, "BranchContinue");
    let continue_stmt = block_with_stmt(continue_cfg, &source, "statement", "Continue");
    let while_condition = block_with_stmt(continue_cfg, &source, "while", "while I < 10");
    let while_after = target_of(continue_cfg, while_condition, EdgeKind::LoopExit);
    assert_eq!(
        successors(continue_cfg, continue_stmt),
        vec![(while_condition, EdgeKind::Normal)],
        "Continue in an unbraced if branch must restart its loop"
    );
    assert!(!successors(continue_cfg, continue_stmt)
        .iter()
        .any(|(target, _)| *target == while_after));

    let nested_cfg = cfg_for(&cfgs, "NestedBranchBreak");
    let nested_break = block_with_stmt(nested_cfg, &source, "statement", "Break");
    let inner_condition = block_with_stmt(nested_cfg, &source, "for", "for Inner := 0 to 10");
    let inner_after = target_of(nested_cfg, inner_condition, EdgeKind::LoopExit);
    let outer_condition = block_with_stmt(nested_cfg, &source, "while", "while Outer < 10");
    assert_eq!(
        successors(nested_cfg, nested_break),
        vec![(inner_after, EdgeKind::Normal)],
        "nested unbraced Break must target the innermost loop"
    );
    assert!(!successors(nested_cfg, nested_break)
        .iter()
        .any(|(target, _)| *target == inner_condition));
    assert!(can_reach(nested_cfg, inner_after, outer_condition));
}

#[test]
fn try_branch_controls_keep_loop_targets_and_raise_stops_fallthrough() {
    let source = br#"
unit TryControlFlow;
interface
implementation

procedure TryBranchControls;
var I: Integer;
begin
  while I < 10 do
    try
      if I = 0 then
        Break
      else
        Continue;
    except
      on E: Exception do
        I := 1;
    end;
  I := 2;
end;

procedure BranchRaise;
var Value: Integer;
begin
  try
    if Value = 0 then
      raise Exception.Create('error')
    else
      Value := 1;
    Value := 2;
  except
    on E: Exception do
      Value := 3;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let try_cfg = cfg_for(&cfgs, "TryBranchControls");
    let break_stmt = block_with_stmt(try_cfg, &source, "statement", "Break");
    let continue_stmt = block_with_stmt(try_cfg, &source, "statement", "Continue");
    let while_condition = block_with_stmt(try_cfg, &source, "while", "while I < 10");
    let while_after = target_of(try_cfg, while_condition, EdgeKind::LoopExit);
    assert_eq!(
        successors(try_cfg, break_stmt),
        vec![(while_after, EdgeKind::Normal)],
        "Break nested in a try/if must leave the active loop"
    );
    assert_eq!(
        successors(try_cfg, continue_stmt),
        vec![(while_condition, EdgeKind::Normal)],
        "Continue nested in a try/if must restart the active loop"
    );

    let raise_cfg = cfg_for(&cfgs, "BranchRaise");
    let raise_stmt = block_with_stmt(raise_cfg, &source, "raise", "raise Exception.Create");
    let handler = block_of_kind(raise_cfg, cfg_core::BasicBlockKind::ExceptHandler);
    let after_try = block_with_stmt(raise_cfg, &source, "assignment", "Value := 2");
    let raise_successors = successors(raise_cfg, raise_stmt);
    assert!(
        raise_successors.contains(&(handler, EdgeKind::ExceptionThrow)),
        "raise in an unbraced branch must enter the exception handler"
    );
    assert!(
        raise_successors.contains(&(raise_cfg.exit, EdgeKind::ExceptionThrow)),
        "without semantic type resolution, the raise must retain an unmatched path"
    );
    assert!(!can_reach(raise_cfg, raise_stmt, after_try));
}

#[test]
fn exit_with_value_inside_statement_wrapper_terminates_function() {
    let source = br#"
unit WrappedExit;
interface
implementation

function ExitWithValue(Value: Integer): Integer;
begin
  if Value > 0 then
    Exit(Value);
  Result := 1;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ExitWithValue");

    let exit_stmt = block_with_stmt(cfg, &source, "statement", "Exit(Value)");
    let result_assignment = block_with_stmt(cfg, &source, "assignment", "Result := 1");
    assert_eq!(
        successors(cfg, exit_stmt),
        vec![(cfg.exit, EdgeKind::Normal)],
        "Exit(value) wrapped in a statement node must terminate the function"
    );
    assert!(!can_reach(cfg, exit_stmt, result_assignment));
}

#[test]
fn terminated_repeat_body_does_not_fall_through_to_condition() {
    let source = br#"
unit RepeatTermination;
interface
implementation

procedure RepeatExit;
begin
  repeat
    Exit;
  until Done;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "RepeatExit");

    let exit_stmt = block_with_stmt(cfg, &source, "statement", "Exit");
    let repeat_condition = block_with_stmt(cfg, &source, "repeat", "repeat");
    let after_loop = block_with_stmt(cfg, &source, "statement", "After");
    assert_eq!(
        successors(cfg, exit_stmt),
        vec![(cfg.exit, EdgeKind::Normal)],
        "a terminated repeat body must not acquire a condition fallthrough"
    );
    assert!(!can_reach(cfg, exit_stmt, repeat_condition));
    assert!(!can_reach(cfg, exit_stmt, after_loop));
}

#[test]
fn parenthesized_break_and_continue_have_exact_loop_successors() {
    let source = br#"
unit ParenthesizedLoopControls;
interface
implementation

procedure ParenthesizedBreak;
begin
  while BreakCondition do
  begin
    if StopNow then
      Break();
    AfterBreak;
  end;
  AfterBreakLoop;
end;

procedure ParenthesizedContinue;
begin
  while ContinueCondition do
  begin
    if SkipRest then
      Continue();
    AfterContinue;
  end;
  AfterContinueLoop;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let break_cfg = cfg_for(&cfgs, "ParenthesizedBreak");
    let break_stmt = block_with_stmt(break_cfg, &source, "statement", "Break()");
    let break_condition = block_with_stmt(break_cfg, &source, "while", "while BreakCondition");
    let break_after_loop = target_of(break_cfg, break_condition, EdgeKind::LoopExit);
    assert_eq!(
        successors(break_cfg, break_stmt),
        vec![(break_after_loop, EdgeKind::Normal)],
        "Break() must leave the active loop without falling through"
    );

    let continue_cfg = cfg_for(&cfgs, "ParenthesizedContinue");
    let continue_stmt = block_with_stmt(continue_cfg, &source, "statement", "Continue()");
    let continue_condition =
        block_with_stmt(continue_cfg, &source, "while", "while ContinueCondition");
    assert_eq!(
        successors(continue_cfg, continue_stmt),
        vec![(continue_condition, EdgeKind::Normal)],
        "Continue() must restart the active loop without reaching later body code"
    );
}
