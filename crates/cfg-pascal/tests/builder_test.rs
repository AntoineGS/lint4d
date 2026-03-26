use cfg_core::{BasicBlockKind, EdgeKind};
use cfg_pascal::build_file_cfgs;
use tree_sitter::Parser;

fn load_fixture(name: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/{}", name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read fixture {}: {}", path, e))
}

fn parse(source: &[u8]) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    let lang = tree_sitter_pascal::LANGUAGE;
    parser
        .set_language(&lang.into())
        .expect("failed to set language");
    parser.parse(source, None).expect("parse failed")
}

fn find_cfg_by_name<'a>(cfgs: &'a [cfg_core::Cfg], name: &str) -> &'a cfg_core::Cfg {
    cfgs.iter()
        .find(|c| c.proc_name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = cfgs.iter().map(|c| c.proc_name.as_str()).collect();
            panic!(
                "CFG for '{}' not found. Available: {:?}",
                name, names
            )
        })
}

fn has_edge_kind(cfg: &cfg_core::Cfg, kind: &EdgeKind) -> bool {
    cfg.graph
        .edge_indices()
        .any(|e| cfg.graph[e] == *kind)
}

#[test]
fn linear_proc_has_entry_body_exit() {
    let source = load_fixture("simple_proc.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "SimpleLinear");

    // entry -> body -> exit = 3 blocks, 2 edges
    assert_eq!(
        cfg.graph.node_count(),
        3,
        "SimpleLinear should have 3 blocks (entry, body, exit)"
    );
    assert_eq!(
        cfg.graph.edge_count(),
        2,
        "SimpleLinear should have 2 edges"
    );

    // Verify block kinds
    let entry_kind = &cfg.graph[cfg.entry.index()].kind;
    let exit_kind = &cfg.graph[cfg.exit.index()].kind;
    assert_eq!(*entry_kind, BasicBlockKind::Entry);
    assert_eq!(*exit_kind, BasicBlockKind::Exit);

    // Body block should have statements
    let body_block_idx = cfg
        .graph
        .node_indices()
        .find(|&idx| {
            let b = &cfg.graph[idx];
            b.kind == BasicBlockKind::Normal && !b.stmts.is_empty()
        })
        .expect("should have a normal block with statements");
    let body = &cfg.graph[body_block_idx];
    assert!(
        body.stmts.len() >= 2,
        "body should have at least 2 statements, got {}",
        body.stmts.len()
    );

    // Entry is reachable to exit
    assert!(cfg.is_reachable(cfg.entry, cfg.exit));
}

#[test]
fn if_else_creates_diamond() {
    let source = load_fixture("simple_proc.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "SimpleIfElse");

    // Should have ConditionalTrue and ConditionalFalse edges
    assert!(
        has_edge_kind(cfg, &EdgeKind::ConditionalTrue),
        "SimpleIfElse should have ConditionalTrue edge"
    );
    assert!(
        has_edge_kind(cfg, &EdgeKind::ConditionalFalse),
        "SimpleIfElse should have ConditionalFalse edge"
    );

    // Entry is reachable to exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit"
    );

    // Should have at least 5 blocks: entry, condition/body, then, else, join, exit
    // (the condition stmt may be on the body block that precedes the branch)
    assert!(
        cfg.graph.node_count() >= 5,
        "SimpleIfElse should have at least 5 blocks, got {}",
        cfg.graph.node_count()
    );
}

#[test]
fn raise_terminates_block() {
    let source = load_fixture("simple_proc.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "SimpleRaise");

    // entry reaches exit (via raise -> ExceptionThrow edge to exit)
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit via raise"
    );

    // Should have an ExceptionThrow edge
    assert!(
        has_edge_kind(cfg, &EdgeKind::ExceptionThrow),
        "SimpleRaise should have ExceptionThrow edge"
    );
}

#[test]
fn exit_connects_to_exit_block() {
    let source = load_fixture("simple_proc.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "SimpleExit");

    // entry reaches exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit"
    );

    // Should have at least 4 blocks: entry, pre-if body, then (exit), join/post, exit
    assert!(
        cfg.graph.node_count() >= 4,
        "SimpleExit should have at least 4 blocks, got {}",
        cfg.graph.node_count()
    );

    // Should have conditional edges (from the if)
    assert!(
        has_edge_kind(cfg, &EdgeKind::ConditionalTrue),
        "SimpleExit should have ConditionalTrue edge"
    );
    assert!(
        has_edge_kind(cfg, &EdgeKind::ConditionalFalse),
        "SimpleExit should have ConditionalFalse edge"
    );
}

#[test]
fn all_four_procs_found() {
    let source = load_fixture("simple_proc.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    assert_eq!(
        cfgs.len(),
        4,
        "should find 4 procedures in simple_proc.pas"
    );

    let names: Vec<&str> = cfgs.iter().map(|c| c.proc_name.as_str()).collect();
    assert!(names.contains(&"SimpleLinear"));
    assert!(names.contains(&"SimpleIfElse"));
    assert!(names.contains(&"SimpleRaise"));
    assert!(names.contains(&"SimpleExit"));
}
