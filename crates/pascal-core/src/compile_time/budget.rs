use super::{BudgetSnapshot, IncompleteReason};
use crate::CancellationToken;
use std::alloc::Layout;
use std::mem::size_of;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};

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
    counters: Counters,
    layout_query_depth: usize,
    last_failed_charge: Option<(&'static str, usize, usize)>,
    definitions: usize,
    fields: usize,
    variants: usize,
    manual_scope: Option<usize>,
}

impl<'a> CompileTimeBudget<'a> {
    /// Limits used by stream consumers for dimension-specific admission.
    pub fn limits(&self) -> CompileTimeLimits {
        self.limits
    }
    pub(crate) fn cancel_token(&self) -> &'a dyn CancellationToken {
        self.cancel
    }
    pub(crate) fn constrain_retained(&mut self, maximum: usize) -> Result<(), IncompleteReason> {
        self.limits.max_retained_bytes = self.limits.max_retained_bytes.min(maximum);
        self.charge(0, 0, 0)
    }

    pub fn new(limits: CompileTimeLimits, cancel: &'a dyn CancellationToken) -> Self {
        Self {
            limits,
            cancel,
            counters: Counters::Inline(BudgetSnapshot::default()),
            layout_query_depth: 0,
            last_failed_charge: None,
            definitions: 0,
            fields: 0,
            variants: 0,
            manual_scope: None,
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
        self.counters.with_snapshot_mut(|current| {
            let next = admit_snapshot(*current, self.limits, work, byte_work, retained).map_err(
                |(reason, failure)| {
                    self.last_failed_charge = Some(failure);
                    reason
                },
            )?;
            *current = next;
            Ok(())
        })?;
        if let Some(scope) = &mut self.manual_scope {
            *scope += retained;
        }
        Ok(())
    }

    /// Release live storage only. Neither operation count nor byte work refunds.
    /// Releasing more than legacy storage clamps to zero rather than wrapping.
    /// Receipt-owned storage and the live ledger cannot be refunded this way.
    pub fn release(&mut self, retained: usize) {
        let retained = if let Some(scope) = &mut self.manual_scope {
            let released = retained.min(*scope);
            *scope -= released;
            released
        } else {
            retained
        };
        match &mut self.counters {
            Counters::Inline(current) => {
                current.retained_bytes = current.retained_bytes.saturating_sub(retained);
            }
            Counters::Shared(ledger) => {
                let mut state = ledger.lock();
                let manual = state.usage.retained_bytes - state.reserved - ledger.control_bytes;
                state.usage.retained_bytes -= retained.min(manual);
            }
        }
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        match &self.counters {
            Counters::Inline(current) => *current,
            Counters::Shared(ledger) => ledger.lock().usage,
        }
    }

    /// Scratch marks exclude independently owned/cache/shared reservations.
    /// These are admission invoices, not differences of whole callback usage.
    pub(crate) fn manual_retained(&self) -> usize {
        if let Some(scope) = self.manual_scope {
            return scope;
        }
        match &self.counters {
            Counters::Inline(current) => current.retained_bytes,
            Counters::Shared(ledger) => {
                let state = ledger.lock();
                state.usage.retained_bytes - state.reserved - ledger.control_bytes
            }
        }
    }

    /// Compilation owns every ordinary retained admission issued in this scope.
    /// Nested receipt-backed cache/graph/output owners are invoiced separately.
    /// The invoice transfers only on success; on error all scoped locals have
    /// already died before their invoice is released. No callback delta is used.
    pub(crate) fn admit_compilation<T>(
        &mut self,
        build: impl FnOnce(&mut Self) -> Result<T, crate::ResolverError>,
    ) -> Result<AdmittedOwned<T>, crate::ResolverError> {
        let mut reservation = self
            .mint_retention(1, 0, size_of::<AdmittedOwned<T>>())
            .map_err(|reason| super::resolver_error(reason, self))?;
        let parent = self.manual_scope.replace(0);
        let result = build(self);
        let invoice = self.manual_scope.take().unwrap_or(0);
        self.manual_scope = parent;
        match result {
            Ok(value) => {
                reservation.absorb_manual(invoice);
                Ok(AdmittedOwned { value, reservation })
            }
            Err(error) => {
                self.release_invoice(invoice);
                Err(error)
            }
        }
    }

    fn release_invoice(&mut self, invoice: usize) {
        let parent = self.manual_scope.take();
        self.release(invoice);
        self.manual_scope = parent;
    }

    /// Admit an owned carrier and its backing payload before calling `build`.
    /// `retained_payload` excludes the inline carrier/control fields, which this
    /// method admits too. The returned value cannot shed its receipt through an
    /// `into_inner` operation, and is deliberately not `Clone`.
    ///
    /// Dropping the carrier releases exactly these retained credits, not work or
    /// byte work. Cache owners and returned providers must use separate carriers.
    pub fn admit_owned<T>(
        &mut self,
        work: usize,
        byte_work: usize,
        retained_payload: usize,
        build: impl FnOnce() -> T,
    ) -> Result<AdmittedOwned<T>, IncompleteReason> {
        let retained = retained_payload
            .checked_add(size_of::<AdmittedOwned<T>>())
            .ok_or(IncompleteReason::MalformedSource)?;
        let reservation = self.mint_retention(work, byte_work, retained)?;
        let value = build();
        Ok(AdmittedOwned { value, reservation })
    }

    /// Store construction may fail after admission. The constructor's locals
    /// die before the reservation; performed work is never rolled back.
    pub(crate) fn admit_fallible<T, E>(
        &mut self,
        work: usize,
        byte_work: usize,
        payload: usize,
        refusal: impl FnOnce(IncompleteReason, &Self) -> E,
        build: impl FnOnce() -> Result<T, E>,
    ) -> Result<AdmittedOwned<T>, E> {
        let retained = payload
            .checked_add(size_of::<AdmittedOwned<T>>())
            .ok_or(IncompleteReason::MalformedSource);
        let reservation = retained
            .and_then(|n| self.mint_retention(work, byte_work, n))
            .map_err(|r| refusal(r, self))?;
        let value = build()?;
        Ok(AdmittedOwned { value, reservation })
    }

    fn mint_retention(
        &mut self,
        work: usize,
        byte_work: usize,
        retained: usize,
    ) -> Result<Retention, IncompleteReason> {
        let control_bytes = match self.counters {
            Counters::Inline(_) => size_of::<Ledger>()
                .checked_add(arc_overhead::<Ledger>()?)
                .ok_or(IncompleteReason::MalformedSource)?,
            Counters::Shared(_) => 0,
        };
        self.charge(
            work,
            byte_work,
            retained
                .checked_add(control_bytes)
                .ok_or(IncompleteReason::MalformedSource)?,
        )?;
        if let Some(scope) = &mut self.manual_scope {
            *scope -= retained + control_bytes;
        }
        if let Counters::Inline(usage) = &self.counters {
            // The ledger/control allocation occurs only AFTER its full credit
            // is admitted. Its storage lasts while the root or any receipt does.
            self.counters = Counters::Shared(Arc::new(Ledger {
                state: Mutex::new(LedgerState {
                    usage: *usage,
                    reserved: 0,
                }),
                control_bytes,
            }));
        }
        let Counters::Shared(ledger) = &self.counters else {
            unreachable!()
        };
        ledger.lock().reserved += retained;
        Ok(Retention {
            ledger: ledger.clone(),
            retained,
        })
    }

    fn owns(&self, reservation: &Retention) -> bool {
        matches!(&self.counters, Counters::Shared(ledger) if Arc::ptr_eq(ledger, &reservation.ledger))
    }

    /// New admitted syntax consumes the request-wide allowance. Snapshot copies
    /// retain already admitted identities and do not call these methods.
    pub(super) fn admit_definition(&mut self) -> Result<(), IncompleteReason> {
        self.charge(1, 0, 0)?;
        self.definitions = self.admit_counter(
            self.definitions,
            1,
            self.limits.max_definitions,
            "max_definitions",
        )?;
        Ok(())
    }
    pub(super) fn admit_field(&mut self) -> Result<(), IncompleteReason> {
        self.charge(1, 0, 0)?;
        self.fields = self.admit_counter(self.fields, 1, self.limits.max_fields, "max_fields")?;
        Ok(())
    }
    pub(super) fn admit_variant(&mut self) -> Result<(), IncompleteReason> {
        self.charge(1, 0, 0)?;
        self.variants = self.admit_counter(
            self.variants,
            1,
            self.limits.max_variant_alternatives,
            "max_variant_alternatives",
        )?;
        Ok(())
    }

    fn admit_counter(
        &mut self,
        current: usize,
        added: usize,
        maximum: usize,
        name: &'static str,
    ) -> Result<usize, IncompleteReason> {
        let result = admit(current, added, maximum, name);
        if result.is_err() {
            self.last_failed_charge = Some((name, current.saturating_add(added), maximum));
        }
        result
    }

    pub(super) fn failure_observed(&self, name: &'static str, maximum: usize) -> Option<usize> {
        self.last_failed_charge
            .filter(|(failed, _, limit)| *failed == name && *limit == maximum)
            .map(|(_, observed, _)| observed)
    }

    /// SizeOf inside an ordinal expression may recursively ask for that same
    /// array's layout. This guard spans parser/layout calls, not just one local
    /// storage traversal, and never resets request work/byte counters.
    pub(super) fn with_layout_query<T>(
        &mut self,
        query: impl FnOnce(&mut Self) -> Result<T, IncompleteReason>,
    ) -> Result<T, IncompleteReason> {
        self.charge(1, 0, 0)?;
        if self.layout_query_depth >= self.limits.max_type_depth {
            return Err(IncompleteReason::Limit {
                name: "max_type_depth",
                maximum: self.limits.max_type_depth,
            });
        }
        self.layout_query_depth += 1;
        let result = query(self);
        self.layout_query_depth -= 1;
        result
    }
}

enum Counters {
    Inline(BudgetSnapshot),
    Shared(Arc<Ledger>),
}

impl Counters {
    fn with_snapshot_mut<T>(&mut self, f: impl FnOnce(&mut BudgetSnapshot) -> T) -> T {
        match self {
            Self::Inline(current) => f(current),
            Self::Shared(ledger) => f(&mut ledger.lock().usage),
        }
    }
}

#[derive(Debug)]
struct Ledger {
    state: Mutex<LedgerState>,
    control_bytes: usize,
}

#[derive(Debug)]
struct LedgerState {
    usage: BudgetSnapshot,
    reserved: usize,
}

impl Ledger {
    fn lock(&self) -> MutexGuard<'_, LedgerState> {
        // No user callback runs with the lock held. Recovering poison preserves
        // the accounting owner even if a caller previously unwound.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A credit is minted only by its root budget. Moving it transfers ownership;
/// neither a callback nor a length measurement can create or duplicate credits.
#[derive(Debug)]
struct Retention {
    ledger: Arc<Ledger>,
    retained: usize,
}

impl Drop for Retention {
    fn drop(&mut self) {
        let mut state = self.ledger.lock();
        // Every credit is moved exactly once or split by an equal-sum transfer.
        // The payload's Drop runs first; no borrowed budget/cancel is touched.
        state.reserved -= self.retained;
        state.usage.retained_bytes -= self.retained;
    }
}

impl Retention {
    fn absorb_manual(&mut self, invoice: usize) {
        self.ledger.lock().reserved += invoice;
        self.retained += invoice;
    }
}

/// An admitted, move-only payload. Fields are private so no operation can move
/// its value out and refund the credits while leaving that payload live.
#[derive(Debug)]
pub struct AdmittedOwned<T> {
    // Field order guarantees value destruction before the reservation refunds.
    value: T,
    reservation: Retention,
}

impl<T> Deref for AdmittedOwned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> AdmittedOwned<T> {
    pub(crate) fn retire_storage(&mut self, bytes: usize) -> Result<(), IncompleteReason> {
        if bytes > self.reservation.retained {
            return Err(IncompleteReason::MalformedSource);
        }
        let mut state = self.reservation.ledger.lock();
        state.reserved -= bytes;
        state.usage.retained_bytes -= bytes;
        self.reservation.retained -= bytes;
        Ok(())
    }
    /// Attach a known construction increment BEFORE mutation, including on
    /// unwind with a surviving owner. No callback interval/delta is inferred.
    pub(crate) fn grow<R>(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
        work: usize,
        bytes: usize,
        retained: usize,
        update: impl FnOnce(&mut T) -> R,
    ) -> Result<R, IncompleteReason> {
        if !budget.owns(&self.reservation) {
            return Err(IncompleteReason::MalformedSource);
        }
        let mut credit = budget.mint_retention(work, bytes, retained)?;
        self.reservation.retained += credit.retained;
        credit.retained = 0;
        Ok(update(&mut self.value))
    }

    pub(crate) fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Mutate an actual owner through the same root; new ordinary admissions
    /// follow that owner even if a fallible mutation leaves partial state.
    pub(crate) fn with_budget_mut<R>(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
        update: impl FnOnce(&mut T, &mut CompileTimeBudget<'_>) -> R,
    ) -> Result<R, IncompleteReason> {
        if !budget.owns(&self.reservation) {
            return Err(IncompleteReason::MalformedSource);
        }
        let parent = budget.manual_scope.replace(0);
        let result = update(&mut self.value, budget);
        let invoice = budget.manual_scope.take().unwrap_or(0);
        budget.manual_scope = parent;
        self.reservation.absorb_manual(invoice);
        Ok(result)
    }
    /// Validate a returned capability without accepting unadmitted or other-root
    /// data as proof. This checks the opaque minting identity, not payload size.
    pub fn is_admitted_by(&self, budget: &CompileTimeBudget<'_>) -> bool {
        budget.owns(&self.reservation)
    }

    /// Move the payload into one admitted shared backing allocation. A refusal
    /// drops this owner normally, refunding only its genuinely dead payload.
    pub fn into_shared(
        mut self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedShared<T>, IncompleteReason> {
        if !budget.owns(&self.reservation) {
            return Err(IncompleteReason::MalformedSource);
        }
        let control = arc_overhead::<Self>()?;
        let retained = size_of::<AdmittedShared<T>>()
            .checked_add(control)
            .ok_or(IncompleteReason::MalformedSource)?;
        let mut handle = budget.mint_retention(1, 0, retained)?;
        // Control bytes follow the backing's last owner, not an arbitrary
        // shared handle. This is a credit transfer, not a new reservation.
        handle.retained -= control;
        self.reservation.retained += control;
        Ok(AdmittedShared {
            inner: Arc::new(self),
            handle,
        })
    }
}

/// Shared admitted backing with a distinct credit for each owned handle.
/// Clones must go through `try_clone` with the SAME root accounting authority.
#[derive(Debug)]
pub struct AdmittedShared<T> {
    inner: Arc<AdmittedOwned<T>>,
    handle: Retention,
}

impl<T> Deref for AdmittedShared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner.value
    }
}

impl<T> AdmittedShared<T> {
    /// Borrow the SAME root ledger for an existing resolver/store call chain.
    /// This is not a new root or copied retained counter. Only store work and
    /// storage use this bridge; declaration counters remain on the root budget.
    pub(crate) fn store_budget<'a>(
        &self,
        limits: CompileTimeLimits,
        cancel: &'a dyn CancellationToken,
    ) -> CompileTimeBudget<'a> {
        CompileTimeBudget {
            limits,
            cancel,
            counters: Counters::Shared(self.handle.ledger.clone()),
            layout_query_depth: 0,
            last_failed_charge: None,
            definitions: 0,
            fields: 0,
            variants: 0,
            manual_scope: None,
        }
    }
    /// Internal shared carrier envelope: its Arc/control credit follows the
    /// wrapped shared handle, and is admitted before allocating the envelope.
    pub(crate) fn into_arc(
        mut self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Arc<Self>, IncompleteReason> {
        if !budget.owns(&self.handle) {
            return Err(IncompleteReason::MalformedSource);
        }
        let mut control = budget.mint_retention(1, 0, arc_overhead::<Self>()?)?;
        self.handle.retained += control.retained;
        control.retained = 0;
        Ok(Arc::new(self))
    }
    pub fn is_admitted_by(&self, budget: &CompileTimeBudget<'_>) -> bool {
        budget.owns(&self.handle)
    }

    /// Current aggregate usage, including credits held by other root owners.
    /// Unlike a copied publication-time snapshot, this remains coherent after
    /// output transfer and after the borrowed budget/token have been dropped.
    pub fn budget_snapshot(&self) -> BudgetSnapshot {
        self.handle.ledger.lock().usage
    }

    pub fn try_clone(&self, budget: &mut CompileTimeBudget<'_>) -> Result<Self, IncompleteReason> {
        if !budget.owns(&self.handle) {
            return Err(IncompleteReason::MalformedSource);
        }
        let handle = budget.mint_retention(1, 0, size_of::<Self>())?;
        Ok(Self {
            inner: self.inner.clone(),
            handle,
        })
    }
}

fn arc_overhead<T>() -> Result<usize, IncompleteReason> {
    // Arc's two reference counters precede the payload. Include alignment
    // padding too, including for callers with over-aligned payloads.
    Layout::new::<[usize; 2]>()
        .extend(Layout::new::<T>())
        .map_err(|_| IncompleteReason::MalformedSource)?
        .0
        .pad_to_align()
        .size()
        .checked_sub(size_of::<T>())
        .ok_or(IncompleteReason::MalformedSource)
}

type ChargeFailure = (IncompleteReason, (&'static str, usize, usize));

fn admit_snapshot(
    current: BudgetSnapshot,
    limits: CompileTimeLimits,
    work: usize,
    byte_work: usize,
    retained: usize,
) -> Result<BudgetSnapshot, ChargeFailure> {
    let check = |current: usize, added: usize, maximum: usize, name| {
        admit(current, added, maximum, name)
            .map_err(|reason| (reason, (name, current.saturating_add(added), maximum)))
    };
    Ok(BudgetSnapshot {
        work: check(current.work, work, limits.max_work, "max_work")?,
        byte_work: check(
            current.byte_work,
            byte_work,
            limits.max_byte_work,
            "max_byte_work",
        )?,
        retained_bytes: check(
            current.retained_bytes,
            retained,
            limits.max_retained_bytes,
            "max_retained_bytes",
        )?,
    })
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
