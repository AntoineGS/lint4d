use cfg_core::{BasicBlockKind, BlockId, Cfg, CfgBuildSink, DefaultCfgBuilder, EdgeKind, StmtRef};
use tree_sitter::Node;

use crate::constructs::{
    is_break_call, is_continue_call, is_exit_call, node_text, ExceptionFrame, LoopFrame,
};

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
        exception_stack: Vec::new(),
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
    exception_stack: Vec<ExceptionFrame>,
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

            "ifElse" => match handle_if_else(ctx, child, current) {
                Some(join) => current = join,
                None => return None,
            },

            "if" => match handle_if_only(ctx, child, current) {
                Some(join) => current = join,
                None => return None,
            },

            "raise" => {
                handle_raise(ctx, child, current);
                return None;
            }

            "try" => match handle_try(ctx, child, current) {
                Some(after) => current = after,
                None => return None,
            },

            "for" | "while" => match handle_for_or_while(ctx, child, current) {
                Some(after) => current = after,
                None => return None,
            },

            "repeat" => match handle_repeat(ctx, child, current) {
                Some(after) => current = after,
                None => return None,
            },

            "statement" => {
                // A `statement` node can wrap Exit, Break, Continue, or other calls.
                if is_exit_call(child, ctx.source) {
                    // Exit terminates the current block and goes to the exit block.
                    add_stmt_ref(ctx, current, child);
                    ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
                    ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
    // Add the if condition as a statement on the current block so that
    // dataflow analysis sees variable references in the condition expression.
    if let Some(cond) = node.child_by_field_name("condition") {
        add_stmt_ref(ctx, current, cond);
    }

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
                ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
                    ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
                ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
                    ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
fn handle_for_or_while(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    let cond_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let body_block = ctx.builder.new_block(BasicBlockKind::Normal);
    let after_block = ctx.builder.new_block(BasicBlockKind::Normal);

    // Add the condition expression as a statement on the cond_block.
    // For `for`: the init assignment and limit are the "condition" concept.
    // For `while`: the condition expression precedes kDo.
    add_stmt_ref(ctx, cond_block, node);

    // current -> cond_block
    ctx.builder.add_edge(current, cond_block, EdgeKind::Normal);
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
    ctx.builder.add_edge(current, body_block, EdgeKind::Normal);

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
fn process_single_stmt(ctx: &mut BuildContext, child: Node, current: BlockId) -> Option<BlockId> {
    match child.kind() {
        "block" => walk_block_stmts(ctx, child, current),
        "ifElse" => handle_if_else(ctx, child, current),
        "if" => handle_if_only(ctx, child, current),
        "raise" => {
            handle_raise(ctx, child, current);
            None
        }
        "try" => handle_try(ctx, child, current),
        "for" | "while" => handle_for_or_while(ctx, child, current),
        "repeat" => handle_repeat(ctx, child, current),
        "statement" if is_exit_call(child, ctx.source) => {
            add_stmt_ref(ctx, current, child);
            ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
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
                ctx.builder.add_edge(current, ctx.exit, EdgeKind::Normal);
                return None;
            }
            add_stmt_ref(ctx, current, child);
            Some(current)
        }
    }
}

/// Handle a `try` node (try..finally or try..except).
///
/// Determines whether the try block has a `finally` or `except` section
/// by scanning children for `kFinally` or `kExcept`, then delegates
/// to the appropriate handler.
fn handle_try(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    let mut has_finally = false;
    let mut has_except = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "kFinally" => has_finally = true,
            "kExcept" => has_except = true,
            _ => {}
        }
    }

    if has_finally {
        handle_try_finally(ctx, node, current)
    } else if has_except {
        handle_try_except(ctx, node, current)
    } else {
        // Malformed try block — treat as a plain block
        Some(current)
    }
}

/// Handle a `try..finally` block.
///
/// CFG pattern:
///   current -> [try body] --FinallyEntry--> finally_block --FinallyExit--> after
///   (any raise in try body) --ExceptionThrow--> finally_block
fn handle_try_finally(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    let finally_block = ctx.builder.new_block(BasicBlockKind::FinallyHandler);
    let after_block = ctx.builder.new_block(BasicBlockKind::Normal);

    // Implicit exception edge: any statement in the try body could throw,
    // so add an ExceptionThrow edge from the current block to finally.
    ctx.builder
        .add_edge(current, finally_block, EdgeKind::ExceptionThrow);

    // Push exception frame so raise routes to finally
    ctx.exception_stack.push(ExceptionFrame {
        finally_entry: Some(finally_block),
        except_entry: None,
    });

    // Walk the try body (the `statements` node between kTry and kFinally)
    let try_body_end = walk_try_body(ctx, node, current);

    ctx.exception_stack.pop();

    // Normal path: try body falls through to finally
    if let Some(tb) = try_body_end {
        ctx.builder
            .add_edge(tb, finally_block, EdgeKind::FinallyEntry);
    }

    // Walk the finally body (the `statements` node after kFinally)
    let finally_end = walk_finally_body(ctx, node, finally_block);

    // Finally exits to the after block
    if let Some(fb) = finally_end {
        ctx.builder.add_edge(fb, after_block, EdgeKind::FinallyExit);
    }

    Some(after_block)
}

/// Handle a `try..except` block.
///
/// CFG pattern:
///   current -> [try body] --Normal--> after (no exception)
///   (any raise in try body) --ExceptionThrow--> except_block
///   except_block -> [handler body] --Normal--> after
fn handle_try_except(ctx: &mut BuildContext, node: Node, current: BlockId) -> Option<BlockId> {
    let except_block = ctx.builder.new_block(BasicBlockKind::ExceptHandler);
    let after_block = ctx.builder.new_block(BasicBlockKind::Normal);

    // Implicit exception edge: any statement in the try body could throw,
    // so add an ExceptionThrow edge from the current block to except handler.
    ctx.builder
        .add_edge(current, except_block, EdgeKind::ExceptionThrow);

    // Push exception frame so raise routes to except handler
    ctx.exception_stack.push(ExceptionFrame {
        finally_entry: None,
        except_entry: Some(except_block),
    });

    // Walk the try body (the `statements` node between kTry and kExcept)
    let try_body_end = walk_try_body(ctx, node, current);

    ctx.exception_stack.pop();

    // Normal path: try body falls through to after (no exception raised)
    if let Some(tb) = try_body_end {
        ctx.builder.add_edge(tb, after_block, EdgeKind::Normal);
    }

    // Walk the except handler bodies
    let except_end = walk_except_handlers(ctx, node, except_block);

    // Except handler falls through to after
    if let Some(eb) = except_end {
        ctx.builder.add_edge(eb, after_block, EdgeKind::Normal);
    }

    Some(after_block)
}

/// Walk the try body: the `statements` node that appears between kTry and kFinally/kExcept.
///
/// Continues from `current`, which is the block before or at the try statement.
fn walk_try_body(ctx: &mut BuildContext, try_node: Node, current: BlockId) -> Option<BlockId> {
    let mut cursor = try_node.walk();
    let mut past_try = false;

    for child in try_node.children(&mut cursor) {
        match child.kind() {
            "kTry" => {
                past_try = true;
                continue;
            }
            "kFinally" | "kExcept" => break,
            _ if !past_try => continue,
            _ => {}
        }

        if child.kind() == "statements" {
            return walk_statements_node(ctx, child, current);
        }
    }

    Some(current)
}

/// Walk a `statements` node, processing each child statement.
fn walk_statements_node(
    ctx: &mut BuildContext,
    statements_node: Node,
    mut current: BlockId,
) -> Option<BlockId> {
    let mut cursor = statements_node.walk();
    for child in statements_node.children(&mut cursor) {
        match child.kind() {
            ";" => continue,
            _ => match process_single_stmt(ctx, child, current) {
                Some(next) => current = next,
                None => return None,
            },
        }
    }
    Some(current)
}

/// Walk the finally body: the `statements` node that appears after kFinally.
fn walk_finally_body(
    ctx: &mut BuildContext,
    try_node: Node,
    finally_block: BlockId,
) -> Option<BlockId> {
    let mut cursor = try_node.walk();
    let mut past_finally = false;

    for child in try_node.children(&mut cursor) {
        match child.kind() {
            "kFinally" => {
                past_finally = true;
                continue;
            }
            "kEnd" | ";" => continue,
            _ if !past_finally => continue,
            _ => {}
        }

        if child.kind() == "statements" {
            return walk_statements_node(ctx, child, finally_block);
        }
    }

    Some(finally_block)
}

/// Walk the except handlers: `exceptionHandler` nodes after kExcept.
///
/// Each `exceptionHandler` has: kOn, identifier, `:`, typeref, kDo, statement/block.
fn walk_except_handlers(
    ctx: &mut BuildContext,
    try_node: Node,
    except_block: BlockId,
) -> Option<BlockId> {
    let mut cursor = try_node.walk();
    let mut past_except = false;
    let mut current = except_block;

    for child in try_node.children(&mut cursor) {
        match child.kind() {
            "kExcept" => {
                past_except = true;
                continue;
            }
            "kEnd" | ";" => continue,
            _ if !past_except => continue,
            _ => {}
        }

        if child.kind() == "exceptionHandler" {
            // Process the handler body: find the statement/block after kDo
            match walk_exception_handler_body(ctx, child, current) {
                Some(next) => current = next,
                None => return None,
            }
        }
    }

    Some(current)
}

/// Walk the body of a single `exceptionHandler` node.
///
/// Structure: kOn, identifier, `:`, typeref, kDo, statement/block
fn walk_exception_handler_body(
    ctx: &mut BuildContext,
    handler_node: Node,
    current: BlockId,
) -> Option<BlockId> {
    let mut past_do = false;
    let mut cursor = handler_node.walk();

    for child in handler_node.children(&mut cursor) {
        match child.kind() {
            "kDo" => {
                past_do = true;
                continue;
            }
            _ if !past_do => continue,
            ";" => continue,
            _ => {}
        }

        return process_single_stmt(ctx, child, current);
    }

    Some(current)
}

/// Handle a `raise` statement: adds the statement to the current block and
/// creates an edge to the appropriate exception target.
///
/// If inside a try/except, routes to the except handler.
/// If inside a try/finally, routes to the finally handler.
/// Otherwise, routes to exit.
fn handle_raise(ctx: &mut BuildContext, node: Node, current: BlockId) {
    add_stmt_ref(ctx, current, node);
    let target = exception_target(ctx);
    ctx.builder
        .add_edge(current, target, EdgeKind::ExceptionThrow);
}

/// Determine the target block for an exception throw.
///
/// Walks the exception stack from innermost to outermost:
/// - If the frame has an `except_entry`, that's the target.
/// - If the frame has a `finally_entry`, that's the target.
/// - Otherwise, falls through to the procedure exit block.
fn exception_target(ctx: &BuildContext) -> BlockId {
    for frame in ctx.exception_stack.iter().rev() {
        if let Some(except) = frame.except_entry {
            return except;
        }
        if let Some(finally) = frame.finally_entry {
            return finally;
        }
    }
    ctx.exit
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
