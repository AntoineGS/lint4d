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
        "legacy-unit fixture must not contain parser errors:\n{}",
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

#[test]
fn legacy_unit_block_is_initialization_and_explicit_sections_remain_distinct() {
    let source = br#"
unit LegacyUnit;
interface
implementation
begin
  LegacyInit;
end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    assert_eq!(
        cfgs.iter()
            .map(|cfg| cfg.proc_name.as_str())
            .collect::<Vec<_>>(),
        vec!["LegacyUnit.<initialization>"]
    );
    let initialization = cfg_for(&cfgs, "LegacyUnit.<initialization>");
    let init = block_with_stmt(initialization, &source, "statement", "LegacyInit");
    // Like an explicit `initialization` section, the legacy section spans its
    // keyword and statements; the closing `end.` belongs to the unit.
    let section_text = b"begin\n  LegacyInit;";
    let section_start = source
        .windows(section_text.len())
        .position(|window| window == section_text)
        .expect("legacy unit initialization");
    assert_eq!(
        initialization.byte_range,
        section_start..section_start + section_text.len()
    );
    assert!(successors(initialization, init)
        .iter()
        .any(|(target, kind)| *target == initialization.exit && *kind == EdgeKind::Normal));

    let explicit_source = br#"
unit ExplicitSections;
interface
implementation
initialization
  ExplicitInit;
finalization
  ExplicitFinal;
end.
"#
    .to_vec();
    let explicit_tree = parse_clean(&explicit_source);
    let explicit_cfgs = build_file_cfgs(&explicit_tree, &explicit_source);
    assert_eq!(
        explicit_cfgs
            .iter()
            .map(|cfg| cfg.proc_name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "ExplicitSections.<initialization>",
            "ExplicitSections.<finalization>"
        ]
    );
}

#[test]
fn legacy_unit_begin_keyword_is_not_a_statement() {
    let source = br#"
unit LegacyUnit;
interface
implementation
begin
  LegacyInit;
end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let initialization = cfg_for(&cfgs, "LegacyUnit.<initialization>");

    let kinds: Vec<&str> = initialization
        .graph
        .node_indices()
        .flat_map(|index| initialization.graph[index].stmts.iter())
        .map(|stmt| stmt.node_kind.as_str())
        .collect();
    assert_eq!(kinds, vec!["statement"]);
}
