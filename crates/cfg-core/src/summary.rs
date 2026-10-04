use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcId {
    pub unit_name: String,
    pub qualified_name: String,
}

impl ProcId {
    pub fn new(unit_name: &str, qualified_name: &str) -> Self {
        Self {
            unit_name: unit_name.to_string(),
            qualified_name: qualified_name.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamEffect {
    None,
    Freed,
    FreedConditionally,
    Reassigned,
    PassedToFreeing(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldAction {
    Freed,
    Created,
    Reassigned,
    FreedConditionally,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldEffect {
    pub field_name: String,
    pub effect: FieldAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionEffect {
    /// Always starts a transaction (unconditional StartTransaction).
    Starts,
    /// Starts only when not already in transaction (guarded).
    StartsConditionally,
    /// Always commits.
    Commits,
    /// Commits only if transaction was started here (guarded).
    CommitsConditionally,
    /// Always rolls back.
    Rollbacks,
    /// Rolls back only if transaction was started here (guarded).
    RollbacksConditionally,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSummary {
    pub id: ProcId,
    pub param_effects: Vec<ParamEffect>,
    pub field_effects: Vec<FieldEffect>,
    pub returns_new_object: bool,
    pub can_raise: bool,
    pub transaction_effects: Vec<TransactionEffect>,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub caller: ProcId,
    pub callee: ProcId,
    pub byte_range: Range<usize>,
}
