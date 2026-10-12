//! Builds a routine's control tree from its syntax tree; facts sit inline.

use std::collections::HashSet;

use cfg_pascal::calls::{CallKind, classify_call, lookup_transaction_method};
use cfg_pascal::cfg_core::{BasicBlockKind, Cfg, EdgeKind};
use pascal_core::decode_bytes;
use pascal_core::parser::node_text;
use tree_sitter::Node;

use super::model::{CallCount, ControlKind, DigestNode, Fact, add_call};
use super::{
    LineIndex, field_children, is_exit_statement, is_keyword, named_children, normalize,
    target_name, truncate,
};

const LABEL_WIDTH: usize = 70;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub conditional_regions: usize,
    pub lambdas: usize,
}

pub fn build(
    scope: Node<'_>,
    cfg: &Cfg,
    text: &[u8],
    lines: &LineIndex,
) -> (Vec<DigestNode>, Stats) {
    let mut builder = Builder {
        cfg,
        text,
        lines,
        stats: Stats::default(),
    };
    let mut seq = Seq::default();
    let body = if scope.kind() == "defProc" {
        for local in field_children(scope, "local") {
            let name = local
                .child_by_field_name("header")
                .and_then(|header| header.child_by_field_name("name"));
            if let (true, Some(name)) = (local.kind() == "defProc", name) {
                seq.push(DigestNode::Fact {
                    line: lines.line(local.start_byte()),
                    fact: Fact::Nested {
                        name: format!("{}.{}", cfg.proc_name, node_text(name, text)),
                    },
                });
            }
        }
        scope.child_by_field_name("body")
    } else {
        Some(scope)
    };
    if let Some(body) = body {
        builder.item(body, &mut seq);
    }
    (seq.finish(), builder.stats)
}

/// Statements in source order; consecutive calls merge into one `Calls` fact.
#[derive(Default)]
struct Seq {
    out: Vec<DigestNode>,
    run: Option<(usize, Vec<CallCount>)>,
}

impl Seq {
    fn call(&mut self, line: usize, name: String) {
        let (_, calls) = self.run.get_or_insert_with(|| (line, Vec::new()));
        add_call(calls, name);
    }

    fn push(&mut self, node: DigestNode) {
        self.flush();
        self.out.push(node);
    }

    fn flush(&mut self) {
        if let Some((line, calls)) = self.run.take() {
            self.out.push(DigestNode::Fact {
                line,
                fact: Fact::Calls { calls },
            });
        }
    }

    fn finish(mut self) -> Vec<DigestNode> {
        self.flush();
        self.out
    }
}

enum Item {
    Fact(Fact),
    Call(String),
}

struct Builder<'a> {
    cfg: &'a Cfg,
    text: &'a [u8],
    lines: &'a LineIndex,
    stats: Stats,
}

impl Builder<'_> {
    fn item(&mut self, node: Node<'_>, seq: &mut Seq) {
        if node.is_extra() || is_keyword(node) {
            return;
        }
        let line = self.lines.line(node.start_byte());
        match node.kind() {
            "block" | "statements" | "labeledStatement" | "labeledStatementTr"
            | "initialization" | "finalization" => {
                for child in named_children(node) {
                    self.item(child, seq);
                }
            }
            "label" | "goto" => {}
            "if" | "ifElse" => self.if_node(node, seq),
            "for" | "foreach" | "while" => {
                let kind = if node.kind() == "while" {
                    ControlKind::While
                } else {
                    ControlKind::For
                };
                let label = self.header(node);
                let children = self.child_seq(node.child_by_field_name("body"));
                seq.push(self.control(kind, node.start_byte(), node.end_byte(), label, children));
            }
            "repeat" => {
                let label = node
                    .child_by_field_name("condition")
                    .map(|condition| self.condition_label(condition))
                    .unwrap_or_default();
                let children = self.nodes_seq(&field_children(node, "body"));
                seq.push(self.control(
                    ControlKind::Repeat,
                    node.start_byte(),
                    node.end_byte(),
                    label,
                    children,
                ));
            }
            "case" => self.case_node(node, seq),
            "try" => self.try_node(node, seq),
            "with" => {
                let entities: Vec<String> = field_children(node, "entity")
                    .into_iter()
                    .map(|entity| self.short(entity, LABEL_WIDTH))
                    .collect();
                let children = self.child_seq(node.child_by_field_name("body"));
                seq.push(self.control(
                    ControlKind::With,
                    node.start_byte(),
                    node.end_byte(),
                    entities.join(", "),
                    children,
                ));
            }
            "ppBlock" => self.conditional(node, seq),
            "asm" => seq.push(self.control(
                ControlKind::Asm,
                node.start_byte(),
                node.end_byte(),
                String::new(),
                Vec::new(),
            )),
            "raise" => {
                let fact = match node.child_by_field_name("exception") {
                    Some(exception) => Fact::Raise {
                        text: self.short(exception, 60),
                    },
                    None => Fact::ReRaise,
                };
                seq.push(DigestNode::Fact { line, fact });
            }
            "statement" if is_exit_statement(node, self.text) => {
                let fact = Fact::Exit {
                    runs_finally: self.runs_finally(node),
                };
                seq.push(DigestNode::Fact { line, fact });
            }
            _ => self.simple(node, seq),
        }
    }

    fn control(
        &self,
        kind: ControlKind,
        start: usize,
        end: usize,
        label: String,
        children: Vec<DigestNode>,
    ) -> DigestNode {
        DigestNode::Control {
            kind,
            line: self.lines.line(start),
            end_line: self.lines.line(end.saturating_sub(1).max(start)),
            label,
            children,
        }
    }

    fn child_seq(&mut self, node: Option<Node<'_>>) -> Vec<DigestNode> {
        let mut seq = Seq::default();
        if let Some(node) = node {
            self.item(node, &mut seq);
        }
        seq.finish()
    }

    fn nodes_seq(&mut self, nodes: &[Node<'_>]) -> Vec<DigestNode> {
        let mut seq = Seq::default();
        for node in nodes {
            self.item(*node, &mut seq);
        }
        seq.finish()
    }

    fn short(&self, node: Node<'_>, max: usize) -> String {
        truncate(&normalize(&node_text(node, self.text)), max)
    }

    /// Loop header text up to the body, without the trailing `do`.
    fn header(&self, node: Node<'_>) -> String {
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |body| body.start_byte());
        let text = normalize(&decode_bytes(&self.text[node.start_byte()..end]));
        let text = if text.to_ascii_lowercase().ends_with(" do") {
            &text[..text.len() - 3]
        } else {
            text.as_str()
        };
        truncate(text, LABEL_WIDTH)
    }

    /// Condition text; calls cut off by truncation are listed after it.
    fn condition_label(&mut self, condition: Node<'_>) -> String {
        let full = normalize(&node_text(condition, self.text));
        let label = truncate(&full, LABEL_WIDTH);
        if full.chars().count() <= LABEL_WIDTH {
            return label;
        }
        let hidden: Vec<String> = self
            .calls_in(condition)
            .into_iter()
            .map(|call| call.name)
            .filter(|name| !label.contains(name.as_str()))
            .collect();
        if hidden.is_empty() {
            label
        } else {
            format!("{label} [calls {}]", hidden.join(", "))
        }
    }

    fn if_node(&mut self, node: Node<'_>, seq: &mut Seq) {
        let label = node
            .child_by_field_name("condition")
            .map(|condition| self.condition_label(condition))
            .unwrap_or_default();
        let then = node.child_by_field_name("then");
        let then_end = then.map_or(node.end_byte(), |then| then.end_byte());
        let children = self.child_seq(then);
        seq.push(self.control(
            ControlKind::If,
            node.start_byte(),
            then_end,
            label,
            children,
        ));
        if let Some(other) = node.child_by_field_name("else") {
            let children = self.child_seq(Some(other));
            seq.push(self.control(
                ControlKind::Else,
                other.start_byte(),
                other.end_byte(),
                String::new(),
                children,
            ));
        }
    }

    fn case_node(&mut self, node: Node<'_>, seq: &mut Seq) {
        let mut selector = String::new();
        let mut arms = Vec::new();
        let mut else_start = None;
        let mut else_nodes = Vec::new();
        for child in named_children(node) {
            match child.kind() {
                "caseCase" => {
                    let labels: Vec<String> = field_children(child, "label")
                        .into_iter()
                        .map(|label| self.case_label(label))
                        .collect();
                    let children = self.child_seq(child.child_by_field_name("body"));
                    arms.push(self.control(
                        ControlKind::Arm,
                        child.start_byte(),
                        child.end_byte(),
                        labels.join(", "),
                        children,
                    ));
                }
                "kElse" | "kOtherwise" => else_start = Some(child.start_byte()),
                _ if is_keyword(child) => {}
                _ if else_start.is_some() => else_nodes.push(child),
                _ if selector.is_empty() && arms.is_empty() => selector = self.short(child, 50),
                _ => {}
            }
        }
        if let Some(start) = else_start {
            let end = else_nodes.last().map_or(start + 1, |last| last.end_byte());
            let children = self.nodes_seq(&else_nodes);
            arms.push(self.control(ControlKind::Arm, start, end, "else".to_string(), children));
        }
        seq.push(self.control(
            ControlKind::Case,
            node.start_byte(),
            node.end_byte(),
            selector,
            arms,
        ));
    }

    /// A `caseLabel` node includes the trailing `:`; the renderer adds its own.
    fn case_label(&self, label: Node<'_>) -> String {
        let text = normalize(&node_text(label, self.text));
        truncate(text.trim_end_matches(':').trim_end(), LABEL_WIDTH)
    }

    fn try_node(&mut self, node: Node<'_>, seq: &mut Seq) {
        let body = field_children(node, "try");
        let body_end = body.last().map_or(node.end_byte(), |last| last.end_byte());
        let children = self.nodes_seq(&body);
        seq.push(self.control(
            ControlKind::Try,
            node.start_byte(),
            body_end,
            String::new(),
            children,
        ));

        let finally = field_children(node, "finally");
        if let Some(keyword) = finally.iter().find(|n| n.kind() == "kFinally") {
            let children = self.nodes_seq(&finally);
            seq.push(self.control(
                ControlKind::Finally,
                keyword.start_byte(),
                node.end_byte(),
                String::new(),
                children,
            ));
        }

        let except = field_children(node, "except");
        if let Some(keyword) = except.iter().find(|n| n.kind() == "kExcept") {
            let mut inner = Seq::default();
            for handler in &except {
                match handler.kind() {
                    "exceptionHandler" => {
                        let label = self.handler_label(*handler);
                        let children = self.child_seq(handler.child_by_field_name("body"));
                        inner.push(self.control(
                            ControlKind::On,
                            handler.start_byte(),
                            handler.end_byte(),
                            label,
                            children,
                        ));
                    }
                    "exceptionElse" => {
                        let children = self.nodes_seq(&named_children(*handler));
                        inner.push(self.control(
                            ControlKind::ExceptElse,
                            handler.start_byte(),
                            handler.end_byte(),
                            String::new(),
                            children,
                        ));
                    }
                    _ => self.item(*handler, &mut inner),
                }
            }
            seq.push(self.control(
                ControlKind::Except,
                keyword.start_byte(),
                node.end_byte(),
                String::new(),
                inner.finish(),
            ));
        }
    }

    fn handler_label(&self, handler: Node<'_>) -> String {
        let exception = handler
            .child_by_field_name("exception")
            .map(|exception| self.short(exception, LABEL_WIDTH));
        match (handler.child_by_field_name("variable"), exception) {
            (Some(variable), Some(exception)) => {
                format!("{}: {exception}", node_text(variable, self.text))
            }
            (None, Some(exception)) => exception,
            _ => String::new(),
        }
    }

    /// `{$IF…}` alternatives; every branch is shown.
    fn conditional<'t>(&mut self, node: Node<'t>, seq: &mut Seq) {
        self.stats.conditional_regions += 1;
        let mut branches = Vec::new();
        let mut current: Option<(Node<'t>, Vec<Node<'t>>)> = None;
        for child in named_children(node) {
            match child.kind() {
                "ppIf" | "ppElse" => {
                    if let Some((directive, nodes)) = current.take() {
                        branches.push(self.branch(directive, &nodes));
                    }
                    current = Some((child, Vec::new()));
                }
                "ppEndIf" => {}
                _ => {
                    if let Some((_, nodes)) = current.as_mut() {
                        nodes.push(child);
                    }
                }
            }
        }
        if let Some((directive, nodes)) = current {
            branches.push(self.branch(directive, &nodes));
        }
        seq.push(self.control(
            ControlKind::Conditional,
            node.start_byte(),
            node.end_byte(),
            String::new(),
            branches,
        ));
    }

    fn branch(&mut self, directive: Node<'_>, nodes: &[Node<'_>]) -> DigestNode {
        let end = nodes
            .last()
            .map_or(directive.end_byte(), |last| last.end_byte());
        let label = self.short(directive, LABEL_WIDTH);
        let children = self.nodes_seq(nodes);
        self.control(
            ControlKind::Branch,
            directive.start_byte(),
            end,
            label,
            children,
        )
    }

    /// Line of the first `finally` statement an `Exit` runs on its way out.
    fn runs_finally(&self, node: Node<'_>) -> Option<usize> {
        let none = HashSet::new();
        self.cfg
            .blocks_for_stmt(node.byte_range())
            .into_iter()
            .find_map(|block| {
                let path = self.cfg.path_avoiding(
                    block,
                    self.cfg.exit,
                    &none,
                    &[EdgeKind::ExceptionThrow],
                )?;
                path.iter().find_map(|edge| {
                    let target = &self.cfg.graph[edge.to.index()];
                    if target.kind != BasicBlockKind::FinallyHandler {
                        return None;
                    }
                    target
                        .stmts
                        .first()
                        .map(|stmt| self.lines.line(stmt.byte_range.start))
                })
            })
    }

    fn simple(&mut self, node: Node<'_>, seq: &mut Seq) {
        self.scan(node, &mut |line, item| match item {
            Item::Call(name) => seq.call(line, name),
            Item::Fact(fact) => seq.push(DigestNode::Fact { line, fact }),
        });
    }

    fn calls_in(&mut self, node: Node<'_>) -> Vec<CallCount> {
        let mut calls = Vec::new();
        self.scan(node, &mut |_, item| {
            if let Item::Call(name) = item {
                add_call(&mut calls, name);
            }
        });
        calls
    }

    /// Pre-order walk of one simple statement or expression.
    fn scan(&mut self, root: Node<'_>, sink: &mut dyn FnMut(usize, Item)) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let line = self.lines.line(node.start_byte());
            match node.kind() {
                "lambda" => {
                    self.stats.lambdas += 1;
                    let calls = node
                        .child_by_field_name("body")
                        .map(|body| self.calls_in(body))
                        .unwrap_or_default();
                    sink(line, Item::Fact(Fact::Lambda { calls }));
                    continue;
                }
                "defProc" => continue,
                "inherited" => {
                    let member = named_children(node)
                        .into_iter()
                        .find(|child| child.kind() == "identifier");
                    let name = member.map_or_else(
                        || "inherited".to_string(),
                        |member| format!("inherited {}", node_text(member, self.text)),
                    );
                    sink(line, Item::Call(name));
                    continue;
                }
                _ => {}
            }
            if let Some(item) = self.classify(node) {
                sink(line, item);
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }

    fn classify(&self, node: Node<'_>) -> Option<Item> {
        let kind = match node.kind() {
            "exprCall" | "statement" => classify_call(node, self.text)?,
            "exprDot" => {
                let parent = node.parent()?;
                if parent.kind() == "exprCall" && parent.child_by_field_name("entity") == Some(node)
                {
                    return None;
                }
                match classify_call(node, self.text)? {
                    // A dotted name without parentheses is a call only as a
                    // statement; elsewhere it may be a property read (l4d-ftm).
                    CallKind::MethodCall { .. } if parent.kind() != "statement" => return None,
                    other => other,
                }
            }
            _ => return None,
        };
        Some(match kind {
            CallKind::Constructor { class_name } => Item::Fact(Fact::New {
                class: class_name,
                target: assignment_target(node, self.text),
            }),
            CallKind::Free { var_name }
            | CallKind::Destroy { var_name }
            | CallKind::FreeAndNil { var_name } => Item::Fact(Fact::Free { var: var_name }),
            CallKind::MethodCall { receiver, method } => {
                match lookup_transaction_method(&method).first() {
                    Some(entry) => Item::Fact(Fact::Tx {
                        op: entry.op.into(),
                        receiver,
                        method,
                    }),
                    None => Item::Call(format!("{receiver}.{method}")),
                }
            }
            CallKind::FunctionCall { name } => {
                if is_cast(&name)
                    || ["exit", "break", "continue"]
                        .iter()
                        .any(|w| name.eq_ignore_ascii_case(w))
                {
                    return None;
                }
                Item::Call(name)
            }
        })
    }
}

/// `TFoo(x)` is a cast by Delphi naming convention, not a call.
fn is_cast(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 1 && bytes[0] == b'T' && bytes[1].is_ascii_uppercase()
}

/// The left-hand side when `node` is the right-hand side of an assignment.
fn assignment_target(node: Node<'_>, text: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() != "assignment" || parent.child_by_field_name("rhs") != Some(node) {
        return None;
    }
    parent
        .child_by_field_name("lhs")
        .map(|lhs| target_name(lhs, text))
}
