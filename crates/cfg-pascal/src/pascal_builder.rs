use cfg_core::{
    BasicBlockKind, BlockId, CfgBuildSink, Cfg, DefaultCfgBuilder, EdgeKind, StmtRef,
};
use tree_sitter::Node;

use crate::constructs::{is_break_call, is_continue_call, is_exit_call, node_text, LoopFrame};

/// Build CFGs for all procedure/function definitions in a parsed Pascal file.
///
/// Walks the tree looking for `defProc` nodes, extracts the procedure name
/// and body block, then builds a CFG for each one.
pub fn build_file_cfgs(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<Cfg> {
    let mut cfgs = Vec::new();
    collect_def_proc_cfgs(tree.root_node(), source, &mut cfgs);
    cfgs
}

fn collect_def_proc_cfgs(node: Node, source: &[u8], out: &mut Vec<Cfg>) {
    if node.kind() == "defProc" {
        if let Some(cfg) = build_proc_cfg(node, source) {
            out.push(cfg);
        }
        // Don't recurse into defProc to avoid nested procedure confusion.
        // Nested procedures would be their own defProc children and are
        // collected separately.
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_def_proc_cfgs(child, source, out);
    }
}

/// Build a CFG for a single `defProc` node.
fn build_proc_cfg(def_proc: Node, source: &[u8]) -> Option<Cfg> {
    let proc_name = extract_proc_name(def_proc, source)?;
    let block = def_proc.child_by_field_name("body")?;

    let byte_range = def_proc.start_byte()..def_proc.end_byte();
    let mut builder = DefaultCfgBuilder::new(proc_name, byte_range);

    let entry = builder.new_block(BasicBlockKind::Entry);
    let exit = builder.new_block(BasicBlockKind::Exit);
    builder.set_entry(entry);
    builder.set_exit(exit);

    let body = builder.new_block(BasicBlockKind::Normal);
    builder.add_edge(entry, body, EdgeKind::Normal);

    let mut ctx = BuildContext {
        builder: &mut builder,
        exit,
        source,
        loop_stack: Vec::new(),
    };

    let final_block = walk_block_stmts(&mut ctx, block, body);

    // Connect the final block to exit if it isn't already terminated.
    if let Some(fb) = final_block {
        ctx.builder.add_edge(fb, exit, EdgeKind::Normal);
    }

    Some(builder.finish())
}

/// Extract the procedure/function name from a `defProc` node.
///
/// For standalone procedures: `declProc > identifier`
/// For methods: `declProc > genericDot > identifier, identifier`
fn extract_proc_name(def_proc: Node, source: &[u8]) -> Option<String> {
    let decl_proc = if let Some(header) = def_proc.child_by_field_name("header") {
        header
    } else {
        let mut cursor = def_proc.walk();
        let found = def_proc
            .children(&mut cursor)
            .find(|c| c.kind() == "declProc");
        match found {
            Some(n) => n,
            None => return None,
        }
    };

    // Try genericDot first (for method implementations like TClass.Method)
    let mut cursor = decl_proc.walk();
    if let Some(generic_dot) = decl_proc
        .children(&mut cursor)
        .find(|c| c.kind() == "genericDot")
    {
        let idents: Vec<Node> = generic_dot
            .children(&mut generic_dot.walk())
            .filter(|c| c.kind() == "identifier")
            .collect();

        if idents.len() >= 2 {
            return Some(format!(
                "{}.{}",
                node_text(idents[0], source),
                node_text(idents[1], source)
            ));
        }
        if !idents.is_empty() {
            return Some(node_text(idents[0], source));
        }
    }

    // Try the `name` field
    if let Some(name_node) = decl_proc.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    // Fallback: first direct identifier child
    let mut cursor2 = decl_proc.walk();
    for child in decl_proc.children(&mut cursor2) {
        if child.kind() == "identifier" {
            return Some(node_text(child, source));
        }
    }

    None
}

/// Mutable context passed through the CFG building walk.
struct BuildContext<'a> {
    builder: &'a mut DefaultCfgBuilder,
    exit: BlockId,
    source: &'a [u8],
    loop_stack: Vec<LoopFrame>,
}

/// Walk the children of a `block` node, building CFG blocks and edges.
///
/// Returns `Some(block_id)` for the block that control falls through to,
/// or `None` if control does not fall through (e.g., raise/exit terminated it).
fn walk_block_stmts(ctx: &mut BuildContext, block: Node, mut current: BlockId) -> Option<BlockId> {
    let mut cursor = block.walk();
    for child in block.children(&mut cursor) {
        match child.kind() {
            // Skip structural tokens
            "kBegin" | "kEnd" | ";" | "declVars" | "declConsts" | "declTypes" => continue,

            "ifElse" => {
                match handle_if_else(ctx, child, current) {
                    Some(join) => current = join,
                    None => return None,
                }
            }

            "if" => {
                match handle_if_only(ctx, child, current) {
                    Some(join) => current = join,
                    None => return None,
                }
            }

            "raise" => {
                handle_raise(ctx, child, current);
                return None;
            }

            "for" | "while" => {
                match handle_for_or_while(ctx, child, current) {
                    Some(after) => current = after,
                    None => return None,
                }
            }

            "repeat" => {
                match handle_repeat(ctx, child, current) {
                    Some(after) => current = after,
                    None => return None,
                }
            }

            "statement" => {
                // A `statement` node can wrap Exit, Break, Continue, or other calls.
                if is_exit_call(child, ctx.source) {
                    // Exit terminates the current block and goes to the exit block.
                    add_stmt_ref(ctx, current, child);
                    ctx.builder
                        .add_edge(current, ctx.exit, EdgeKind::Normal);
                    return None;
                }
                if is_break_call(child, ctx.source) {
                    add_stmt_ref(ctx, current, child);
                    if let Some(frame) = ctx.loop_stack.last() {
                        ctx.builder
                            .add_edge(current, frame.break_target, EdgeKind::Normal);
                    }
                    return None;
                }
                if is_continue_call(child, ctx.source) {
                    add_stmt_ref(ctx, current, child);
                    if let Some(frame) = ctx.loop_stack.last() {
                        ctx.builder
                            .add_edge(current, frame.continue_target, EdgeKind::Normal);
                    }
                    return None;
                }
                // Regular statement
                add_stmt_ref(ctx, current, child);
            }

            "block" => {
                // Nested begin..end block
                match walk_block_stmts(ctx, child, current) {
                    Some(after) => current = after,
                    None => return None,
                }
            }

            _ => {
                // Any other statement node (assignment, exprCall, etc.)
                // Check if it's an exit call at the top level
                if is_exit_call(child, ctx.source) {
                    add_stmt_ref(ctx, current, child);
                    ctx.builder
                        .add_edge(current, ctx.exit, EdgeKind::Normal);
                    return None;
                }
                add_stmt_ref(ctx, current, child);
            }
        }
    }

    Some(current)
}

/// Handle an `ifElse` node (if/then/else).
///
/// Creates a diamond pattern:
///   current --ConditionalTrue-->  then_block
///   current --ConditionalFalse--> else_block
///   then_block  --Normal--> join
///   else_block  --Normal--> join
///
/// Returns `Some(join)` if at least one branch falls through, `None` if both terminate.
fn handle_if_else(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    // Add the if condition as a statement on the current block
    if let Some(cond) = node.child_by_field_name("condition") {
        add_stmt_ref(ctx, current, cond);
    }

    let then_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let else_block = ctx.builder.new_block(BasicBlockKind::Normal);

    ctx.builder
        .add_edge(current, then_block, EdgeKind::ConditionalTrue);
    ctx.builder
        .add_edge(current, else_block, EdgeKind::ConditionalFalse);

    // Process then branch
    let then_end = process_branch_child(ctx, node, "then", then_block);

    // Process else branch
    let else_end = process_branch_child(ctx, node, "else", else_block);

    // Create join block if at least one branch falls through
    match (then_end, else_end) {
        (Some(t), Some(e)) => {
            let join = ctx.builder.new_block(BasicBlockKind::Normal);
            ctx.builder.add_edge(t, join, EdgeKind::Normal);
            ctx.builder.add_edge(e, join, EdgeKind::Normal);
            Some(join)
        }
        (Some(t), None) => {
            let join = ctx.builder.new_block(BasicBlockKind::Normal);
            ctx.builder.add_edge(t, join, EdgeKind::Normal);
            Some(join)
        }
        (None, Some(e)) => {
            let join = ctx.builder.new_block(BasicBlockKind::Normal);
            ctx.builder.add_edge(e, join, EdgeKind::Normal);
            Some(join)
        }
        (None, None) => None,
    }
}

/// Handle an `if` node (if/then without else).
///
/// Creates:
///   current --ConditionalTrue-->  then_block
///   current --ConditionalFalse--> join
///   then_block  --Normal--> join
///
/// Returns `Some(join)` always since the false branch always falls through.
fn handle_if_only(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    let then_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let join = ctx.builder.new_block(BasicBlockKind::Normal);

    ctx.builder
        .add_edge(current, then_block, EdgeKind::ConditionalTrue);
    ctx.builder
        .add_edge(current, join, EdgeKind::ConditionalFalse);

    // The then body of an `if` (without else) is a direct child.
    // It can be a `statement` node, a `block`, or another statement type.
    let then_end = process_if_then_children(ctx, node, then_block);

    if let Some(te) = then_end {
        ctx.builder.add_edge(te, join, EdgeKind::Normal);
    }

    Some(join)
}

/// Process the then-body children of an `if` node (no else).
///
/// The `if` node has children: kIf, condition, kThen, then-body-statement.
/// We need to find the then-body statement(s) after `kThen`.
fn process_if_then_children(
    ctx: &mut BuildContext,
    if_node: Node,
    then_block: BlockId,
) -> Option<BlockId> {
    let mut past_then = false;
    let current = then_block;
    let mut cursor = if_node.walk();

    for child in if_node.children(&mut cursor) {
        if child.kind() == "kThen" {
            past_then = true;
            continue;
        }
        if !past_then {
            continue;
        }
        // Skip semicolons
        if child.kind() == ";" {
            continue;
        }

        // Process the then-body statement
        match child.kind() {
            "raise" => {
                handle_raise(ctx, child, current);
                return None;
            }
            "statement" if is_exit_call(child, ctx.source) => {
                add_stmt_ref(ctx, current, child);
                ctx.builder
                    .add_edge(current, ctx.exit, EdgeKind::Normal);
                return None;
            }
            "block" => {
                return walk_block_stmts(ctx, child, current);
            }
            "ifElse" => {
                return handle_if_else(ctx, child, current);
            }
            "if" => {
                return handle_if_only(ctx, child, current);
            }
            _ => {
                if is_exit_call(child, ctx.source) {
                    add_stmt_ref(ctx, current, child);
                    ctx.builder
                        .add_edge(current, ctx.exit, EdgeKind::Normal);
                    return None;
                }
                add_stmt_ref(ctx, current, child);
            }
        }
    }

    Some(current)
}

/// Process a field-named branch child (used for "then" and "else" fields of ifElse).
fn process_branch_child(
    ctx: &mut BuildContext,
    parent: Node,
    field_name: &str,
    branch_block: BlockId,
) -> Option<BlockId> {
    let current = branch_block;
    let mut cursor = parent.walk();

    for child in parent.children_by_field_name(field_name, &mut cursor) {
        // Skip keyword and punctuation nodes
        match child.kind() {
            "kElse" | "kThen" | ";" => continue,
            _ => {}
        }

        match child.kind() {
            "raise" => {
                handle_raise(ctx, child, current);
                return None;
            }
            "statement" if is_exit_call(child, ctx.source) => {
                add_stmt_ref(ctx, current, child);
                ctx.builder
                    .add_edge(current, ctx.exit, EdgeKind::Normal);
                return None;
            }
            "block" => {
                return walk_block_stmts(ctx, child, current);
            }
            "ifElse" => {
                return handle_if_else(ctx, child, current);
            }
            "if" => {
                return handle_if_only(ctx, child, current);
            }
            _ => {
                if is_exit_call(child, ctx.source) {
                    add_stmt_ref(ctx, current, child);
                    ctx.builder
                        .add_edge(current, ctx.exit, EdgeKind::Normal);
                    return None;
                }
                add_stmt_ref(ctx, current, child);
            }
        }
    }

    Some(current)
}

/// Handle a `for` or `while` loop node.
///
/// CFG pattern:
///   current -> cond_block --ConditionalTrue-->  body_block
///                         --LoopExit-->         after_block
///   body_block -> cond_block (LoopBack)
///
/// Returns `Some(after_block)` always since the loop can exit via LoopExit.
fn handle_for_or_while(
    ctx: &mut BuildContext,
    node: Node,
    current: BlockId,
) -> Option<BlockId> {
    let cond_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let body_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let after_block = ctx.builder.new_block(BasicBlockKind::Normal);

    // Add the condition expression as a statement on the cond_block.
    // For `for`: the init assignment and limit are the "condition" concept.
    // For `while`: the condition expression precedes kDo.
    add_stmt_ref(ctx, cond_block, node);

    // current -> cond_block
    ctx.builder
        .add_edge(current, cond_block, EdgeKind::Normal);
    // cond_block -> body_block (loop enters)
    ctx.builder
        .add_edge(cond_block, body_block, EdgeKind::ConditionalTrue);
    // cond_block -> after_block (loop exits)
    ctx.builder
        .add_edge(cond_block, after_block, EdgeKind::LoopExit);

    // Push loop frame for break/continue
    ctx.loop_stack.push(LoopFrame {
        continue_target: cond_block,
        break_target: after_block,
    });

    // Find and process the body (the statement/block after kDo)
    let body_end = process_loop_body_after_kdo(ctx, node, body_block);

    ctx.loop_stack.pop();

    // body -> cond_block (LoopBack)
    if let Some(be) = body_end {
        ctx.builder.add_edge(be, cond_block, EdgeKind::LoopBack);
    }

    Some(after_block)
}

/// Handle a `repeat..until` loop node.
///
/// CFG pattern:
///   current -> body_block -> cond_block --LoopBack-->  body_block
///                                       --LoopExit-->  after_block
///
/// Returns `Some(after_block)`.
fn handle_repeat(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    let body_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let cond_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let after_block = ctx.builder.new_block(BasicBlockKind::Normal);

    // current -> body_block
    ctx.builder
        .add_edge(current, body_block, EdgeKind::Normal);

    // Push loop frame for break/continue
    ctx.loop_stack.push(LoopFrame {
        continue_target: cond_block,
        break_target: after_block,
    });

    // Process the body: children between kRepeat and kUntil.
    // The body is in a `statements` child node.
    let body_end = process_repeat_body(ctx, node, body_block);

    ctx.loop_stack.pop();

    let body_final = body_end.unwrap_or(body_block);

    // body -> cond_block
    ctx.builder
        .add_edge(body_final, cond_block, EdgeKind::Normal);

    // Add the until condition as a statement on the cond_block
    add_stmt_ref(ctx, cond_block, node);

    // cond_block -> body_block (LoopBack, condition false = keep looping)
    ctx.builder
        .add_edge(cond_block, body_block, EdgeKind::LoopBack);
    // cond_block -> after_block (LoopExit, condition true = exit)
    ctx.builder
        .add_edge(cond_block, after_block, EdgeKind::LoopExit);

    Some(after_block)
}

/// Process the body of a for/while loop: finds the statement after kDo and walks it.
fn process_loop_body_after_kdo(
    ctx: &mut BuildContext,
    loop_node: Node,
    body_block: BlockId,
) -> Option<BlockId> {
    let mut past_do = false;
    let mut cursor = loop_node.walk();

    for child in loop_node.children(&mut cursor) {
        if child.kind() == "kDo" {
            past_do = true;
            continue;
        }
        if !past_do {
            continue;
        }
        // Skip semicolons
        if child.kind() == ";" {
            continue;
        }

        // Process the body statement
        return process_single_stmt(ctx, child, body_block);
    }

    Some(body_block)
}

/// Process the body of a repeat..until loop: walks the `statements` child.
fn process_repeat_body(
    ctx: &mut BuildContext,
    repeat_node: Node,
    body_block: BlockId,
) -> Option<BlockId> {
    let mut current = body_block;
    let mut cursor = repeat_node.walk();

    for child in repeat_node.children(&mut cursor) {
        match child.kind() {
            "kRepeat" | "kUntil" | ";" => continue,
            "statements" => {
                // Walk the statements inside the repeat body
                let mut inner_cursor = child.walk();
                for stmt in child.children(&mut inner_cursor) {
                    match stmt.kind() {
                        ";" => continue,
                        _ => match process_single_stmt(ctx, stmt, current) {
                            Some(next) => current = next,
                            None => return None,
                        },
                    }
                }
                return Some(current);
            }
            _ => {
                // If the condition or other node types appear, skip them
                // (they come after kUntil)
            }
        }
    }

    Some(current)
}

/// Process a single statement node in a loop body or similar context.
///
/// Handles block, if, ifElse, raise, exit, break, continue, and other statements.
fn process_single_stmt(
    ctx: &mut BuildContext,
    child: Node,
    current: BlockId,
) -> Option<BlockId> {
    match child.kind() {
        "block" => walk_block_stmts(ctx, child, current),
        "ifElse" => handle_if_else(ctx, child, current),
        "if" => handle_if_only(ctx, child, current),
        "raise" => {
            handle_raise(ctx, child, current);
            None
        }
        "for" | "while" => handle_for_or_while(ctx, child, current),
        "repeat" => handle_repeat(ctx, child, current),
        "statement" if is_exit_call(child, ctx.source) => {
            add_stmt_ref(ctx, current, child);
            ctx.builder
                .add_edge(current, ctx.exit, EdgeKind::Normal);
            None
        }
        "statement" if is_break_call(child, ctx.source) => {
            add_stmt_ref(ctx, current, child);
            if let Some(frame) = ctx.loop_stack.last() {
                ctx.builder
                    .add_edge(current, frame.break_target, EdgeKind::Normal);
            }
            None
        }
        "statement" if is_continue_call(child, ctx.source) => {
            add_stmt_ref(ctx, current, child);
            if let Some(frame) = ctx.loop_stack.last() {
                ctx.builder
                    .add_edge(current, frame.continue_target, EdgeKind::Normal);
            }
            None
        }
        _ => {
            if is_exit_call(child, ctx.source) {
                add_stmt_ref(ctx, current, child);
                ctx.builder
                    .add_edge(current, ctx.exit, EdgeKind::Normal);
                return None;
            }
            add_stmt_ref(ctx, current, child);
            Some(current)
        }
    }
}

/// Handle a `raise` statement: adds the statement to the current block and
/// creates an edge to the exit (or exception handler, when implemented).
fn handle_raise(ctx: &mut BuildContext, node: Node, current: BlockId) {
    add_stmt_ref(ctx, current, node);
    // For now, raise always goes to exit. When try/except is implemented,
    // this will route to the exception handler instead.
    ctx.builder
        .add_edge(current, ctx.exit, EdgeKind::ExceptionThrow);
}

/// Add a `StmtRef` for a node to a block.
fn add_stmt_ref(ctx: &mut BuildContext, block: BlockId, node: Node) {
    ctx.builder.add_stmt(
        block,
        StmtRef {
            byte_range: node.start_byte()..node.end_byte(),
            node_kind: node.kind().to_string(),
        },
    );
}
