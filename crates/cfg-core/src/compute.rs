use crate::call_graph::CallGraph;
use crate::summary::*;
use crate::types::Cfg;
use petgraph::algo::toposort;
use std::collections::HashMap;

/// Language-specific callback for analyzing a single CFG.
pub trait CfgAnalyzer {
    fn analyze(
        &self,
        proc_id: &ProcId,
        cfg: &Cfg,
        callee_summaries: &HashMap<ProcId, ProcSummary>,
    ) -> ProcSummary;
}

pub struct SummaryComputer;

impl SummaryComputer {
    pub fn compute(
        cfgs: &HashMap<ProcId, Cfg>,
        call_graph: &mut CallGraph,
        analyzer: &dyn CfgAnalyzer,
    ) {
        // Ensure all procs with CFGs are in the call graph.
        for id in cfgs.keys() {
            call_graph.add_proc(id.clone());
        }

        // Try topological sort (fails if there are cycles).
        let topo_result = toposort(&call_graph.graph, None);

        match topo_result {
            Ok(order) => {
                // Process in reverse topological order so leaves (callees) come first.
                for node_idx in order.into_iter().rev() {
                    let proc_id = call_graph.graph[node_idx].clone();
                    if let Some(cfg) = cfgs.get(&proc_id) {
                        let callee_summaries = collect_callee_summaries(&proc_id, call_graph);
                        let summary = analyzer.analyze(&proc_id, cfg, &callee_summaries);
                        call_graph.set_summary(summary);
                    }
                }
            }
            Err(_) => {
                // Cycle detected — fall back to fixed-point iteration.
                const MAX_ROUNDS: usize = 20;
                for _ in 0..MAX_ROUNDS {
                    let mut changed = false;
                    let proc_ids: Vec<ProcId> = cfgs.keys().cloned().collect();
                    for proc_id in &proc_ids {
                        if let Some(cfg) = cfgs.get(proc_id) {
                            let callee_summaries = collect_callee_summaries(proc_id, call_graph);
                            let new_summary = analyzer.analyze(proc_id, cfg, &callee_summaries);
                            let old_summary = call_graph.get_summary(proc_id).cloned();
                            let is_different =
                                old_summary.map(|old| old != new_summary).unwrap_or(true);
                            if is_different {
                                changed = true;
                                call_graph.set_summary(new_summary);
                            }
                        }
                    }
                    if !changed {
                        break;
                    }
                }
            }
        }
    }
}

/// Collect the summaries of all known callees of `proc_id` from the call graph.
fn collect_callee_summaries(
    proc_id: &ProcId,
    call_graph: &CallGraph,
) -> HashMap<ProcId, ProcSummary> {
    call_graph
        .callees(proc_id)
        .into_iter()
        .filter_map(|callee_id| {
            call_graph
                .get_summary(callee_id)
                .map(|s| (callee_id.clone(), s.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{CfgBuildSink, DefaultCfgBuilder};
    use crate::types::{BasicBlockKind, EdgeKind};

    /// Build a minimal entry -> exit CFG for a given proc name.
    fn minimal_cfg(proc_name: &str) -> Cfg {
        let mut b = DefaultCfgBuilder::new(proc_name.to_string(), 0..10);
        let entry = b.new_block(BasicBlockKind::Entry);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_edge(entry, exit, EdgeKind::Normal);
        b.set_entry(entry);
        b.set_exit(exit);
        b.finish()
    }

    /// A mock analyzer:
    /// - Marks param 0 as `Freed` if the proc name contains "Free".
    /// - Propagates `Freed` from any callee summary that has param 0 as `Freed`.
    struct MockAnalyzer;

    impl CfgAnalyzer for MockAnalyzer {
        fn analyze(
            &self,
            proc_id: &ProcId,
            _cfg: &Cfg,
            callee_summaries: &HashMap<ProcId, ProcSummary>,
        ) -> ProcSummary {
            let callee_frees = callee_summaries.values().any(|s| {
                s.param_effects
                    .first()
                    .map(|e| *e == ParamEffect::Freed)
                    .unwrap_or(false)
            });

            let param_effect = if proc_id.qualified_name.contains("Free") || callee_frees {
                ParamEffect::Freed
            } else {
                ParamEffect::None
            };

            ProcSummary {
                id: proc_id.clone(),
                param_effects: vec![param_effect],
                field_effects: vec![],
                returns_new_object: false,
                can_raise: false,
            }
        }
    }

    #[test]
    fn compute_leaf_function_summary() {
        let free_helper = ProcId::new("TestUnit", "FreeHelper");
        let mut cfgs = HashMap::new();
        cfgs.insert(free_helper.clone(), minimal_cfg("FreeHelper"));

        let mut call_graph = CallGraph::new();
        SummaryComputer::compute(&cfgs, &mut call_graph, &MockAnalyzer);

        let summary = call_graph
            .get_summary(&free_helper)
            .expect("summary should be present for FreeHelper");
        assert_eq!(
            summary.param_effects.first(),
            Some(&ParamEffect::Freed),
            "FreeHelper should have param 0 marked as Freed"
        );
    }

    #[test]
    fn compute_transitive_summary() {
        let free_helper = ProcId::new("TestUnit", "FreeHelper");
        let cleanup = ProcId::new("TestUnit", "Cleanup");

        let mut cfgs = HashMap::new();
        cfgs.insert(free_helper.clone(), minimal_cfg("FreeHelper"));
        cfgs.insert(cleanup.clone(), minimal_cfg("Cleanup"));

        let mut call_graph = CallGraph::new();
        // Cleanup calls FreeHelper.
        call_graph.add_call(CallSite {
            caller: cleanup.clone(),
            callee: free_helper.clone(),
            byte_range: 0..5,
        });

        SummaryComputer::compute(&cfgs, &mut call_graph, &MockAnalyzer);

        let cleanup_summary = call_graph
            .get_summary(&cleanup)
            .expect("summary should be present for Cleanup");
        assert_eq!(
            cleanup_summary.param_effects.first(),
            Some(&ParamEffect::Freed),
            "Cleanup should inherit Freed from FreeHelper"
        );
    }

    #[test]
    fn compute_unknown_callee_conservative() {
        let do_stuff = ProcId::new("TestUnit", "DoStuff");
        // OtherUnit.SomeProc has no CFG entry — it's an unknown callee.
        let other_proc = ProcId::new("OtherUnit", "SomeProc");

        let mut cfgs = HashMap::new();
        cfgs.insert(do_stuff.clone(), minimal_cfg("DoStuff"));
        // No CFG for other_proc.

        let mut call_graph = CallGraph::new();
        call_graph.add_call(CallSite {
            caller: do_stuff.clone(),
            callee: other_proc.clone(),
            byte_range: 0..5,
        });

        SummaryComputer::compute(&cfgs, &mut call_graph, &MockAnalyzer);

        let summary = call_graph
            .get_summary(&do_stuff)
            .expect("summary should be present for DoStuff");
        // No callee summary available and proc name doesn't contain "Free",
        // so param effect should be None (conservative: no propagation).
        assert_eq!(
            summary.param_effects.first(),
            Some(&ParamEffect::None),
            "DoStuff should have None param effect when callee is unknown"
        );
    }
}
