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

/// Build routine flow models and run the bounded H2164/H2077 analyses.
#[allow(clippy::too_many_arguments)]
pub(super) fn flow_diagnostics_with_budget(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    prior_diagnostics: &[SemanticDiagnostic],
    _capacity: usize,
    diagnostic_cap_reached: &mut bool,
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
        diagnostic_cap_reached,
        cancel,
        budget,
    )
}
