use cfg_core::BlockId;
use tree_sitter::Node;

use crate::exception_types::ExceptionTypeFact;

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
            // A statement wraps the expression for both `Exit;` and
            // `Exit(value);`. Delegate to the expression classifier so the
            // latter is not mistaken for an ordinary fallthrough statement.
            let mut cursor = node.walk();
            let is_exit = node
                .children(&mut cursor)
                .any(|child| is_exit_call(child, source));
            is_exit
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

/// Check whether an `Exit` call evaluates an argument before completing.
///
/// The argument expression is part of the protected computation, so it may
/// produce an exceptional completion independently of the explicit `Exit`
/// transfer. A bare `Exit;` has no such additional computation.
pub(crate) fn exit_has_argument(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "statement" => {
            let mut cursor = node.walk();
            let has_argument = node
                .children(&mut cursor)
                .any(|child| exit_has_argument(child, source));
            has_argument
        }
        "exprCall" => is_exit_call(node, source) && node.child_by_field_name("args").is_some(),
        _ => false,
    }
}

/// Check whether a node represents a call to `Break`.
pub(crate) fn is_break_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "statement" => {
            let mut cursor = node.walk();
            let result = node
                .children(&mut cursor)
                .any(|child| is_break_call(child, source));
            result
        }
        "exprCall" => {
            let Some(entity) = node.child_by_field_name("entity") else {
                return false;
            };
            entity.kind() == "identifier"
                && node_text(entity, source).eq_ignore_ascii_case("break")
                && call_has_no_arguments(node)
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
            let result = node
                .children(&mut cursor)
                .any(|child| is_continue_call(child, source));
            result
        }
        "exprCall" => {
            let Some(entity) = node.child_by_field_name("entity") else {
                return false;
            };
            entity.kind() == "identifier"
                && node_text(entity, source).eq_ignore_ascii_case("continue")
                && call_has_no_arguments(node)
        }
        "identifier" => {
            let text = node_text(node, source);
            text.eq_ignore_ascii_case("continue")
        }
        _ => false,
    }
}

fn call_has_no_arguments(node: Node) -> bool {
    let Some(args) = node.child_by_field_name("args") else {
        return true;
    };
    let mut cursor = args.walk();
    let result = args.named_children(&mut cursor).next().is_none();
    result
}

/// Identifier for a cleanup scope that may need to be unwound by a control
/// transfer. The builder deliberately keeps this separate from block IDs:
/// a transfer target can be a block inside an enclosing `try..finally` even
/// when the transfer itself originated in a nested scope.
pub(crate) type ScopeId = usize;

/// Identifier for one CFG-construction walk of a label namespace. The root
/// procedure body has one binding, while each cloned finalizer body gets a
/// distinct binding so deferred gotos resolve within their own clone.
pub(crate) type LabelBindingId = usize;

/// The kind of abrupt completion produced by a statement or expression.
///
/// `Goto` carries a label target and the target scope set, allowing cleanup
/// routing to distinguish an in-scope jump from one that leaves a finalizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TransferKind {
    Exit,
    Break,
    Continue,
    Goto,
    Exception,
}

/// A pending control transfer whose edge has not yet been routed through all
/// enclosing cleanup scopes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PendingTransfer {
    pub source: BlockId,
    pub kind: TransferKind,
    pub target: Option<BlockId>,
    pub target_label: Option<String>,
    pub target_label_binding: Option<LabelBindingId>,
    pub target_scopes: Vec<ScopeId>,
    pub from_finally: bool,
    /// Semantic information used for exception-handler dispatch.
    pub exception_fact: Option<ExceptionTypeFact>,
    /// Syntactic constructor spelling retained as source metadata. It may
    /// distinguish cleanup continuations, but never decides handler matches.
    pub exception_type: Option<String>,
}

impl PendingTransfer {
    pub(crate) fn exit(source: BlockId) -> Self {
        Self {
            source,
            kind: TransferKind::Exit,
            target: None,
            target_label: None,
            target_label_binding: None,
            target_scopes: Vec::new(),
            from_finally: false,
            exception_fact: None,
            exception_type: None,
        }
    }

    pub(crate) fn block_target(
        source: BlockId,
        kind: TransferKind,
        target: BlockId,
        target_scopes: Vec<ScopeId>,
    ) -> Self {
        debug_assert!(matches!(kind, TransferKind::Break | TransferKind::Continue));
        Self {
            source,
            kind,
            target: Some(target),
            target_label: None,
            target_label_binding: None,
            target_scopes,
            from_finally: false,
            exception_fact: None,
            exception_type: None,
        }
    }

    pub(crate) fn exception(source: BlockId) -> Self {
        Self::exception_with_fact(source, ExceptionTypeFact::Unknown, None)
    }

    pub(crate) fn exception_with_fact(
        source: BlockId,
        exception_fact: ExceptionTypeFact,
        exception_type: Option<String>,
    ) -> Self {
        Self {
            source,
            kind: TransferKind::Exception,
            target: None,
            target_label: None,
            target_label_binding: None,
            target_scopes: Vec::new(),
            from_finally: false,
            exception_fact: Some(exception_fact),
            exception_type,
        }
    }

    pub(crate) fn goto(
        source: BlockId,
        target_label: String,
        target_label_binding: LabelBindingId,
        target_scopes: Vec<ScopeId>,
    ) -> Self {
        Self {
            source,
            kind: TransferKind::Goto,
            target: None,
            target_label: Some(target_label),
            target_label_binding: Some(target_label_binding),
            target_scopes,
            from_finally: false,
            exception_fact: None,
            exception_type: None,
        }
    }

    /// Whether this transfer's target lies outside `scope_id` and therefore
    /// must pass through that scope's finalizer.
    pub(crate) fn leaves_scope(&self, scope_id: ScopeId) -> bool {
        match self.kind {
            TransferKind::Exception | TransferKind::Exit => true,
            TransferKind::Break | TransferKind::Continue | TransferKind::Goto => {
                !self.target_scopes.contains(&scope_id)
            }
        }
    }

    pub(crate) fn with_source(&self, source: BlockId) -> Self {
        Self {
            source,
            kind: self.kind,
            target: self.target,
            target_label: self.target_label.clone(),
            target_label_binding: self.target_label_binding,
            target_scopes: self.target_scopes.clone(),
            from_finally: true,
            exception_fact: self.exception_fact,
            exception_type: self.exception_type.clone(),
        }
    }
}

/// Preserve the syntactic constructor name from `raise TException.Create(...)`.
///
/// This is metadata only: semantic handler matching is performed by the
/// per-file type index. Calls such as `raise MakeError()` remain unknown
/// because the function's return type is not available here.
pub(crate) fn raised_exception_type(node: Node, source: &[u8]) -> Option<String> {
    raised_exception_parts(node, source).map(|parts| parts.join("."))
}

/// Extract the qualified receiver parts from `raise TException.Create(...)`.
///
/// This is source metadata only. Semantic validation of the receiver and its
/// constructor belongs to `ExceptionTypeIndex`.
pub(crate) fn raised_exception_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    let exception = node.child_by_field_name("exception")?;
    match exception.kind() {
        "exprCall" => constructor_type_parts(exception, source),
        "exprDot" => constructor_type_parts_from_entity(exception, source),
        "exprParens" => {
            let mut cursor = exception.walk();
            let exception_type = exception
                .named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "exprCall" | "exprDot"))
                .and_then(|child| {
                    if child.kind() == "exprCall" {
                        constructor_type_parts(child, source)
                    } else {
                        constructor_type_parts_from_entity(child, source)
                    }
                });
            exception_type
        }
        _ => None,
    }
}

/// Whether evaluating the raised expression can produce a separate exception.
///
/// Constructor calls are executable expressions: even when their syntactic
/// type is retained as metadata, evaluating the arguments or running the
/// constructor may raise a different exception.
pub(crate) fn raise_may_throw_during_evaluation(node: Node, source: &[u8]) -> bool {
    let Some(exception) = node.child_by_field_name("exception") else {
        return false;
    };
    if exception.kind() == "exprCall" {
        return true;
    }
    if exception.kind() == "exprDot" {
        return raised_exception_parts(node, source).is_some();
    }
    if exception.kind() == "exprParens" {
        let mut cursor = exception.walk();
        return exception
            .named_children(&mut cursor)
            .any(|child| raise_may_throw_during_evaluation_expression(child, source));
    }
    false
}

fn raise_may_throw_during_evaluation_expression(node: Node, source: &[u8]) -> bool {
    if node.kind() == "exprCall" {
        return true;
    }
    if node.kind() == "exprDot" {
        return constructor_type_parts_from_entity(node, source).is_some();
    }
    if node.kind() == "exprParens" {
        let mut cursor = node.walk();
        return node
            .named_children(&mut cursor)
            .any(|child| raise_may_throw_during_evaluation_expression(child, source));
    }
    false
}

fn constructor_type_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    let entity = node.child_by_field_name("entity")?;
    constructor_type_parts_from_entity(entity, source)
}

fn constructor_type_parts_from_entity(node: Node, source: &[u8]) -> Option<Vec<String>> {
    let mut parts = qualified_parts(node, source)?;
    let constructor = parts.pop()?;
    constructor
        .eq_ignore_ascii_case("create")
        .then_some(parts)
        .filter(|parts| !parts.is_empty())
}

fn qualified_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" => Some(vec![node_text(node, source)]),
        "exprDot" => node
            .child_by_field_name("lhs")
            .and_then(|lhs| qualified_parts(lhs, source))
            .and_then(|mut parts| {
                let rhs = node.child_by_field_name("rhs")?;
                parts.extend(qualified_parts(rhs, source)?);
                Some(parts)
            }),
        "exprParens" => {
            let mut cursor = node.walk();
            let child = node
                .named_children(&mut cursor)
                .next()
                .and_then(|child| qualified_parts(child, source));
            child
        }
        _ => None,
    }
}

/// Context for loop constructs: tracks where `break` and `continue` jump to.
pub(crate) struct LoopFrame {
    pub continue_target: BlockId,
    pub break_target: BlockId,
    pub continue_scopes: Vec<ScopeId>,
    pub break_scopes: Vec<ScopeId>,
}

/// The result of walking one statement or statement sequence.
///
/// A plain `Option<BlockId>` cannot represent an `if` where one arm raises
/// and the other falls through, nor can it preserve an `Exit`/`Break`/`raise`
/// while a `finally` body is being built. Keeping abrupt paths alongside the
/// normal continuation makes those paths explicit without changing cfg-core's
/// public graph types.
#[derive(Debug, Clone, Default)]
pub(crate) struct Flow {
    pub normal: Option<BlockId>,
    pub transfers: Vec<PendingTransfer>,
}

impl Flow {
    pub(crate) fn normal(block: BlockId) -> Self {
        Self {
            normal: Some(block),
            transfers: Vec::new(),
        }
    }

    pub(crate) fn transfer(transfer: PendingTransfer) -> Self {
        Self {
            normal: None,
            transfers: vec![transfer],
        }
    }
}
