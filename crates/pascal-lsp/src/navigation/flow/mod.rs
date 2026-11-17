#![allow(dead_code)] // Model fields are intentionally consumed by the next flow-analysis tasks.

mod model;
mod usage;

#[cfg(test)]
mod tests;

use super::{AssistanceBudget, Document, NavigationIndex, SemanticDiagnostic};
use lsp_types::Url;
use std::sync::atomic::AtomicBool;

use model::routine_flows;
#[allow(unused_imports)]
pub(crate) use model::{
    Access, FlowExclusion, LocalKind, LocalVar, LocalVarParameterMode, RoutineFlow, VarOccurrence,
    VarType,
};

/// Flow diagnostics are intentionally empty until the analyses are added.
pub(super) fn flow_diagnostics_with_budget(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    prior_diagnostics: &[SemanticDiagnostic],
    _capacity: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<SemanticDiagnostic>, String> {
    let flows = routine_flows(index, uri, document, cancel, budget)?;
    usage::flow_diagnostics(
        index,
        uri,
        document,
        &flows,
        prior_diagnostics,
        _capacity,
        cancel,
        budget,
    )
}
