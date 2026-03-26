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
            panic!("CFG for '{}' not found. Available: {:?}", name, names)
        })
}

fn has_edge_kind(cfg: &cfg_core::Cfg, kind: &EdgeKind) -> bool {
    cfg.graph.edge_indices().any(|e| cfg.graph[e] == *kind)
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

    assert_eq!(cfgs.len(), 4, "should find 4 procedures in simple_proc.pas");

    let names: Vec<&str> = cfgs.iter().map(|c| c.proc_name.as_str()).collect();
    assert!(names.contains(&"SimpleLinear"));
    assert!(names.contains(&"SimpleIfElse"));
    assert!(names.contains(&"SimpleRaise"));
    assert!(names.contains(&"SimpleExit"));
}

#[test]
fn for_loop_has_back_edge() {
    let source = load_fixture("loops.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestForLoop");

    // A for loop should produce a LoopBack edge (body -> condition)
    assert!(
        has_edge_kind(cfg, &EdgeKind::LoopBack),
        "for loop should have a LoopBack edge"
    );

    // Entry should reach exit (loop can exit via LoopExit)
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit through for loop"
    );
}

#[test]
fn while_loop_has_back_edge() {
    let source = load_fixture("loops.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestWhileLoop");

    // A while loop should produce a LoopBack edge (body -> condition)
    assert!(
        has_edge_kind(cfg, &EdgeKind::LoopBack),
        "while loop should have a LoopBack edge"
    );

    // Entry should reach exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit through while loop"
    );
}

#[test]
fn repeat_until_has_back_edge() {
    let source = load_fixture("loops.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestRepeatUntil");

    // A repeat..until loop should produce a LoopBack edge (condition -> body)
    assert!(
        has_edge_kind(cfg, &EdgeKind::LoopBack),
        "repeat..until should have a LoopBack edge"
    );

    // Entry should reach exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit through repeat..until loop"
    );
}

#[test]
fn break_in_loop_reaches_exit() {
    let source = load_fixture("loops.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestBreakInLoop");

    // Entry should reach exit (break provides a path out of the loop)
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit via break in loop"
    );
}

#[test]
fn try_finally_has_exception_and_finally_edges() {
    let source = load_fixture("try_blocks.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestTryFinally");

    // Should have a FinallyHandler block
    let has_finally_handler = cfg
        .graph
        .node_indices()
        .any(|idx| cfg.graph[idx].kind == BasicBlockKind::FinallyHandler);
    assert!(
        has_finally_handler,
        "TestTryFinally should have a FinallyHandler block"
    );

    // Should have FinallyEntry edge (normal path into finally)
    assert!(
        has_edge_kind(cfg, &EdgeKind::FinallyEntry),
        "TestTryFinally should have FinallyEntry edge"
    );

    // Should have FinallyExit edge (finally block exits to after)
    assert!(
        has_edge_kind(cfg, &EdgeKind::FinallyExit),
        "TestTryFinally should have FinallyExit edge"
    );

    // Should have ExceptionThrow edge (exception path into finally)
    assert!(
        has_edge_kind(cfg, &EdgeKind::ExceptionThrow),
        "TestTryFinally should have ExceptionThrow edge"
    );

    // Entry reaches exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit in try/finally"
    );
}

#[test]
fn try_except_has_exception_handler() {
    let source = load_fixture("try_blocks.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestTryExcept");

    // Should have an ExceptHandler block
    let has_except_handler = cfg
        .graph
        .node_indices()
        .any(|idx| cfg.graph[idx].kind == BasicBlockKind::ExceptHandler);
    assert!(
        has_except_handler,
        "TestTryExcept should have an ExceptHandler block"
    );

    // Should have ExceptionThrow edge (raise in try body -> except handler)
    assert!(
        has_edge_kind(cfg, &EdgeKind::ExceptionThrow),
        "TestTryExcept should have ExceptionThrow edge"
    );

    // Entry reaches exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit in try/except"
    );
}

#[test]
fn nested_try_reachable() {
    let source = load_fixture("try_blocks.pas");
    let tree = parse(&source);
    let cfgs = build_file_cfgs(&tree, &source);

    let cfg = find_cfg_by_name(&cfgs, "TestNestedTryFinallyExcept");

    // Should have both FinallyHandler and ExceptHandler blocks
    let has_finally_handler = cfg
        .graph
        .node_indices()
        .any(|idx| cfg.graph[idx].kind == BasicBlockKind::FinallyHandler);
    let has_except_handler = cfg
        .graph
        .node_indices()
        .any(|idx| cfg.graph[idx].kind == BasicBlockKind::ExceptHandler);
    assert!(
        has_finally_handler,
        "nested try should have a FinallyHandler block"
    );
    assert!(
        has_except_handler,
        "nested try should have an ExceptHandler block"
    );

    // Entry reaches exit
    assert!(
        cfg.is_reachable(cfg.entry, cfg.exit),
        "entry should reach exit in nested try"
    );
}
