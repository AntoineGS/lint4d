//! Serializable digest model shared by the text, index and JSON renderers.

use std::ops::Range;

use cfg_pascal::calls::TransactionOp;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDigest {
    pub path: String,
    pub routines: Vec<RoutineDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutineDigest {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub byte_range: Range<usize>,
    pub objects: Vec<ObjectVerdict>,
    /// Calls found in three or more separate runs, removed from those runs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub frequent_calls: Vec<CallCount>,
    pub body: Vec<DigestNode>,
    pub degraded: Vec<Degradation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum DigestNode {
    Control {
        kind: ControlKind,
        line: usize,
        end_line: usize,
        label: String,
        children: Vec<DigestNode>,
    },
    Fact {
        line: usize,
        fact: Fact,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    If,
    Else,
    Case,
    Arm,
    For,
    While,
    Repeat,
    Try,
    Finally,
    Except,
    On,
    ExceptElse,
    With,
    Conditional,
    Branch,
    Asm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    New {
        class: String,
        target: Option<String>,
    },
    Free {
        var: String,
    },
    Tx {
        op: TxOp,
        receiver: String,
        method: String,
    },
    Raise {
        text: String,
    },
    ReRaise,
    Exit {
        runs_finally: Option<usize>,
    },
    Calls {
        calls: Vec<CallCount>,
    },
    Nested {
        name: String,
    },
    Lambda {
        calls: Vec<CallCount>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallCount {
    pub name: String,
    pub count: usize,
}

/// Add one call, keeping first-use order.
pub fn add_call(calls: &mut Vec<CallCount>, name: String) {
    match calls.iter_mut().find(|call| call.name == name) {
        Some(call) => call.count += 1,
        None => calls.push(CallCount { name, count: 1 }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TxOp {
    Start,
    Commit,
    Rollback,
    InTransaction,
}

impl From<TransactionOp> for TxOp {
    fn from(op: TransactionOp) -> Self {
        match op {
            TransactionOp::Start => Self::Start,
            TransactionOp::Commit => Self::Commit,
            TransactionOp::Rollback => Self::Rollback,
            TransactionOp::InTransaction => Self::InTransaction,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectVerdict {
    pub var: String,
    pub class: String,
    pub line: usize,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verdict {
    FreedAllPaths,
    /// At most three escapes, ordered by line; `more` when others exist.
    Escapes {
        via: Vec<Escape>,
        more: bool,
    },
    NotFreedHere {
        why: NotFreedReason,
    },
    Unknown {
        why: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Escape {
    pub line: usize,
    pub how: EscapeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EscapeKind {
    Exception,
    Exit,
    FallsOffEnd,
    ReCreated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotFreedReason {
    Result,
    NonLocal,
    NeverFreed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Degradation {
    ParseErrors { lines: Vec<usize> },
    Conditional { regions: usize },
    HiddenDirective { line: usize },
    AnonymousMethods,
}

/// Routine-index markers, derived from the same model as the full digest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Markers {
    pub tx: bool,
    pub raises: usize,
    pub exits: usize,
    pub escape: bool,
    pub unknown: bool,
}

impl Markers {
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        if self.tx {
            parts.push("tx".to_string());
        }
        if self.raises > 0 {
            parts.push(format!("raise{}", self.raises));
        }
        if self.exits > 0 {
            parts.push(format!("exit{}", self.exits));
        }
        if self.escape {
            parts.push("!escape".to_string());
        }
        if self.unknown {
            parts.push("?".to_string());
        }
        parts.join(" ")
    }
}

impl RoutineDigest {
    pub fn line_count(&self) -> usize {
        self.end_line + 1 - self.start_line
    }

    pub fn markers(&self) -> Markers {
        let mut markers = Markers {
            escape: self
                .objects
                .iter()
                .any(|o| matches!(o.verdict, Verdict::Escapes { .. })),
            unknown: self
                .objects
                .iter()
                .any(|o| matches!(o.verdict, Verdict::Unknown { .. })),
            ..Markers::default()
        };
        count_facts(&self.body, &mut markers);
        markers
    }
}

fn count_facts(nodes: &[DigestNode], markers: &mut Markers) {
    for node in nodes {
        match node {
            DigestNode::Control { children, .. } => count_facts(children, markers),
            DigestNode::Fact { fact, .. } => match fact {
                Fact::Tx { .. } => markers.tx = true,
                Fact::Raise { .. } | Fact::ReRaise => markers.raises += 1,
                Fact::Exit { .. } => markers.exits += 1,
                _ => {}
            },
        }
    }
}
