use std::collections::HashSet;

use cfg_core::{BlockId, Cfg, EdgeKind};
use cfg_pascal::build_file_cfgs;
use tree_sitter::{Node, Parser, Tree};

fn parse(source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    let language = tree_sitter_pascal::LANGUAGE;
    parser
        .set_language(&language.into())
        .expect("failed to set Pascal language");
    parser.parse(source, None).expect("parser returned no tree")
}

fn parse_clean(source: &[u8]) -> Tree {
    let tree = parse(source);
    assert!(
        !tree.root_node().has_error(),
        "numeric-label fixture must not contain parser errors:\n{}",
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

fn blocks_with_exact_stmt(cfg: &Cfg, source: &[u8], kind: &str, text: &str) -> Vec<BlockId> {
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
                            .is_ok_and(|stmt_text| stmt_text.trim() == text)
                })
                .then(|| BlockId::from(index))
        })
        .collect()
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

fn walk_nodes(node: Node<'_>, out: &mut Vec<(String, String)>, source: &[u8]) {
    let text = std::str::from_utf8(&source[node.byte_range()]).unwrap_or("");
    out.push((node.kind().to_string(), text.to_string()));

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_nodes(child, out, source);
    }
}

#[test]
fn numeric_labels_route_forward_backward_and_named_targets() {
    let source = br#"
unit NumericLabels;
interface
implementation

procedure NumericFlow;
label 0007, 42, Named;
begin
  goto 0007;
  SkippedForward;
7:
  ForwardBody;
  goto 42;
42:
  BackwardBody;
  goto 0007;
  SkippedBackward;
  goto Named;
Named:
  NamedBody;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NumericFlow");

    let forward_gotos = blocks_with_exact_stmt(cfg, &source, "goto", "goto 0007;");
    assert_eq!(
        forward_gotos.len(),
        2,
        "both zero-padded gotos must be retained"
    );
    let first_goto = forward_gotos
        .iter()
        .copied()
        .min_by_key(|block| {
            cfg.graph[block.index()]
                .stmts
                .iter()
                .map(|stmt| stmt.byte_range.start)
                .min()
                .expect("goto statement range")
        })
        .expect("forward goto");
    let backward_goto = forward_gotos
        .iter()
        .copied()
        .max_by_key(|block| {
            cfg.graph[block.index()]
                .stmts
                .iter()
                .map(|stmt| stmt.byte_range.start)
                .min()
                .expect("goto statement range")
        })
        .expect("backward goto");
    let forward_body = block_with_stmt(cfg, &source, "statement", "ForwardBody");
    let back_body = block_with_stmt(cfg, &source, "statement", "BackwardBody");
    let named_goto = block_with_stmt(cfg, &source, "goto", "goto Named");
    let named_body = block_with_stmt(cfg, &source, "statement", "NamedBody");
    let skipped_forward = block_with_stmt(cfg, &source, "statement", "SkippedForward");
    let skipped_backward = block_with_stmt(cfg, &source, "statement", "SkippedBackward");

    assert_eq!(
        successors(cfg, first_goto),
        vec![(forward_body, EdgeKind::Goto)],
        "a numeric forward goto must resolve despite a different number of leading zeros"
    );
    assert_eq!(
        successors(cfg, backward_goto),
        vec![(forward_body, EdgeKind::Goto)],
        "a numeric backward goto must use the canonical label name"
    );
    let goto_42 = block_with_stmt(cfg, &source, "goto", "goto 42");
    assert_eq!(
        successors(cfg, goto_42),
        vec![(back_body, EdgeKind::Goto)],
        "a decimal label must target the matching definition"
    );
    assert_eq!(
        successors(cfg, named_goto),
        vec![(named_body, EdgeKind::Goto)],
        "identifier labels must remain a separate namespace spelling"
    );
    assert!(!can_reach(cfg, first_goto, skipped_forward));
    assert!(!can_reach(cfg, backward_goto, skipped_backward));
}

#[test]
fn numeric_labels_work_through_finally_and_case_literals_stay_literals() {
    let source = br#"
unit NumericFinally;
interface
implementation

procedure NumericFinally;
label 100;
begin
  try
    goto 0100;
  finally
    goto 100;
  end;
100:
  TargetBody;
end;

procedure CaseLiteral;
begin
  case Choice of
    1: CaseBody;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let mut nodes = Vec::new();
    walk_nodes(tree.root_node(), &mut nodes, &source);
    assert!(
        nodes
            .iter()
            .any(|(kind, text)| kind == "labelNumber" && text == "0100"),
        "numeric goto must use the dedicated labelNumber token"
    );
    assert!(
        nodes
            .iter()
            .any(|(kind, text)| kind == "literalNumber" && text == "1"),
        "case labels must continue using literalNumber"
    );

    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "NumericFinally");
    let target = block_with_stmt(cfg, &source, "statement", "TargetBody");
    let cleanup_blocks = blocks_with_stmt(cfg, &source, "goto", "goto 100");
    assert!(
        !cleanup_blocks.is_empty(),
        "finally goto must be represented"
    );

    let gotos = blocks_with_stmt(cfg, &source, "goto", "goto ");
    assert!(
        gotos.len() >= 2,
        "try and finally numeric gotos must be retained"
    );
    assert!(
        gotos.iter().any(|goto| {
            successors(cfg, *goto)
                .iter()
                .any(|(_, kind)| *kind == EdgeKind::FinallyEntry)
        }),
        "a numeric goto leaving try/finally must enter cleanup"
    );
    assert!(
        cleanup_blocks.iter().any(|cleanup| {
            successors(cfg, *cleanup)
                .iter()
                .any(|(successor, kind)| *successor == target && *kind == EdgeKind::FinallyExit)
        }),
        "the finally numeric goto must leave cleanup at the canonical target"
    );
}

#[test]
fn signed_real_and_hex_gotos_are_not_accepted_as_numeric_labels() {
    for source in [
        b"program NegativeLabel; begin goto -1; end.".as_slice(),
        b"program RealLabel; begin goto 1.0; end.".as_slice(),
        b"program HexLabel; begin goto $1; end.".as_slice(),
    ] {
        assert!(
            parse(source).root_node().has_error(),
            "non-decimal goto label must remain a parse error: {}",
            String::from_utf8_lossy(source)
        );
    }
}
