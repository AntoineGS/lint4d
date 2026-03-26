use crate::types::*;
use std::fmt::Write;

impl Cfg {
    pub fn to_dot(&self) -> String {
        let mut out = String::new();
        writeln!(out, "digraph \"{}\" {{", self.proc_name).unwrap();
        writeln!(out, "  rankdir=TB;").unwrap();

        for idx in self.graph.node_indices() {
            let block = &self.graph[idx];
            let label = match block.kind {
                BasicBlockKind::Entry => "Entry".to_string(),
                BasicBlockKind::Exit => "Exit".to_string(),
                BasicBlockKind::Normal => {
                    if block.stmts.is_empty() {
                        format!("B{}", idx.index())
                    } else {
                        let kinds: Vec<_> =
                            block.stmts.iter().map(|s| s.node_kind.as_str()).collect();
                        format!("B{}\\n{}", idx.index(), kinds.join("\\n"))
                    }
                }
                BasicBlockKind::FinallyHandler => format!("Finally(B{})", idx.index()),
                BasicBlockKind::ExceptHandler => format!("Except(B{})", idx.index()),
                BasicBlockKind::BareExceptHandler => format!("BareExcept(B{})", idx.index()),
            };
            let shape = match block.kind {
                BasicBlockKind::Entry | BasicBlockKind::Exit => "ellipse",
                BasicBlockKind::FinallyHandler
                | BasicBlockKind::ExceptHandler
                | BasicBlockKind::BareExceptHandler => "doubleoctagon",
                BasicBlockKind::Normal => "box",
            };
            writeln!(
                out,
                "  n{} [label=\"{}\", shape={}];",
                idx.index(),
                label,
                shape
            )
            .unwrap();
        }

        for edge in self.graph.edge_indices() {
            let (src, tgt) = self.graph.edge_endpoints(edge).unwrap();
            let kind = &self.graph[edge];
            let style = match kind {
                EdgeKind::ExceptionThrow => "style=dashed, color=red, ",
                EdgeKind::ConditionalTrue => "color=green, ",
                EdgeKind::ConditionalFalse => "color=orange, ",
                EdgeKind::LoopBack => "style=bold, color=blue, ",
                _ => "",
            };
            let label = format!("{:?}", kind);
            writeln!(
                out,
                "  n{} -> n{} [{}label=\"{}\"];",
                src.index(),
                tgt.index(),
                style,
                label
            )
            .unwrap();
        }

        writeln!(out, "}}").unwrap();
        out
    }
}

#[cfg(test)]
mod tests {
    use crate::builder::{CfgBuildSink, DefaultCfgBuilder};
    use crate::types::*;

    fn build_linear_cfg() -> Cfg {
        let mut b = DefaultCfgBuilder::new("TestProc".to_string(), 0..100);

        let entry = b.new_block(BasicBlockKind::Entry);
        let body = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);

        b.add_stmt(
            body,
            StmtRef {
                byte_range: 0..10,
                node_kind: "assign_stmt".to_string(),
            },
        );

        b.add_edge(entry, body, EdgeKind::Normal);
        b.add_edge(body, exit, EdgeKind::Normal);

        b.set_entry(entry);
        b.set_exit(exit);

        b.finish()
    }

    #[test]
    fn dot_output_contains_nodes_and_edges() {
        let cfg = build_linear_cfg();
        let dot = cfg.to_dot();

        assert!(dot.contains("digraph"), "should contain 'digraph'");
        assert!(dot.contains("Entry"), "should contain 'Entry' node label");
        assert!(dot.contains("Exit"), "should contain 'Exit' node label");
        assert!(dot.contains("->"), "should contain edges");
    }
}
