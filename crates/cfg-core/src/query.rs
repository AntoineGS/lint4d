use crate::types::*;
use petgraph::algo::dominators::simple_fast;
use petgraph::visit::Dfs;
use petgraph::Direction;
use std::collections::HashSet;

impl Cfg {
    /// Returns true if there is any path from `from` to `to`.
    pub fn is_reachable(&self, from: BlockId, to: BlockId) -> bool {
        let mut dfs = Dfs::new(&self.graph, from.0);
        while let Some(node) = dfs.next(&self.graph) {
            if node == to.0 {
                return true;
            }
        }
        false
    }

    /// Returns the set of blocks that dominate `block` (strict dominators only,
    /// not including `block` itself).
    pub fn dominators(&self, block: BlockId) -> HashSet<BlockId> {
        let doms = simple_fast(&self.graph, self.entry.0);
        let mut result = HashSet::new();
        let mut current = block.0;
        loop {
            match doms.immediate_dominator(current) {
                None => break,
                Some(dom) => {
                    result.insert(BlockId(dom));
                    current = dom;
                }
            }
        }
        result
    }

    /// Returns true if every path from `from` to exit passes through `through`.
    /// Uses the approach: remove `through` and check if exit is still reachable
    /// from `from`.
    pub fn must_pass_through(&self, from: BlockId, through: BlockId) -> bool {
        if from == through {
            return true;
        }
        let mut visited = HashSet::new();
        let mut stack = vec![from.0];
        while let Some(node) = stack.pop() {
            if node == through.0 {
                continue; // skip through node
            }
            if node == self.exit.0 {
                return false; // found a bypass to exit
            }
            if !visited.insert(node) {
                continue;
            }
            for neighbor in self.graph.neighbors(node) {
                stack.push(neighbor);
            }
        }
        true
    }

    /// Returns all blocks on any path from `from` to `to`.
    pub fn blocks_between(&self, from: BlockId, to: BlockId) -> HashSet<BlockId> {
        // Forward reachable from `from`
        let mut forward = HashSet::new();
        let mut dfs = Dfs::new(&self.graph, from.0);
        while let Some(node) = dfs.next(&self.graph) {
            forward.insert(node);
        }

        // Backward reachable from `to` (walk predecessors)
        let mut backward = HashSet::new();
        let mut stack = vec![to.0];
        while let Some(node) = stack.pop() {
            if !backward.insert(node) {
                continue;
            }
            for pred in self.graph.neighbors_directed(node, Direction::Incoming) {
                stack.push(pred);
            }
        }

        forward.intersection(&backward).map(|&n| BlockId(n)).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::builder::{CfgBuildSink, DefaultCfgBuilder};
    use crate::types::*;

    // -----------------------------------------------------------------------
    // CFG helpers
    // -----------------------------------------------------------------------

    /// entry -> a -> b -> exit  (4 nodes, 3 edges)
    fn build_linear_cfg() -> (crate::types::Cfg, BlockId, BlockId, BlockId, BlockId) {
        let mut b = DefaultCfgBuilder::new("Linear".to_string(), 0..100);
        let entry = b.new_block(BasicBlockKind::Entry);
        let a = b.new_block(BasicBlockKind::Normal);
        let bb = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_edge(entry, a, EdgeKind::Normal);
        b.add_edge(a, bb, EdgeKind::Normal);
        b.add_edge(bb, exit, EdgeKind::Normal);
        b.set_entry(entry);
        b.set_exit(exit);
        (b.finish(), entry, a, bb, exit)
    }

    /// entry -> then / else -> join -> exit  (5 nodes, 5 edges)
    #[allow(clippy::type_complexity)]
    fn build_diamond_cfg()
    -> (crate::types::Cfg, BlockId, BlockId, BlockId, BlockId, BlockId)
    {
        let mut b = DefaultCfgBuilder::new("Diamond".to_string(), 0..200);
        let entry = b.new_block(BasicBlockKind::Entry);
        let then_blk = b.new_block(BasicBlockKind::Normal);
        let else_blk = b.new_block(BasicBlockKind::Normal);
        let join = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_edge(entry, then_blk, EdgeKind::ConditionalTrue);
        b.add_edge(entry, else_blk, EdgeKind::ConditionalFalse);
        b.add_edge(then_blk, join, EdgeKind::Normal);
        b.add_edge(else_blk, join, EdgeKind::Normal);
        b.add_edge(join, exit, EdgeKind::Normal);
        b.set_entry(entry);
        b.set_exit(exit);
        (b.finish(), entry, then_blk, else_blk, join, exit)
    }

    /// entry -> try_body -> finally -> exit
    /// Extra edges: ExceptionThrow entry->finally, FinallyEntry try_body->finally
    fn build_try_finally_cfg() -> (crate::types::Cfg, BlockId, BlockId, BlockId, BlockId) {
        let mut b = DefaultCfgBuilder::new("TryFinally".to_string(), 0..300);
        let entry = b.new_block(BasicBlockKind::Entry);
        let try_body = b.new_block(BasicBlockKind::Normal);
        let finally = b.new_block(BasicBlockKind::FinallyHandler);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_edge(entry, try_body, EdgeKind::Normal);
        b.add_edge(try_body, finally, EdgeKind::FinallyEntry);
        b.add_edge(entry, finally, EdgeKind::ExceptionThrow);
        b.add_edge(finally, exit, EdgeKind::Normal);
        b.set_entry(entry);
        b.set_exit(exit);
        (b.finish(), entry, try_body, finally, exit)
    }

    // -----------------------------------------------------------------------
    // is_reachable tests
    // -----------------------------------------------------------------------

    #[test]
    fn reachable_linear() {
        let (cfg, entry, _a, _b, exit) = build_linear_cfg();
        assert!(cfg.is_reachable(entry, exit));
    }

    #[test]
    fn not_reachable_reverse() {
        let (cfg, entry, _a, _b, exit) = build_linear_cfg();
        assert!(!cfg.is_reachable(exit, entry));
    }

    #[test]
    fn reachable_diamond_both_arms() {
        let (cfg, entry, then_blk, else_blk, _join, exit) = build_diamond_cfg();
        assert!(cfg.is_reachable(entry, exit));
        assert!(cfg.is_reachable(then_blk, exit));
        assert!(cfg.is_reachable(else_blk, exit));
    }

    #[test]
    fn diamond_arms_not_reachable_from_each_other() {
        let (cfg, _entry, then_blk, else_blk, _join, _exit) = build_diamond_cfg();
        assert!(!cfg.is_reachable(then_blk, else_blk));
        assert!(!cfg.is_reachable(else_blk, then_blk));
    }

    // -----------------------------------------------------------------------
    // must_pass_through tests
    // -----------------------------------------------------------------------

    #[test]
    fn must_pass_through_finally() {
        let (cfg, _entry, try_body, finally, _exit) = build_try_finally_cfg();
        assert!(cfg.must_pass_through(try_body, finally));
    }

    // -----------------------------------------------------------------------
    // dominators tests
    // -----------------------------------------------------------------------

    #[test]
    fn dominators_diamond_join() {
        let (cfg, entry, _then_blk, _else_blk, join, _exit) = build_diamond_cfg();
        let doms = cfg.dominators(join);
        // entry must be in the dominator set of join
        assert!(doms.contains(&entry));
    }

    // -----------------------------------------------------------------------
    // blocks_between tests
    // -----------------------------------------------------------------------

    #[test]
    fn blocks_between_linear() {
        let (cfg, entry, a, b, exit) = build_linear_cfg();
        let between = cfg.blocks_between(entry, exit);
        assert!(between.contains(&entry));
        assert!(between.contains(&a));
        assert!(between.contains(&b));
        assert!(between.contains(&exit));
        assert_eq!(between.len(), 4);
    }
}
