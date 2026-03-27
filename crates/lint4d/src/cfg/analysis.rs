use cfg_core::call_graph::CallGraph;
use cfg_core::summary::ProcId;
use cfg_core::types::Cfg;
use std::collections::HashMap;

use crate::dcu::ProjectContext;

pub struct AnalysisContext<'a> {
    pub cfgs: HashMap<ProcId, Cfg>,
    pub call_graph: CallGraph,
    pub project: &'a ProjectContext,
}

impl<'a> AnalysisContext<'a> {
    pub fn new(
        cfgs: HashMap<ProcId, Cfg>,
        call_graph: CallGraph,
        project: &'a ProjectContext,
    ) -> Self {
        Self {
            cfgs,
            call_graph,
            project,
        }
    }

    pub fn get_cfg(&self, proc_id: &ProcId) -> Option<&Cfg> {
        self.cfgs.get(proc_id)
    }
}
