use crate::types::*;
use petgraph::algo::dominators::simple_fast;
use petgraph::visit::{Dfs, EdgeRef};
use petgraph::Direction;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

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

        forward
            .intersection(&backward)
            .map(|&n| BlockId(n))
            .collect()
    }
}

/// One edge of a path returned by [`Cfg::path_avoiding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEdge {
    pub from: BlockId,
    pub to: BlockId,
    pub kind: EdgeKind,
}

impl Cfg {
    /// Every block holding the innermost statement reference that contains
    /// `range`, sorted by block index.
    ///
    /// The Pascal builder copies a `finally` body once per continuation
    /// (normal completion, exception, each `Exit`/`Break`/`goto`), so one
    /// source statement can live in several blocks; all copies are returned.
    pub fn blocks_for_stmt(&self, range: Range<usize>) -> Vec<BlockId> {
        let contains =
            |s: &StmtRef| s.byte_range.start <= range.start && range.end <= s.byte_range.end;
        let Some(innermost) = self
            .graph
            .node_weights()
            .flat_map(|block| block.stmts.iter())
            .filter(|s| contains(s))
            .map(|s| s.byte_range.len())
            .min()
        else {
            return Vec::new();
        };
        let mut blocks: Vec<BlockId> = self
            .graph
            .node_indices()
            .filter(|&index| {
                self.graph[index]
                    .stmts
                    .iter()
                    .any(|s| contains(s) && s.byte_range.len() == innermost)
            })
            .map(BlockId)
            .collect();
        blocks.sort();
        blocks
    }

    /// Shortest path from `from` to `to` that never enters a block in
    /// `avoid`. The path starts with an outgoing edge of `from`, so
    /// `from == to` asks for a cycle. Edges leaving `from` whose kind is in
    /// `skip_from` are never taken (for example, ignore exceptions raised by
    /// the statement that is the subject of the query).
    pub fn path_avoiding(
        &self,
        from: BlockId,
        to: BlockId,
        avoid: &HashSet<BlockId>,
        skip_from: &[EdgeKind],
    ) -> Option<Vec<PathEdge>> {
        if avoid.contains(&to) {
            return None;
        }
        let mut parent: HashMap<BlockId, PathEdge> = HashMap::new();
        let mut queue = VecDeque::from([from]);
        let mut expanded = HashSet::new();
        while let Some(block) = queue.pop_front() {
            if !expanded.insert(block) {
                continue;
            }
            for edge in self.graph.edges(block.0) {
                if block == from && skip_from.contains(edge.weight()) {
                    continue;
                }
                let target = BlockId(edge.target());
                if avoid.contains(&target) || parent.contains_key(&target) {
                    continue;
                }
                parent.insert(
                    target,
                    PathEdge {
                        from: block,
                        to: target,
                        kind: edge.weight().clone(),
                    },
                );
                if target == to {
                    return Some(reconstruct(&parent, from, to));
                }
                queue.push_back(target);
            }
        }
        None
    }

    /// True if every path from `from` to exit enters a block of `through`.
    pub fn must_pass_through_any(&self, from: BlockId, through: &HashSet<BlockId>) -> bool {
        if from == self.exit {
            return through.contains(&from);
        }
        through.contains(&from) || self.path_avoiding(from, self.exit, through, &[]).is_none()
    }
}

fn reconstruct(parent: &HashMap<BlockId, PathEdge>, from: BlockId, to: BlockId) -> Vec<PathEdge> {
    let mut path = Vec::new();
    let mut current = to;
    loop {
        let edge = parent[&current].clone();
        current = edge.from;
        path.push(edge);
        if current == from {
            break;
        }
    }
    path.reverse();
    path
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
    fn build_diamond_cfg() -> (
        crate::types::Cfg,
        BlockId,
        BlockId,
        BlockId,
        BlockId,
        BlockId,
    ) {
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

    use std::collections::HashSet;

    fn set(blocks: &[BlockId]) -> HashSet<BlockId> {
        blocks.iter().copied().collect()
    }

    #[test]
    fn path_avoiding_finds_the_other_diamond_arm() {
        let (cfg, entry, then_blk, else_blk, _join, exit) = build_diamond_cfg();
        let path = cfg
            .path_avoiding(entry, exit, &set(&[then_blk]), &[])
            .expect("the else arm bypasses then");
        assert_eq!(path.first().map(|e| e.to), Some(else_blk));
        assert_eq!(path.last().map(|e| e.to), Some(exit));
        assert!(cfg
            .path_avoiding(entry, exit, &set(&[then_blk, else_blk]), &[])
            .is_none());
    }

    #[test]
    fn path_avoiding_skips_edge_kinds_leaving_from() {
        let (cfg, entry, try_body, finally, exit) = build_try_finally_cfg();
        assert!(cfg
            .path_avoiding(entry, exit, &set(&[finally]), &[EdgeKind::ExceptionThrow])
            .is_none());
        let path = cfg
            .path_avoiding(entry, exit, &set(&[try_body]), &[])
            .expect("the exception edge reaches finally then exit");
        assert_eq!(path[0].kind, EdgeKind::ExceptionThrow);
    }

    #[test]
    fn path_avoiding_finds_cycles_back_to_from() {
        let mut b = DefaultCfgBuilder::new("Loop".to_string(), 0..10);
        let entry = b.new_block(BasicBlockKind::Entry);
        let head = b.new_block(BasicBlockKind::Normal);
        let body = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_edge(entry, head, EdgeKind::Normal);
        b.add_edge(head, body, EdgeKind::ConditionalTrue);
        b.add_edge(body, head, EdgeKind::LoopBack);
        b.add_edge(head, exit, EdgeKind::LoopExit);
        b.set_entry(entry);
        b.set_exit(exit);
        let cfg = b.finish();
        let path = cfg
            .path_avoiding(body, body, &HashSet::new(), &[])
            .expect("body reaches itself through the loop header");
        assert_eq!(path.len(), 2);
        assert!(cfg.path_avoiding(body, body, &set(&[head]), &[]).is_none());
    }

    #[test]
    fn must_pass_through_any_accepts_a_set() {
        let (cfg, entry, then_blk, else_blk, _join, _exit) = build_diamond_cfg();
        assert!(cfg.must_pass_through_any(entry, &set(&[then_blk, else_blk])));
        assert!(!cfg.must_pass_through_any(entry, &set(&[then_blk])));
    }

    #[test]
    fn must_pass_through_any_at_exit_requires_membership() {
        let (cfg, _entry, a, _bb, exit) = build_linear_cfg();
        assert!(!cfg.must_pass_through_any(exit, &set(&[a])));
        assert!(cfg.must_pass_through_any(exit, &set(&[exit])));
    }

    #[test]
    fn blocks_for_stmt_returns_every_copy_of_the_innermost_statement() {
        let stmt = |range: std::ops::Range<usize>| StmtRef {
            byte_range: range,
            node_kind: "statement".to_string(),
        };
        let mut b = DefaultCfgBuilder::new("Copies".to_string(), 0..100);
        let entry = b.new_block(BasicBlockKind::Entry);
        let outer = b.new_block(BasicBlockKind::Normal);
        let copy_a = b.new_block(BasicBlockKind::FinallyHandler);
        let copy_b = b.new_block(BasicBlockKind::FinallyHandler);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_stmt(outer, stmt(0..50));
        b.add_stmt(copy_a, stmt(10..20));
        b.add_stmt(copy_b, stmt(10..20));
        b.add_edge(entry, outer, EdgeKind::Normal);
        b.add_edge(outer, exit, EdgeKind::Normal);
        b.set_entry(entry);
        b.set_exit(exit);
        let cfg = b.finish();
        assert_eq!(cfg.blocks_for_stmt(12..15), vec![copy_a, copy_b]);
        assert_eq!(cfg.blocks_for_stmt(30..35), vec![outer]);
        assert!(cfg.blocks_for_stmt(60..70).is_empty());
    }
}
