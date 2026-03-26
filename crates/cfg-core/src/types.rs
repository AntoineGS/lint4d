use petgraph::graph::{DiGraph, NodeIndex};
use std::ops::Range;

/// Opaque handle to a basic block within a CFG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub(crate) NodeIndex);

impl BlockId {
    pub fn index(self) -> NodeIndex {
        self.0
    }
}

impl From<NodeIndex> for BlockId {
    fn from(idx: NodeIndex) -> Self {
        Self(idx)
    }
}

/// A reference to a source-level statement within a basic block.
#[derive(Debug, Clone)]
pub struct StmtRef {
    pub byte_range: Range<usize>,
    pub node_kind: String,
}

/// Classification of a basic block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasicBlockKind {
    Entry,
    Exit,
    Normal,
    FinallyHandler,
    ExceptHandler,
    BareExceptHandler,
}

/// A single basic block: a straight-line sequence of statements
/// with no branches except at the end.
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub stmts: Vec<StmtRef>,
    pub kind: BasicBlockKind,
}

impl BasicBlock {
    pub fn new(kind: BasicBlockKind) -> Self {
        Self {
            stmts: Vec::new(),
            kind,
        }
    }
}

/// Classification of a CFG edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeKind {
    Normal,
    ConditionalTrue,
    ConditionalFalse,
    LoopBack,
    LoopExit,
    ExceptionThrow,
    FinallyEntry,
    FinallyExit,
    CaseArm,
    Goto,
}

/// A per-procedure control flow graph.
#[derive(Debug)]
pub struct Cfg {
    pub graph: DiGraph<BasicBlock, EdgeKind>,
    pub entry: BlockId,
    pub exit: BlockId,
    pub proc_name: String,
    pub byte_range: Range<usize>,
}
