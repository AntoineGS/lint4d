use crate::summary::*;
use petgraph::graph::DiGraph;
use petgraph::Direction;
use std::collections::HashMap;

#[derive(Debug)]
pub struct CallGraph {
    pub graph: DiGraph<ProcId, CallSite>,
    pub summaries: HashMap<ProcId, ProcSummary>,
    node_map: HashMap<ProcId, petgraph::graph::NodeIndex>,
}

impl CallGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            summaries: HashMap::new(),
            node_map: HashMap::new(),
        }
    }

    pub fn add_proc(&mut self, id: ProcId) {
        if !self.node_map.contains_key(&id) {
            let idx = self.graph.add_node(id.clone());
            self.node_map.insert(id, idx);
        }
    }

    pub fn add_call(&mut self, site: CallSite) {
        self.add_proc(site.caller.clone());
        self.add_proc(site.callee.clone());
        let caller_idx = self.node_map[&site.caller];
        let callee_idx = self.node_map[&site.callee];
        self.graph.add_edge(caller_idx, callee_idx, site);
    }

    pub fn get_summary(&self, id: &ProcId) -> Option<&ProcSummary> {
        self.summaries.get(id)
    }

    pub fn set_summary(&mut self, summary: ProcSummary) {
        self.summaries.insert(summary.id.clone(), summary);
    }

    pub fn callees(&self, caller: &ProcId) -> Vec<&ProcId> {
        match self.node_map.get(caller) {
            None => vec![],
            Some(&idx) => self.graph.neighbors(idx).map(|n| &self.graph[n]).collect(),
        }
    }

    pub fn callers(&self, callee: &ProcId) -> Vec<&ProcId> {
        match self.node_map.get(callee) {
            None => vec![],
            Some(&idx) => self
                .graph
                .neighbors_directed(idx, Direction::Incoming)
                .map(|n| &self.graph[n])
                .collect(),
        }
    }

    pub fn to_dot(&self) -> String {
        use petgraph::dot::{Config, Dot};
        format!(
            "{:?}",
            Dot::with_config(&self.graph, &[Config::EdgeNoLabel])
        )
    }
}

impl Default for CallGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form_create() -> ProcId {
        ProcId::new("MainForm", "TMainForm.FormCreate")
    }

    fn free_helper() -> ProcId {
        ProcId::new("MainForm", "TMainForm.FreeHelper")
    }

    #[test]
    fn call_graph_tracks_callers_and_callees() {
        let mut cg = CallGraph::new();
        let site = CallSite {
            caller: form_create(),
            callee: free_helper(),
            byte_range: 0..10,
        };
        cg.add_call(site);

        let callees = cg.callees(&form_create());
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0], &free_helper());

        let callers = cg.callers(&free_helper());
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0], &form_create());
    }

    #[test]
    fn call_graph_stores_summaries() {
        let mut cg = CallGraph::new();
        let id = form_create();
        let summary = ProcSummary {
            id: id.clone(),
            param_effects: vec![ParamEffect::None],
            field_effects: vec![],
            returns_new_object: false,
            can_raise: false,
        };
        cg.set_summary(summary);

        let retrieved = cg.get_summary(&id).expect("summary should be present");
        assert_eq!(retrieved.id, id);
        assert!(!retrieved.returns_new_object);
        assert!(!retrieved.can_raise);
    }

    #[test]
    fn call_graph_dot_output() {
        let mut cg = CallGraph::new();
        let site = CallSite {
            caller: form_create(),
            callee: free_helper(),
            byte_range: 0..5,
        };
        cg.add_call(site);

        let dot = cg.to_dot();
        assert!(
            dot.contains("digraph"),
            "DOT output should contain 'digraph'"
        );
        assert!(dot.contains("->"), "DOT output should contain '->'");
    }
}
