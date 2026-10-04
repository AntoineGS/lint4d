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
        "bare-raise fixture must not contain parser errors:\n{}",
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

fn successors(cfg: &Cfg, from: BlockId) -> Vec<(BlockId, EdgeKind)> {
    cfg.graph
        .edge_indices()
        .filter_map(|edge| {
            let (source, target) = cfg.graph.edge_endpoints(edge)?;
            (source == from.index()).then(|| (BlockId::from(target), cfg.graph[edge].clone()))
        })
        .collect()
}

#[test]
fn bare_raise_is_parse_clean_and_keeps_the_existing_exception_transfer() {
    let source = br#"
unit BareRaise;
interface
implementation

procedure Reraise;
begin
  try
    Work;
  except
    on E: Exception do
      raise;
  end;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "Reraise");
    let raises = blocks_with_exact_stmt(cfg, &source, "raise", "raise;");
    assert_eq!(
        raises.len(),
        1,
        "bare raise must be retained as a statement"
    );
    assert_eq!(
        successors(cfg, raises[0]),
        vec![(cfg.exit, EdgeKind::ExceptionThrow)],
        "bare raise keeps the current Task 1 unknown-exception transfer"
    );
}
