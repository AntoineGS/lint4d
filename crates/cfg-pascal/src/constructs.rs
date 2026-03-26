use cfg_core::BlockId;
use tree_sitter::Node;

/// Extract the UTF-8 text of a node from the source bytes.
pub(crate) fn node_text(node: Node, source: &[u8]) -> String {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}

/// Check whether a node represents a call to `Exit`.
///
/// In tree-sitter-pascal, a standalone `Exit;` is parsed as a `statement`
/// node containing an `identifier` child with text "Exit" (case-insensitive).
/// It can also appear as a bare `identifier` child of a `block`.
pub(crate) fn is_exit_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "statement" => {
            // statement wrapping an identifier
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    let text = node_text(child, source);
                    if text.eq_ignore_ascii_case("exit") {
                        return true;
                    }
                }
            }
            false
        }
        "identifier" => {
            let text = node_text(node, source);
            text.eq_ignore_ascii_case("exit")
        }
        "exprCall" => {
            // Exit(...) with a return value
            if let Some(entity) = node.child_by_field_name("entity") {
                if entity.kind() == "identifier" {
                    let text = node_text(entity, source);
                    return text.eq_ignore_ascii_case("exit");
                }
            }
            false
        }
        _ => false,
    }
}

/// Check whether a node represents a call to `Break`.
pub(crate) fn is_break_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    let text = node_text(child, source);
                    if text.eq_ignore_ascii_case("break") {
                        return true;
                    }
                }
            }
            false
        }
        "identifier" => {
            let text = node_text(node, source);
            text.eq_ignore_ascii_case("break")
        }
        _ => false,
    }
}

/// Check whether a node represents a call to `Continue`.
pub(crate) fn is_continue_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    let text = node_text(child, source);
                    if text.eq_ignore_ascii_case("continue") {
                        return true;
                    }
                }
            }
            false
        }
        "identifier" => {
            let text = node_text(node, source);
            text.eq_ignore_ascii_case("continue")
        }
        _ => false,
    }
}

/// Context for exception handling: tracks the target blocks when an
/// exception is raised or a `finally` needs to be entered.
pub(crate) struct ExceptionFrame {
    pub finally_entry: Option<BlockId>,
    pub except_entry: Option<BlockId>,
}

/// Context for loop constructs: tracks where `break` and `continue` jump to.
pub(crate) struct LoopFrame {
    pub continue_target: BlockId,
    pub break_target: BlockId,
}
