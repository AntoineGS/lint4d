use super::{BudgetSnapshot, IncompleteReason};
use crate::CancellationToken;

/// Request-wide limits, including dimensions enforced by later consumers.
/// Pending fragments have their own admission limit in addition to total
/// retained bytes; generic retained storage is not automatically pending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileTimeLimits {
    pub max_definitions: usize,
    pub max_fields: usize,
    pub max_variant_alternatives: usize,
    pub max_pending_bytes: usize,
    pub max_scope_depth: usize,
    pub max_type_depth: usize,
    pub max_array_dimensions: usize,
    pub max_branch_states: usize,
    pub max_provider_queries: usize,
    pub max_work: usize,
    pub max_byte_work: usize,
    pub max_retained_bytes: usize,
}

impl Default for CompileTimeLimits {
    fn default() -> Self {
        Self {
            max_definitions: 32_768,
            max_fields: 32_768,
            max_variant_alternatives: 32_768,
            max_pending_bytes: 1_048_576,
            max_scope_depth: 256,
            max_type_depth: 256,
            max_array_dimensions: 256,
            max_branch_states: 64,
            max_provider_queries: 256,
            max_work: 1_000_000,
            max_byte_work: 16_777_216,
            max_retained_bytes: 1_048_576,
        }
    }
}

/// One accounting authority shared by declaration, branch and provider work.
/// Call `charge` before allocation, copying, scanning or external reads. Failed
/// admission is transactional and allocates no requested payload.
pub struct CompileTimeBudget<'a> {
    limits: CompileTimeLimits,
    cancel: &'a dyn CancellationToken,
    counters: BudgetSnapshot,
}

impl<'a> CompileTimeBudget<'a> {
    pub fn new(limits: CompileTimeLimits, cancel: &'a dyn CancellationToken) -> Self {
        Self {
            limits,
            cancel,
            counters: BudgetSnapshot::default(),
        }
    }

    pub fn charge(
        &mut self,
        work: usize,
        byte_work: usize,
        retained: usize,
    ) -> Result<(), IncompleteReason> {
        if self.cancel.is_cancelled() {
            return Err(IncompleteReason::Cancelled);
        }
        let work = admit(self.counters.work, work, self.limits.max_work, "max_work")?;
        let byte_work = admit(
            self.counters.byte_work,
            byte_work,
            self.limits.max_byte_work,
            "max_byte_work",
        )?;
        let retained_bytes = admit(
            self.counters.retained_bytes,
            retained,
            self.limits.max_retained_bytes,
            "max_retained_bytes",
        )?;
        self.counters = BudgetSnapshot {
            work,
            byte_work,
            retained_bytes,
        };
        Ok(())
    }

    /// Release live storage only. Neither operation count nor byte work refunds.
    /// Releasing more than retained clamps to zero rather than wrapping.
    pub fn release(&mut self, retained: usize) {
        self.counters.retained_bytes = self.counters.retained_bytes.saturating_sub(retained);
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        self.counters
    }
}

fn admit(
    current: usize,
    added: usize,
    maximum: usize,
    name: &'static str,
) -> Result<usize, IncompleteReason> {
    current
        .checked_add(added)
        .filter(|value| *value <= maximum)
        .ok_or(IncompleteReason::Limit { name, maximum })
}
