use crate::types::*;
use petgraph::graph::DiGraph;
use std::ops::Range;

/// Trait implemented by CFG builders. Language-specific frontends call these
/// methods to construct a [`Cfg`] without knowing its internal representation.
pub trait CfgBuildSink {
    fn new_block(&mut self, kind: BasicBlockKind) -> BlockId;
    fn add_stmt(&mut self, block: BlockId, stmt: StmtRef);
    fn add_edge(&mut self, from: BlockId, to: BlockId, kind: EdgeKind);
    fn set_entry(&mut self, block: BlockId);
    fn set_exit(&mut self, block: BlockId);
    fn finish(self) -> Cfg;
}

/// General-purpose CFG builder backed by a petgraph [`DiGraph`].
pub struct DefaultCfgBuilder {
    graph: DiGraph<BasicBlock, EdgeKind>,
    entry: Option<BlockId>,
    exit: Option<BlockId>,
    proc_name: String,
    byte_range: Range<usize>,
}

impl DefaultCfgBuilder {
    pub fn new(proc_name: String, byte_range: Range<usize>) -> Self {
        Self {
            graph: DiGraph::new(),
            entry: None,
            exit: None,
            proc_name,
            byte_range,
        }
    }
}

impl CfgBuildSink for DefaultCfgBuilder {
    fn new_block(&mut self, kind: BasicBlockKind) -> BlockId {
        let idx = self.graph.add_node(BasicBlock::new(kind));
        BlockId(idx)
    }

    fn add_stmt(&mut self, block: BlockId, stmt: StmtRef) {
        self.graph[block.0].stmts.push(stmt);
    }

    fn add_edge(&mut self, from: BlockId, to: BlockId, kind: EdgeKind) {
        self.graph.add_edge(from.0, to.0, kind);
    }

    fn set_entry(&mut self, block: BlockId) {
        self.entry = Some(block);
    }

    fn set_exit(&mut self, block: BlockId) {
        self.exit = Some(block);
    }

    fn finish(self) -> Cfg {
        Cfg {
            graph: self.graph,
            entry: self.entry.expect("entry block must be set"),
            exit: self.exit.expect("exit block must be set"),
            proc_name: self.proc_name,
            byte_range: self.byte_range,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(node_kind: &str) -> StmtRef {
        StmtRef {
            byte_range: 0..1,
            node_kind: node_kind.to_string(),
        }
    }

    /// entry -> body -> exit  (3 nodes, 2 edges)
    #[test]
    fn builder_creates_linear_cfg() {
        let mut b = DefaultCfgBuilder::new("LinearProc".to_string(), 0..100);

        let entry = b.new_block(BasicBlockKind::Entry);
        let body = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);

        b.add_stmt(body, stmt("assign_stmt"));
        b.add_stmt(body, stmt("call_stmt"));

        b.add_edge(entry, body, EdgeKind::Normal);
        b.add_edge(body, exit, EdgeKind::Normal);

        b.set_entry(entry);
        b.set_exit(exit);

        let cfg = b.finish();

        assert_eq!(cfg.graph.node_count(), 3, "expected 3 nodes");
        assert_eq!(cfg.graph.edge_count(), 2, "expected 2 edges");
        assert_eq!(cfg.graph[body.0].stmts.len(), 2, "body should have 2 stmts");
        assert_eq!(cfg.entry, entry);
        assert_eq!(cfg.exit, exit);
        assert_eq!(cfg.proc_name, "LinearProc");
    }

    /// entry -> then / else -> join -> exit  (5 nodes, 5 edges)
    ///
    /// Topology:
    ///   entry --ConditionalTrue-->  then_block
    ///   entry --ConditionalFalse--> else_block
    ///   then_block  --Normal--> join
    ///   else_block  --Normal--> join
    ///   join        --Normal--> exit
    #[test]
    fn builder_creates_diamond_cfg() {
        let mut b = DefaultCfgBuilder::new("DiamondProc".to_string(), 0..200);

        let entry = b.new_block(BasicBlockKind::Entry);
        let then_block = b.new_block(BasicBlockKind::Normal);
        let else_block = b.new_block(BasicBlockKind::Normal);
        let join = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);

        b.add_stmt(entry, stmt("if_stmt"));
        b.add_stmt(then_block, stmt("then_assign"));
        b.add_stmt(else_block, stmt("else_assign"));

        b.add_edge(entry, then_block, EdgeKind::ConditionalTrue);
        b.add_edge(entry, else_block, EdgeKind::ConditionalFalse);
        b.add_edge(then_block, join, EdgeKind::Normal);
        b.add_edge(else_block, join, EdgeKind::Normal);
        b.add_edge(join, exit, EdgeKind::Normal);

        b.set_entry(entry);
        b.set_exit(exit);

        let cfg = b.finish();

        assert_eq!(cfg.graph.node_count(), 5, "expected 5 nodes");
        assert_eq!(cfg.graph.edge_count(), 5, "expected 5 edges");
        assert_eq!(
            cfg.graph[entry.0].stmts.len(),
            1,
            "entry should have 1 stmt"
        );
        assert_eq!(cfg.graph[then_block.0].stmts.len(), 1);
        assert_eq!(cfg.graph[else_block.0].stmts.len(), 1);
        assert_eq!(cfg.graph[join.0].stmts.len(), 0, "join has no stmts");
        assert_eq!(cfg.entry, entry);
        assert_eq!(cfg.exit, exit);
        assert_eq!(cfg.proc_name, "DiamondProc");
    }
}
