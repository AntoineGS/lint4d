use cfg_core::{
    BasicBlockKind, BlockId, CfgBuildSink, Cfg, DefaultCfgBuilder, EdgeKind, StmtRef,
};
use tree_sitter::Node;

use crate::constructs::{is_exit_call, node_text};

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

            "statement" => {
                // A `statement` node can wrap Exit, Break, Continue, or other calls.
                if is_exit_call(child, ctx.source) {
                    // Exit terminates the current block and goes to the exit block.
                    add_stmt_ref(ctx, current, child);
                    ctx.builder
                        .add_edge(current, ctx.exit, EdgeKind::Normal);
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
