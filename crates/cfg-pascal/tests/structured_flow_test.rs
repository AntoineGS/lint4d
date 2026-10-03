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
        "structured-flow fixture must not contain parser errors:\n{}",
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

fn statement_free_roots(cfg: &Cfg) -> Vec<BlockId> {
    cfg.graph
        .node_indices()
        .filter(|&node| {
            node != cfg.entry.index()
                && node != cfg.exit.index()
                && cfg.graph[node].stmts.is_empty()
                && !cfg.graph.edge_indices().any(|edge| {
                    cfg.graph
                        .edge_endpoints(edge)
                        .is_some_and(|(_, target)| target == node)
                })
        })
        .map(BlockId::from)
        .collect()
}

#[test]
fn case_labels_and_ranges_dispatch_to_independent_arms_and_default() {
    let source = br#"
unit CaseFlow;
interface
implementation

procedure CaseAlternatives;
begin
  case Choice of
    1, 2: FirstArm;
    3..5: RangeArm;
  else
    DefaultArm;
  end;
  AfterCase;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "CaseAlternatives");

    let selector = block_with_stmt(cfg, &source, "case", "case Choice");
    let first_arm = block_with_stmt(cfg, &source, "statement", "FirstArm");
    let range_arm = block_with_stmt(cfg, &source, "statement", "RangeArm");
    let default_arm = block_with_stmt(cfg, &source, "statement", "DefaultArm");
    let after_case = block_with_stmt(cfg, &source, "statement", "AfterCase");

    let selector_successors = successors(cfg, selector);
    assert_eq!(
        selector_successors
            .iter()
            .filter(|(_, kind)| *kind == EdgeKind::CaseArm)
            .count(),
        3,
        "the selector must dispatch to both labels/ranges and the default arm"
    );
    for arm in [first_arm, range_arm, default_arm] {
        assert!(
            selector_successors.contains(&(arm, EdgeKind::CaseArm)),
            "selector must have a CaseArm edge to {arm:?}"
        );
        assert!(can_reach(cfg, arm, after_case));
    }
    assert!(!can_reach(cfg, first_arm, range_arm));
    assert!(!can_reach(cfg, range_arm, default_arm));

    let selector_ref = cfg.graph[selector.index()]
        .stmts
        .iter()
        .find(|stmt| stmt.node_kind == "case")
        .expect("case selector reference");
    let selector_text = std::str::from_utf8(&source[selector_ref.byte_range.clone()]).unwrap();
    assert!(!selector_text.contains("FirstArm"));
    assert!(!selector_text.contains("RangeArm"));
    assert!(!selector_text.contains("DefaultArm"));
}

#[test]
fn case_without_default_keeps_a_no_match_path_and_routes_selector_exceptions() {
    let source = br#"
unit CaseNoDefault;
interface
implementation

procedure NestedCase;
begin
  try
    if ChooseCase then
      case CaseValue() of
        1: OneArm;
        2..4: RangeArm;
      end
    else
      ElseArm;
  except
    HandleCase;
  end;
  AfterCase;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NestedCase");

    let selector = block_with_stmt(cfg, &source, "case", "case CaseValue()");
    let one_arm = block_with_stmt(cfg, &source, "statement", "OneArm");
    let range_arm = block_with_stmt(cfg, &source, "statement", "RangeArm");
    let else_arm = block_with_stmt(cfg, &source, "statement", "ElseArm");
    let handler = block_with_stmt(cfg, &source, "statement", "HandleCase");
    let after_case = block_with_stmt(cfg, &source, "statement", "AfterCase");

    let selector_successors = successors(cfg, selector);
    assert!(selector_successors.contains(&(one_arm, EdgeKind::CaseArm)));
    assert!(selector_successors.contains(&(range_arm, EdgeKind::CaseArm)));
    let no_match = selector_successors
        .iter()
        .find_map(|(target, kind)| {
            (*kind == EdgeKind::CaseArm && *target != one_arm && *target != range_arm)
                .then_some(*target)
        })
        .expect("case without else must retain a no-match CaseArm edge");
    assert!(can_reach(cfg, no_match, after_case));
    assert!(
        selector_successors.contains(&(handler, EdgeKind::ExceptionThrow)),
        "evaluating a case selector must retain its enclosing handler edge"
    );
    assert!(can_reach(cfg, one_arm, after_case));
    assert!(can_reach(cfg, range_arm, after_case));
    assert!(can_reach(cfg, else_arm, after_case));
}

#[test]
fn case_otherwise_is_a_default_arm_without_a_false_no_match_path() {
    let source = br#"
unit CaseOtherwise;
interface
implementation

procedure CaseOtherwise;
begin
  case Choice of
    1: FirstArm;
  otherwise
    OtherwiseArm;
  end;
  AfterCase;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "CaseOtherwise");
    let selector = block_with_stmt(cfg, &source, "case", "case Choice");
    let first_arm = block_with_stmt(cfg, &source, "statement", "FirstArm");
    let otherwise_arm = block_with_stmt(cfg, &source, "statement", "OtherwiseArm");
    let after_case = block_with_stmt(cfg, &source, "statement", "AfterCase");

    let selector_successors = successors(cfg, selector);
    let case_arms: Vec<_> = selector_successors
        .iter()
        .filter_map(|(target, kind)| (*kind == EdgeKind::CaseArm).then_some(*target))
        .collect();
    assert_eq!(case_arms.len(), 2);
    assert!(case_arms.contains(&first_arm));
    assert!(case_arms.contains(&otherwise_arm));
    assert!(can_reach(cfg, first_arm, after_case));
    assert!(can_reach(cfg, otherwise_arm, after_case));
}

#[test]
fn empty_colon_after_otherwise_is_not_an_executable_case_statement() {
    let source = br#"
unit CaseOtherwiseColon;
interface
implementation

procedure CaseOtherwiseColon;
begin
  try
    case Choice of
      otherwise:
    end;
  except
    HandleCase;
  end;
  AfterCase;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "CaseOtherwiseColon");
    let selector = block_with_stmt(cfg, &source, "case", "case Choice");
    let handler = block_with_stmt(cfg, &source, "statement", "HandleCase");
    let after_case = block_with_stmt(cfg, &source, "statement", "AfterCase");

    assert!(
        cfg.graph
            .node_indices()
            .flat_map(|index| cfg.graph[index].stmts.iter())
            .all(|stmt| stmt.node_kind != ":"),
        "the optional colon after otherwise is punctuation, not an executable statement"
    );

    let default_arm = successors(cfg, selector)
        .into_iter()
        .find_map(|(target, kind)| (kind == EdgeKind::CaseArm).then_some(target))
        .expect("otherwise must still create a default case arm when its body is empty");
    assert!(can_reach(cfg, default_arm, after_case));

    let handler_edges: Vec<_> = cfg
        .graph
        .edge_indices()
        .filter_map(|edge| {
            let (source, target) = cfg.graph.edge_endpoints(edge)?;
            (target == handler.index() && cfg.graph[edge] == EdgeKind::ExceptionThrow)
                .then_some(BlockId::from(source))
        })
        .collect();
    assert_eq!(
        handler_edges,
        vec![selector],
        "only evaluating the case selector should have an exception edge to the handler"
    );
}

#[test]
fn preprocessor_statement_blocks_are_alternatives_and_can_skip() {
    let source = br#"
unit ConditionalStatements;
interface
implementation

procedure ConditionalRaise;
begin
  try
    {$IFDEF FIRST}
    raise FirstError;
    {$ELSE}
    raise SecondError;
    {$ENDIF}
  except
    HandleRaise;
  end;
end;

procedure ConditionalSkip;
begin
  BeforeSkip;
  {$IFDEF FIRST}
  BranchWork;
  {$ENDIF}
  AfterSkip;
end;

procedure NestedConditional;
begin
  {$IFDEF OUTER}
  {$IFDEF INNER}
  NestedWork;
  {$ENDIF}
  {$ELSE}
  OtherWork;
  {$ENDIF}
  AfterNested;
end;

initialization
  {$IFDEF INIT}
  InitFirst;
  {$ELSE}
  InitSecond;
  {$ENDIF}
  InitAfter;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let raise_cfg = cfg_for(&cfgs, "ConditionalRaise");
    let first_raise = block_with_stmt(raise_cfg, &source, "raise", "raise FirstError");
    let second_raise = block_with_stmt(raise_cfg, &source, "raise", "raise SecondError");
    let raise_handler = block_with_stmt(raise_cfg, &source, "statement", "HandleRaise");
    assert!(successors(raise_cfg, first_raise).contains(&(raise_handler, EdgeKind::ExceptionThrow)));
    assert!(
        successors(raise_cfg, second_raise).contains(&(raise_handler, EdgeKind::ExceptionThrow))
    );
    assert!(!can_reach(raise_cfg, first_raise, second_raise));
    assert!(!can_reach(raise_cfg, second_raise, first_raise));
    assert!(
        raise_cfg
            .graph
            .node_indices()
            .flat_map(|index| raise_cfg.graph[index].stmts.iter())
            .all(|stmt| !stmt.node_kind.starts_with("pp")),
        "preprocessor directives are syntax controls, not executable statement references"
    );

    let skip_cfg = cfg_for(&cfgs, "ConditionalSkip");
    let before_skip = block_with_stmt(skip_cfg, &source, "statement", "BeforeSkip");
    let branch_work = block_with_stmt(skip_cfg, &source, "statement", "BranchWork");
    let after_skip = block_with_stmt(skip_cfg, &source, "statement", "AfterSkip");
    let before_successors = successors(skip_cfg, before_skip);
    assert!(before_successors
        .iter()
        .any(|(_, kind)| *kind == EdgeKind::ConditionalTrue));
    assert!(before_successors
        .iter()
        .any(|(_, kind)| *kind == EdgeKind::ConditionalFalse));
    assert!(can_reach(skip_cfg, branch_work, after_skip));

    let nested_cfg = cfg_for(&cfgs, "NestedConditional");
    let nested_work = block_with_stmt(nested_cfg, &source, "statement", "NestedWork");
    let other_work = block_with_stmt(nested_cfg, &source, "statement", "OtherWork");
    let after_nested = block_with_stmt(nested_cfg, &source, "statement", "AfterNested");
    assert!(can_reach(nested_cfg, nested_work, after_nested));
    assert!(can_reach(nested_cfg, other_work, after_nested));
    assert!(!can_reach(nested_cfg, nested_work, other_work));
    assert!(!can_reach(nested_cfg, other_work, nested_work));

    let init_cfg = cfg_for(&cfgs, "ConditionalStatements.<initialization>");
    let init_first = block_with_stmt(init_cfg, &source, "statement", "InitFirst");
    let init_second = block_with_stmt(init_cfg, &source, "statement", "InitSecond");
    let init_after = block_with_stmt(init_cfg, &source, "statement", "InitAfter");
    assert!(can_reach(init_cfg, init_first, init_after));
    assert!(can_reach(init_cfg, init_second, init_after));
    assert!(!can_reach(init_cfg, init_first, init_second));
    assert!(!can_reach(init_cfg, init_second, init_first));
}

#[test]
fn preprocessor_elseif_retains_the_no_match_path_through_nested_blocks() {
    let source = br#"
unit ConditionalElseIf;
interface
implementation

procedure ConditionalElseIf;
begin
  {$IFDEF OUTER}
    {$IFDEF INNER}
    Exit;
    {$ELSEIF INNER_ALT}
    Exit;
    {$ENDIF}
  {$ELSEIF OUTER_ALT}
    Exit;
  {$ENDIF}
  AfterAllConditions;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ConditionalElseIf");
    let after = block_with_stmt(cfg, &source, "statement", "AfterAllConditions");

    assert!(
        cfg.is_reachable(cfg.entry, after),
        "all unknown preprocessor conditions may be false, so the post-conditional path must remain reachable"
    );
}

#[test]
fn preprocessor_branch_labels_keep_local_and_external_targets() {
    let source = br#"
unit ConditionalLabels;
interface
implementation

procedure BranchLocalLabels;
label Done;
begin
  {$IFDEF FIRST}
  goto Done;
  Done: FirstBranch;
  {$ELSEIF SECOND}
  goto Done;
  Done: SecondBranch;
  {$ENDIF}
end;

procedure BranchExternalLabel;
label Done;
begin
  {$IFDEF FIRST}
  Done: FirstBranch;
  {$ELSEIF SECOND}
  Done: SecondBranch;
  {$ENDIF}
  goto Done;
end;

procedure BranchLabelCleanup;
label Done;
begin
  {$IFDEF FIRST}
  try
    goto Done;
    Done: FirstBranch;
  finally
    CleanupFirst;
  end;
  {$ELSEIF SECOND}
  Done: SecondBranch;
  {$ENDIF}
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let local_cfg = cfg_for(&cfgs, "BranchLocalLabels");
    let local_gotos = blocks_with_stmt(local_cfg, &source, "goto", "goto Done");
    let local_labels = blocks_with_stmt(local_cfg, &source, "label", "Done:");
    assert_eq!(local_gotos.len(), 2);
    assert_eq!(local_labels.len(), 2);
    assert_eq!(
        successors(local_cfg, local_gotos[0]),
        vec![(local_labels[0], EdgeKind::Goto)],
        "a branch-local goto must prefer the label declared in that same branch"
    );
    assert_eq!(
        successors(local_cfg, local_gotos[1]),
        vec![(local_labels[1], EdgeKind::Goto)],
        "the alternate branch must resolve its own duplicate label"
    );

    let external_cfg = cfg_for(&cfgs, "BranchExternalLabel");
    let external_goto = block_with_stmt(external_cfg, &source, "goto", "goto Done");
    let external_labels = blocks_with_stmt(external_cfg, &source, "label", "Done:");
    assert_eq!(external_labels.len(), 2);
    let external_targets: HashSet<_> = successors(external_cfg, external_goto)
        .into_iter()
        .filter_map(|(target, kind)| (kind == EdgeKind::Goto).then_some(target))
        .collect();
    assert_eq!(
        external_targets,
        external_labels.into_iter().collect(),
        "a goto outside the conditional must retain every feasible duplicate-label target"
    );

    let cleanup_cfg = cfg_for(&cfgs, "BranchLabelCleanup");
    let cleanup_goto = block_with_stmt(cleanup_cfg, &source, "goto", "goto Done");
    let cleanup_labels = blocks_with_stmt(cleanup_cfg, &source, "label", "Done:");
    let first_cleanup_label = cleanup_labels
        .iter()
        .copied()
        .find(|label| {
            cleanup_cfg.graph[label.index()].stmts.iter().any(|stmt| {
                stmt.node_kind == "statement"
                    && std::str::from_utf8(&source[stmt.byte_range.clone()])
                        .is_ok_and(|text| text.contains("FirstBranch"))
            })
        })
        .expect("first branch label must be present");
    assert_eq!(
        successors(cleanup_cfg, cleanup_goto),
        vec![(first_cleanup_label, EdgeKind::Goto)],
        "a branch-local target inside try/finally must not be replaced by the alternate branch label"
    );
    assert!(
        !successors(cleanup_cfg, cleanup_goto)
            .iter()
            .any(|(_, kind)| *kind == EdgeKind::FinallyEntry),
        "a goto to its same-branch label must not unwind an unrelated cleanup scope"
    );
}

#[test]
fn preprocessor_goto_can_target_labels_in_a_separate_conditional_block() {
    let source = br#"
unit SeparateConditionalLabels;
interface
implementation

procedure SeparateConditionalLabels;
label Done;
begin
  {$IFDEF A}
  goto Done;
  {$ENDIF}
  {$IFDEF B}
  Done: First;
  {$ELSE}
  Done: Second;
  {$ENDIF}
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "SeparateConditionalLabels");
    let goto_block = block_with_stmt(cfg, &source, "goto", "goto Done");
    let labels = blocks_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(labels.len(), 2);

    let targets: HashSet<_> = successors(cfg, goto_block)
        .into_iter()
        .filter_map(|(target, kind)| (kind == EdgeKind::Goto).then_some(target))
        .collect();
    assert_eq!(
        targets,
        labels.into_iter().collect(),
        "a goto in one conditional block must retain labels from a separate feasible block"
    );
}

#[test]
fn preprocessor_labels_inside_cloned_finalizers_keep_their_local_binding() {
    let source = br#"
unit ConditionalFinalizerLabels;
interface
implementation

procedure ConditionalFinalizerLabels;
label Done;
begin
  try
    Work;
  finally
    {$IFDEF FIRST}
    goto Done;
    {$ELSEIF SECOND}
    goto Done;
    {$ENDIF}
  Done:
    CleanupDone;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ConditionalFinalizerLabels");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");

    assert!(gotos.len() >= 2);
    for goto in gotos {
        let edges = successors(cfg, goto);
        let [(target, kind)] = edges.as_slice() else {
            panic!("finalizer goto must have one resolved target edge");
        };
        assert_eq!(*kind, EdgeKind::FinallyExit);
        assert!(
            cfg.graph[target.index()].stmts.iter().any(|stmt| {
                stmt.node_kind == "label"
                    && std::str::from_utf8(&source[stmt.byte_range.clone()])
                        .is_ok_and(|text| text.contains("Done:"))
            }),
            "a preprocessor-branch goto inside a finalizer must resolve a finalizer-local label"
        );
    }
}

#[test]
fn preprocessor_labels_in_each_finalizer_clone_keep_forward_targets_local() {
    let source = br#"
unit ForwardConditionalFinalizerLabels;
interface
implementation

procedure ForwardConditionalFinalizerLabels;
label Done;
begin
  try
    if Flag then Exit;
  finally
    goto Done;
    {$IFDEF A}
    Done: CleanupA;
    {$ELSE}
    Done: CleanupB;
    {$ENDIF}
    Tail;
  end;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ForwardConditionalFinalizerLabels");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");
    let labels = blocks_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(gotos.len(), 3);
    assert_eq!(labels.len(), 6);

    let expected_pairs: Vec<Vec<BlockId>> =
        labels.chunks_exact(2).map(|pair| pair.to_vec()).collect();
    let mut actual_pairs = Vec::new();
    for goto in gotos {
        let edges = successors(cfg, goto);
        assert_eq!(
            edges.len(),
            2,
            "each clone must resolve both feasible labels"
        );
        assert!(
            edges.iter().all(|(_, kind)| *kind == EdgeKind::FinallyExit),
            "a finalizer-local goto must use cleanup completion edges"
        );
        let mut targets: Vec<_> = edges.into_iter().map(|(target, _)| target).collect();
        targets.sort_by_key(|target| target.index());
        actual_pairs.push(targets);
    }
    actual_pairs.sort_by_key(|pair| pair[0].index());
    assert_eq!(
        actual_pairs, expected_pairs,
        "each finalizer clone must target its own conditional-label pair"
    );

    let exit = block_with_stmt(cfg, &source, "statement", "Exit");
    let after = block_with_stmt(cfg, &source, "statement", "After");
    assert!(
        !can_reach(cfg, exit, after),
        "Exit must not reach the post-finalizer continuation"
    );
}

#[test]
fn preprocessor_labels_in_each_finalizer_clone_keep_backward_targets_local() {
    let source = br#"
unit BackwardConditionalFinalizerLabels;
interface
implementation

procedure BackwardConditionalFinalizerLabels;
label Done;
begin
  try
    if Flag then Exit;
  finally
    {$IFDEF A}
    Done: CleanupA;
    {$ELSE}
    Done: CleanupB;
    {$ENDIF}
    if Retry then goto Done;
    Tail;
  end;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "BackwardConditionalFinalizerLabels");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");
    let labels = blocks_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(gotos.len(), 3);
    assert_eq!(labels.len(), 6);

    let expected_pairs: Vec<Vec<BlockId>> =
        labels.chunks_exact(2).map(|pair| pair.to_vec()).collect();
    let mut actual_pairs = Vec::new();
    for goto in gotos {
        let edges = successors(cfg, goto);
        assert_eq!(
            edges.len(),
            2,
            "each clone must resolve both feasible labels"
        );
        assert!(
            edges.iter().all(|(_, kind)| *kind == EdgeKind::FinallyExit),
            "a finalizer-local goto must use cleanup completion edges"
        );
        let mut targets: Vec<_> = edges.into_iter().map(|(target, _)| target).collect();
        targets.sort_by_key(|target| target.index());
        actual_pairs.push(targets);
    }
    actual_pairs.sort_by_key(|pair| pair[0].index());
    assert_eq!(
        actual_pairs, expected_pairs,
        "each finalizer clone must target its own conditional-label pair"
    );

    let exit = block_with_stmt(cfg, &source, "statement", "Exit");
    let after = block_with_stmt(cfg, &source, "statement", "After");
    assert!(
        !can_reach(cfg, exit, after),
        "Exit must not reach the post-finalizer continuation"
    );
}

#[test]
fn finally_goto_resolves_conditional_labels_in_the_containing_routine() {
    let source = br#"
unit FinallyRoutineConditionalLabels;
interface
implementation

procedure FinallyRoutineConditionalLabels;
label Done;
begin
  try
    Work;
  finally
    goto Done;
  end;
  {$IFDEF A}
  Done: First;
  {$ELSE}
  Done: Second;
  {$ENDIF}
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "FinallyRoutineConditionalLabels");
    let labels = blocks_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(labels.len(), 2);
    let expected: HashSet<_> = labels.iter().copied().collect();

    for goto in blocks_with_stmt(cfg, &source, "goto", "goto Done") {
        let targets: HashSet<_> = successors(cfg, goto)
            .into_iter()
            .filter_map(|(target, kind)| {
                (kind == EdgeKind::FinallyExit && expected.contains(&target)).then_some(target)
            })
            .collect();
        assert_eq!(
            targets, expected,
            "a finalizer goto must resolve conditional labels in its containing routine"
        );
    }
}

#[test]
fn nested_finalizer_goto_resolves_enclosing_conditional_label_clone() {
    let source = br#"
unit NestedEnclosingConditionalLabels;
interface
implementation

procedure NestedEnclosingConditionalLabels;
label Done;
begin
  try
    if Flag then Exit;
  finally
    try
      Work;
    finally
      goto Done;
    end;
    {$IFDEF A}
    Done: First;
    {$ELSE}
    Done: Second;
    {$ENDIF}
    Tail;
  end;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NestedEnclosingConditionalLabels");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");
    let labels = blocks_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(gotos.len(), 6);
    assert_eq!(labels.len(), 6);

    let expected_pairs: HashSet<Vec<BlockId>> =
        labels.chunks_exact(2).map(|pair| pair.to_vec()).collect();
    let mut actual_pairs = HashSet::new();
    for goto in gotos {
        let edges = successors(cfg, goto);
        let mut targets: Vec<_> = edges
            .into_iter()
            .filter_map(|(target, _)| labels.contains(&target).then_some(target))
            .collect();
        targets.sort_by_key(|target| target.index());
        assert_eq!(
            targets.len(),
            2,
            "nested finalizer goto must resolve both labels in its enclosing clone"
        );
        actual_pairs.insert(targets);
    }
    assert_eq!(
        actual_pairs, expected_pairs,
        "nested finalizer gotos must not resolve another clone's conditional labels"
    );

    let exit = block_with_stmt(cfg, &source, "statement", "Exit");
    let after = block_with_stmt(cfg, &source, "statement", "After");
    assert!(!can_reach(cfg, exit, after));
}

#[test]
fn preprocessor_statement_blocks_preserve_loop_controls_and_finally_labels() {
    let source = br#"
unit ConditionalTransfers;
interface
implementation

procedure ConditionalLoopControls;
begin
  while LoopCondition do
  begin
    {$IFDEF FIRST}
    Continue;
    {$ELSE}
    Break;
    {$ENDIF}
  end;
  AfterLoop;
end;

procedure ConditionalLabel;
label Done;
begin
  {$IFDEF FIRST}
  Done: BranchWork;
  {$ELSE}
  OtherBranch;
  {$ENDIF}
  goto Done;
end;

procedure ConditionalGotoFinally;
label Done;
begin
  try
    {$IFDEF FIRST}
    goto Done;
    {$ELSE}
    Work;
    {$ENDIF}
  finally
    Cleanup;
  end;
Done:
  Target;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let loop_cfg = cfg_for(&cfgs, "ConditionalLoopControls");
    let condition = block_with_stmt(loop_cfg, &source, "while", "while LoopCondition");
    let after_loop = successors(loop_cfg, condition)
        .into_iter()
        .find_map(|(target, kind)| (kind == EdgeKind::LoopExit).then_some(target))
        .expect("loop condition must have an exit target");
    let continue_stmt = block_with_stmt(loop_cfg, &source, "statement", "Continue");
    let break_stmt = block_with_stmt(loop_cfg, &source, "statement", "Break");
    assert_eq!(
        successors(loop_cfg, continue_stmt),
        vec![(condition, EdgeKind::Normal)]
    );
    assert_eq!(
        successors(loop_cfg, break_stmt),
        vec![(after_loop, EdgeKind::Normal)]
    );

    let label_cfg = cfg_for(&cfgs, "ConditionalLabel");
    let conditional_label = block_with_stmt(label_cfg, &source, "label", "Done:");
    let conditional_goto = block_with_stmt(label_cfg, &source, "goto", "goto Done");
    assert_eq!(
        successors(label_cfg, conditional_goto),
        vec![(conditional_label, EdgeKind::Goto)]
    );

    let goto_cfg = cfg_for(&cfgs, "ConditionalGotoFinally");
    let goto = block_with_stmt(goto_cfg, &source, "goto", "goto Done");
    let label = block_with_stmt(goto_cfg, &source, "label", "Done:");
    let cleanup_blocks = blocks_with_stmt(goto_cfg, &source, "statement", "Cleanup");
    let cleanup = cleanup_blocks
        .iter()
        .copied()
        .find(|cleanup| successors(goto_cfg, goto).contains(&(*cleanup, EdgeKind::FinallyEntry)))
        .expect("conditional goto must enter finally before leaving its scope");
    assert!(successors(goto_cfg, cleanup).contains(&(label, EdgeKind::FinallyExit)));
}

#[test]
fn foreach_has_a_back_edge_exit_and_nested_loop_controls() {
    let source = br#"
unit ForEachFlow;
interface
implementation

procedure ForEachControls;
begin
  for Item in Items do
  begin
    if Item = 1 then
      Continue;
    if Item = 2 then
      Break;
    Body(Item);
  end;
  AfterEach;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ForEachControls");

    let header = block_with_stmt(cfg, &source, "foreach", "for Item in Items");
    let body = block_with_stmt(cfg, &source, "statement", "Body(Item)");
    let continue_stmt = block_with_stmt(cfg, &source, "statement", "Continue");
    let break_stmt = block_with_stmt(cfg, &source, "statement", "Break");
    let after_each = block_with_stmt(cfg, &source, "statement", "AfterEach");

    let header_successors = successors(cfg, header);
    let body_entry = header_successors
        .iter()
        .find_map(|(target, kind)| (*kind == EdgeKind::ConditionalTrue).then_some(*target))
        .expect("foreach header must enter its body");
    let loop_exit = header_successors
        .iter()
        .find_map(|(target, kind)| (*kind == EdgeKind::LoopExit).then_some(*target))
        .expect("foreach header must have a loop-exit edge");
    assert!(can_reach(cfg, body_entry, body));
    assert!(can_reach(cfg, body, header));
    assert!(can_reach(cfg, loop_exit, after_each));
    assert_eq!(
        successors(cfg, continue_stmt),
        vec![(header, EdgeKind::Normal)],
        "Continue inside a foreach must restart the foreach"
    );
    assert_eq!(
        successors(cfg, break_stmt),
        vec![(loop_exit, EdgeKind::Normal)],
        "Break inside a foreach must leave the foreach"
    );

    let header_ref = cfg.graph[header.index()]
        .stmts
        .iter()
        .find(|stmt| stmt.node_kind == "foreach")
        .expect("foreach header reference");
    let header_text = std::str::from_utf8(&source[header_ref.byte_range.clone()]).unwrap();
    assert!(!header_text.contains("Body(Item)"));
}

#[test]
fn foreach_header_and_body_exceptions_reach_an_enclosing_handler() {
    let source = br#"
unit ProtectedForEach;
interface
implementation

procedure ProtectedForEach;
begin
  try
    for Item in ItemsCall() do
      BodyCall(Item);
  except
    HandleEach;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ProtectedForEach");

    let header = block_with_stmt(cfg, &source, "foreach", "for Item in ItemsCall()");
    let body = block_with_stmt(cfg, &source, "statement", "BodyCall(Item)");
    let handler = block_with_stmt(cfg, &source, "statement", "HandleEach");
    assert!(successors(cfg, header).contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, body).contains(&(handler, EdgeKind::ExceptionThrow)));

    let header_ref = cfg.graph[header.index()]
        .stmts
        .iter()
        .find(|stmt| stmt.node_kind == "foreach")
        .expect("foreach header reference");
    let header_text = std::str::from_utf8(&source[header_ref.byte_range.clone()]).unwrap();
    assert!(!header_text.contains("BodyCall"));
}

#[test]
fn with_evaluates_context_once_and_walks_a_nested_body() {
    let source = br#"
unit WithFlow;
interface
implementation

procedure NestedWith;
begin
  with ContextRecord(), OtherContext() do
  begin
    if Ready then
      NestedBody
    else
      AlternateBody;
  end;
  AfterWith;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NestedWith");

    let context = block_with_stmt(cfg, &source, "with", "with ContextRecord(), OtherContext()");
    let nested_body = block_with_stmt(cfg, &source, "statement", "NestedBody");
    let alternate_body = block_with_stmt(cfg, &source, "statement", "AlternateBody");
    let after_with = block_with_stmt(cfg, &source, "statement", "AfterWith");

    let body_entry = successors(cfg, context)
        .into_iter()
        .find_map(|(target, kind)| (kind == EdgeKind::Normal).then_some(target))
        .expect("with context evaluation must enter its body");
    assert!(can_reach(cfg, body_entry, nested_body));
    assert!(can_reach(cfg, body_entry, alternate_body));
    assert!(can_reach(cfg, nested_body, after_with));
    assert!(can_reach(cfg, alternate_body, after_with));

    let context_ref = cfg.graph[context.index()]
        .stmts
        .iter()
        .find(|stmt| stmt.node_kind == "with")
        .expect("with context reference");
    let context_text = std::str::from_utf8(&source[context_ref.byte_range.clone()]).unwrap();
    assert!(!context_text.contains("NestedBody"));
    assert!(!context_text.contains("AlternateBody"));
}

#[test]
fn with_context_and_body_exceptions_reach_an_enclosing_handler() {
    let source = br#"
unit ProtectedWith;
interface
implementation

procedure ProtectedWith;
begin
  try
    with ContextCall() do
      BodyCall;
  except
    HandleWith;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ProtectedWith");

    let context = block_with_stmt(cfg, &source, "with", "with ContextCall()");
    let body = block_with_stmt(cfg, &source, "statement", "BodyCall");
    let handler = block_with_stmt(cfg, &source, "statement", "HandleWith");
    assert!(successors(cfg, context).contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(successors(cfg, body).contains(&(handler, EdgeKind::ExceptionThrow)));

    let context_ref = cfg.graph[context.index()]
        .stmts
        .iter()
        .find(|stmt| stmt.node_kind == "with")
        .expect("with context reference");
    let context_text = std::str::from_utf8(&source[context_ref.byte_range.clone()]).unwrap();
    assert!(!context_text.contains("BodyCall"));
}

#[test]
fn terminated_structures_do_not_leave_disconnected_continuation_roots() {
    let source = br#"
unit SyntheticContinuations;
interface
implementation

procedure WithExit;
begin
  with Obj do
    Exit;
end;

procedure CaseExit;
begin
  case Choice of
    1: Exit;
  else
    Exit;
  end;
end;

procedure FinallyExit;
begin
  try
    Exit;
  finally
    Cleanup;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    for procedure in ["WithExit", "CaseExit", "FinallyExit"] {
        let cfg = cfg_for(&cfgs, procedure);
        assert_eq!(
            statement_free_roots(cfg),
            Vec::new(),
            "{procedure} must not retain disconnected synthetic continuation blocks"
        );
    }
}

#[test]
fn goto_resolves_forward_backward_case_insensitive_and_stops_fallthrough() {
    let source = br#"
unit GotoFlow;
interface
implementation

procedure GotoTargets;
begin
  goto ForwardLabel;
  SkippedForward;
ForwardLabel:
  ForwardBody;
  goto BackLabel;
BackLabel:
  BackBody;
  goto FORWARDLABEL;
  UnreachableAfterBackward;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "GotoTargets");

    let forward_goto = block_with_stmt(cfg, &source, "goto", "goto ForwardLabel");
    let second_goto = block_with_stmt(cfg, &source, "goto", "goto BackLabel");
    let backward_goto = block_with_stmt(cfg, &source, "goto", "goto FORWARDLABEL");
    let forward_body = block_with_stmt(cfg, &source, "statement", "ForwardBody");
    let back_body = block_with_stmt(cfg, &source, "statement", "BackBody");
    let skipped = block_with_stmt(cfg, &source, "statement", "SkippedForward");
    let unreachable = block_with_stmt(cfg, &source, "statement", "UnreachableAfterBackward");

    assert_eq!(
        successors(cfg, forward_goto),
        vec![(forward_body, EdgeKind::Goto)],
        "forward goto must target the labeled statement without fallthrough"
    );
    assert_eq!(
        successors(cfg, second_goto),
        vec![(back_body, EdgeKind::Goto)],
        "goto target names must be resolved case-insensitively"
    );
    assert_eq!(
        successors(cfg, backward_goto),
        vec![(forward_body, EdgeKind::Goto)],
        "backward goto must resolve to an earlier label"
    );
    assert!(!can_reach(cfg, forward_goto, skipped));
    assert!(!can_reach(cfg, backward_goto, unreachable));
    assert!(can_reach(cfg, forward_body, back_body));
    assert!(can_reach(cfg, back_body, forward_body));
}

#[test]
fn goto_inside_finally_scope_does_not_unwind_but_leaving_goto_does() {
    let source = br#"
unit GotoCleanup;
interface
implementation

procedure SameScopeGoto;
begin
  try
    goto InsideLabel;
    SkippedInside;
InsideLabel:
    InsideBody;
  finally
    SameCleanup;
  end;
end;

procedure LeavingGoto;
begin
  try
    goto OutsideLabel;
  finally
    LeavingCleanup;
  end;
OutsideLabel:
  OutsideBody;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let same_cfg = cfg_for(&cfgs, "SameScopeGoto");
    let same_goto = block_with_stmt(same_cfg, &source, "goto", "goto InsideLabel");
    let inside_label = block_with_stmt(same_cfg, &source, "label", "InsideLabel");
    let inside_body = block_with_stmt(same_cfg, &source, "statement", "InsideBody");
    assert_eq!(
        successors(same_cfg, same_goto),
        vec![(inside_label, EdgeKind::Goto)],
        "a goto whose label remains in the try body must not enter its finally"
    );
    assert!(can_reach(same_cfg, inside_label, inside_body));

    let leaving_cfg = cfg_for(&cfgs, "LeavingGoto");
    let leaving_goto = block_with_stmt(leaving_cfg, &source, "goto", "goto OutsideLabel");
    let leaving_cleanup = block_with_stmt(leaving_cfg, &source, "statement", "LeavingCleanup");
    let outside_body = block_with_stmt(leaving_cfg, &source, "statement", "OutsideBody");
    assert!(
        successors(leaving_cfg, leaving_goto).contains(&(leaving_cleanup, EdgeKind::FinallyEntry)),
        "a goto leaving a finally scope must enter its cleanup"
    );
    assert!(
        successors(leaving_cfg, leaving_cleanup).contains(&(outside_body, EdgeKind::FinallyExit))
    );
    assert!(!successors(leaving_cfg, leaving_goto)
        .iter()
        .any(|(target, kind)| *target == outside_body && *kind == EdgeKind::Goto));
}

#[test]
fn goto_finalizer_clones_keep_distinct_label_continuations() {
    let source = br#"
unit GotoFinalizerLabels;
interface
implementation

procedure DistinctGotoLabels;
begin
  try
    if ChooseFirst then
      goto FirstLabel
    else
      goto SecondLabel;
  finally
    CleanupBranch;
  end;
FirstLabel:
  FirstBody;
  Exit;
SecondLabel:
  SecondBody;
  Exit;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "DistinctGotoLabels");

    let first_body = block_with_stmt(cfg, &source, "statement", "FirstBody");
    let second_body = block_with_stmt(cfg, &source, "statement", "SecondBody");
    let cleanup_blocks = blocks_with_stmt(cfg, &source, "statement", "CleanupBranch");
    assert!(
        cleanup_blocks.len() >= 2,
        "different goto labels need distinct finalizer continuations"
    );

    let first_cleanup = cleanup_blocks
        .iter()
        .copied()
        .find(|cleanup| successors(cfg, *cleanup).contains(&(first_body, EdgeKind::FinallyExit)))
        .expect("one finalizer clone must continue to FirstLabel");
    let second_cleanup = cleanup_blocks
        .iter()
        .copied()
        .find(|cleanup| successors(cfg, *cleanup).contains(&(second_body, EdgeKind::FinallyExit)))
        .expect("one finalizer clone must continue to SecondLabel");
    assert_ne!(first_cleanup, second_cleanup);
    assert!(!can_reach(cfg, first_cleanup, second_body));
    assert!(!can_reach(cfg, second_cleanup, first_body));
}

#[test]
fn case_selector_reference_skips_parser_extras() {
    let source = br#"
unit CaseSelectorExtra;
interface
implementation

procedure CaseSelectorExtra;
begin
  case { rationale } SelectorCall() of
    1: Arm;
  else
    DefaultArm;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "CaseSelectorExtra");
    let selector = block_with_stmt(cfg, &source, "case", "case");
    let selector_ref = cfg.graph[selector.index()]
        .stmts
        .iter()
        .find(|stmt| stmt.node_kind == "case")
        .expect("case selector reference");
    let selector_text = std::str::from_utf8(&source[selector_ref.byte_range.clone()]).unwrap();
    assert!(selector_text.contains("SelectorCall()"));
}

#[test]
fn finally_goto_resolves_a_procedure_level_label() {
    let source = br#"
unit FinallyGotoProcedureLabel;
interface
implementation

procedure FinallyGotoProcedureLabel;
label Done;
begin
  try
    Work;
  finally
    goto Done;
  end;
  Skipped;
Done:
  TargetBody;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "FinallyGotoProcedureLabel");
    let label = block_with_stmt(cfg, &source, "label", "Done:");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");
    assert!(gotos.len() >= 2);
    for goto in gotos {
        assert_eq!(
            successors(cfg, goto),
            vec![(label, EdgeKind::FinallyExit)],
            "outward finalizer goto must resolve the procedure label"
        );
    }
}

#[test]
fn nested_finally_goto_resolves_an_enclosing_finalizer_label() {
    let source = br#"
unit NestedFinallyGoto;
interface
implementation

procedure NestedFinallyGoto;
label Done;
begin
  try
    Exit;
  finally
    try
      NestedWork;
    finally
      goto Done;
    end;
Done:
    Cleanup;
  end;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NestedFinallyGoto");
    let label = block_with_stmt(cfg, &source, "label", "Done:");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");
    assert!(gotos.len() >= 2);
    for goto in gotos {
        assert_eq!(
            successors(cfg, goto),
            vec![(label, EdgeKind::FinallyExit)],
            "nested finalizer goto must resolve its enclosing label"
        );
    }
}

#[test]
fn cloned_cleanup_gotos_stay_in_their_own_instance() {
    let source = br#"
unit ClonedCleanupGotos;
interface
implementation

procedure ClonedCleanupGotos;
begin
  try
    if LeaveNow then
      Exit;
    Work;
  finally
    goto Done;
  Done:
    Cleanup;
  end;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "ClonedCleanupGotos");
    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto Done");
    let labels = blocks_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(gotos.len(), labels.len());
    assert!(gotos.len() >= 2);
    for (goto, label) in gotos.into_iter().zip(labels) {
        assert!(
            successors(cfg, goto)
                .iter()
                .any(|(target, _)| *target == label),
            "goto {goto:?} must target its same-clone label {label:?}"
        );
    }
}

#[test]
fn goto_after_nested_cleanup_clones_retains_the_later_scope_identity() {
    let source = br#"
unit LaterScopeIdentity;
interface
implementation

procedure LaterScopeIdentity;
begin
  try
    Work;
  finally
    try
      NestedWork;
    finally
      NestedCleanup;
    end;
  end;
  try
    goto Done;
  Done:
    TargetBody;
  finally
    LaterCleanup;
  end;
  After;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "LaterScopeIdentity");
    let goto = block_with_stmt(cfg, &source, "goto", "goto Done");
    let label = block_with_stmt(cfg, &source, "label", "Done:");
    assert_eq!(
        successors(cfg, goto),
        vec![(label, EdgeKind::Goto)],
        "a same-scope goto must not enter the later finally"
    );
}
