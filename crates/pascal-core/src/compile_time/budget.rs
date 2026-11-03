use super::{BudgetSnapshot, IncompleteReason};
use crate::CancellationToken;
use std::alloc::Layout;
use std::mem::size_of;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};

/// Finite prefix-purpose policy; whole-body sessions keep their own defaults.
pub const HEADER_MAX_BYTE_WORK: usize = 64 * 1024 * 1024;
pub const HEADER_MAX_BRANCH_STATES: usize = 256;

/// Steps each source byte entering a session (root, include or provider)
/// adds to `max_work`. Directive and declaration scanning cost a few steps
/// per byte (measured: about 1 to 4, the high end on dense routine code,
/// from per-byte scanning plus per-statement lookups), so the step budget
/// follows the input with twice the densest measured cost.
/// Work that grows faster than the input, such as re-processing it on every
/// forked path, still exhausts the budget.
pub const WORK_PER_SOURCE_BYTE: usize = 8;

/// Byte work each source byte entering a session adds to `max_byte_work`,
/// for the same reason: scanning, fragment, snapshot and mapping copies cost
/// 12 to 15 bytes of byte work per source byte on synthetic library code
/// (tests/compile_time_work_scaling.rs), and SysUtils.pas with its providers
/// (about 2 MB) needs more than the fixed 64 MiB. The grant is about twice
/// the synthetic cost.
pub const BYTE_WORK_PER_SOURCE_BYTE: usize = 32;

/// Work and byte work granted by source bytes entering a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Allowance {
    work: usize,
    byte_work: usize,
}

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
    /// Steps each source byte entering the session adds to `max_work`.
    pub work_per_source_byte: usize,
    pub max_byte_work: usize,
    /// Byte work each source byte entering the session adds to `max_byte_work`.
    pub byte_work_per_source_byte: usize,
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
            // Finite normal-body work headroom. Header selection/reproof
            // inherit this operation policy, with their distinct state fuse.
            max_work: 2_000_000,
            work_per_source_byte: WORK_PER_SOURCE_BYTE,
            max_byte_work: 67_108_864,
            byte_work_per_source_byte: BYTE_WORK_PER_SOURCE_BYTE,
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
    construction: Option<Arc<Mutex<Retention>>>,
}

impl<'a> CompileTimeBudget<'a> {
    /// Limits used by stream consumers for dimension-specific admission.
    pub fn limits(&self) -> CompileTimeLimits {
        self.limits
    }
    pub(crate) fn has_owned_ledger(&self) -> bool {
        matches!(self.counters, Counters::Shared(_))
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
            counters: Counters::Inline(BudgetSnapshot::default(), Allowance::default()),
            layout_query_depth: 0,
            last_failed_charge: None,
            definitions: 0,
            fields: 0,
            variants: 0,
            construction: None,
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
        self.counters.with_snapshot_mut(|current, allowance| {
            let next = admit_snapshot(*current, self.limits, *allowance, work, byte_work, retained)
                .map_err(|(reason, failure)| {
                    self.last_failed_charge = Some(failure);
                    reason
                })?;
            *current = next;
            Ok(())
        })?;
        if let Some(owner) = &self.construction {
            let mut owner = owner.lock().unwrap_or_else(|e| e.into_inner());
            owner.ledger.lock().reserved += retained;
            owner.retained += retained;
        }
        Ok(())
    }

    /// Release live storage only. Neither operation count nor byte work refunds.
    /// Within construction, retire only that selected owner's explicit storage
    /// invoices, never its credit-cell control. Otherwise release only legacy
    /// storage. Independent receipts and the root ledger control cannot be
    /// refunded through this operation.
    pub fn release(&mut self, retained: usize) {
        if let Some(owner) = &self.construction {
            let mut owner = owner.lock().unwrap_or_else(|e| e.into_inner());
            let released = retained.min(owner.retained - owner.floor);
            let mut state = owner.ledger.lock();
            state.reserved -= released;
            state.usage.retained_bytes -= released;
            drop(state);
            owner.retained -= released;
            return;
        }
        match &mut self.counters {
            Counters::Inline(current, _) => {
                current.retained_bytes = current.retained_bytes.saturating_sub(retained);
            }
            Counters::Shared(ledger) => {
                let mut state = ledger.lock();
                let manual = state.usage.retained_bytes
                    - state.reserved
                    - ledger.control_bytes
                    - state.raw_report_floor;
                state.usage.retained_bytes -= retained.min(manual);
            }
        }
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        match &self.counters {
            Counters::Inline(current, _) => *current,
            Counters::Shared(ledger) => ledger.lock().usage,
        }
    }

    fn allowance(&self) -> Allowance {
        match &self.counters {
            Counters::Inline(_, allowance) => *allowance,
            Counters::Shared(ledger) => ledger.lock().allowance,
        }
    }

    /// Work granted on top of `max_work` by source bytes entering the session.
    pub fn work_allowance(&self) -> usize {
        self.allowance().work
    }

    /// Byte work granted on top of `max_byte_work` by source bytes entering
    /// the session.
    pub fn byte_work_allowance(&self) -> usize {
        self.allowance().byte_work
    }

    /// The directive scanner is about to read `scanned` more source bytes:
    /// grant the steps and byte work that processing them legitimately costs
    /// (`work_per_source_byte` and `byte_work_per_source_byte` each), then
    /// charge this read. The grant follows the text actually consumed, so a
    /// header-only or interface-only scan is granted only its prefix. A
    /// refused read grants nothing.
    pub(crate) fn charge_scanned(
        &mut self,
        scanned: usize,
        work: usize,
        byte_work: usize,
    ) -> Result<(), IncompleteReason> {
        let grant = Allowance {
            work: scanned.saturating_mul(self.limits.work_per_source_byte),
            byte_work: scanned.saturating_mul(self.limits.byte_work_per_source_byte),
        };
        self.charge_granted(grant, work, byte_work)
    }

    /// Charge work that is itself proportional to input the session must
    /// consume whole (UTF-8 validation of a raw source), granting exactly
    /// that charge: it does not consume the fixed budget and leaves nothing
    /// for other work to spend. Limits without per-byte grants (factor 0)
    /// fund nothing, so a fixed cap still bounds it.
    pub(crate) fn charge_funded(
        &mut self,
        work: usize,
        byte_work: usize,
    ) -> Result<(), IncompleteReason> {
        let grant = Allowance {
            work: if self.limits.work_per_source_byte > 0 {
                work
            } else {
                0
            },
            byte_work: if self.limits.byte_work_per_source_byte > 0 {
                byte_work
            } else {
                0
            },
        };
        self.charge_granted(grant, work, byte_work)
    }

    /// `charge_funded` when `funded`, otherwise a plain charge.
    pub(crate) fn charge_funded_if(
        &mut self,
        funded: bool,
        work: usize,
        byte_work: usize,
    ) -> Result<(), IncompleteReason> {
        if funded {
            self.charge_funded(work, byte_work)
        } else {
            self.charge(work, byte_work, 0)
        }
    }

    /// Grant `work` and `byte_work` already charged for input the session had
    /// to consume whole (a funded read), leaving nothing for other work.
    /// Limits without per-byte grants (factor 0) fund nothing.
    pub(crate) fn fund_charged(
        &mut self,
        work: usize,
        byte_work: usize,
    ) -> Result<(), IncompleteReason> {
        let grant = Allowance {
            work: if self.limits.work_per_source_byte > 0 {
                work
            } else {
                0
            },
            byte_work: if self.limits.byte_work_per_source_byte > 0 {
                byte_work
            } else {
                0
            },
        };
        self.charge_granted(grant, 0, 0)
    }

    fn charge_granted(
        &mut self,
        grant: Allowance,
        work: usize,
        byte_work: usize,
    ) -> Result<(), IncompleteReason> {
        if self.cancel.is_cancelled() {
            return Err(IncompleteReason::Cancelled);
        }
        self.counters.with_snapshot_mut(|current, allowance| {
            let granted = Allowance {
                work: allowance.work.saturating_add(grant.work),
                byte_work: allowance.byte_work.saturating_add(grant.byte_work),
            };
            let next = admit_snapshot(*current, self.limits, granted, work, byte_work, 0).map_err(
                |(reason, failure)| {
                    self.last_failed_charge = Some(failure);
                    reason
                },
            )?;
            *current = next;
            *allowance = granted;
            Ok(())
        })
    }

    /// Scratch marks exclude independently owned/cache/shared reservations.
    /// These are admission invoices, not differences of whole callback usage.
    pub(crate) fn manual_retained(&self) -> usize {
        if let Some(owner) = &self.construction {
            let owner = owner.lock().unwrap_or_else(|e| e.into_inner());
            return owner.retained - owner.floor;
        }
        match &self.counters {
            Counters::Inline(current, _) => current.retained_bytes,
            Counters::Shared(ledger) => {
                let state = ledger.lock();
                state.usage.retained_bytes
                    - state.reserved
                    - ledger.control_bytes
                    - state.raw_report_floor
            }
        }
    }

    /// Establish construction ownership before calling a fallible builder.
    /// Each successful ordinary admission is attached immediately; the selected
    /// binding is restored on all exits. Named subowners and external callbacks
    /// use their own bindings, not an interval of whole-ledger usage.
    pub(crate) fn admit_compilation<T>(
        &mut self,
        preadmitted: usize,
        build: impl FnOnce(&mut Self) -> Result<T, crate::ResolverError>,
    ) -> Result<AdmittedOwned<T>, crate::ResolverError> {
        let mut initial = InitialInvoice {
            budget: self,
            bytes: preadmitted,
        };
        let reservation = initial
            .mint_retention(1, 0, size_of::<AdmittedOwned<T>>())
            .map_err(|reason| super::resolver_error(reason, &initial))?;
        let increments = initial
            .construction_credit()
            .map_err(|r| super::resolver_error(r, &initial))?;
        if preadmitted != 0 {
            // A named root metadata admission precedes ledger/coordinator
            // controls. Move that exact invoice out of the inline legacy pool.
            let mut owner = increments.lock().unwrap_or_else(|e| e.into_inner());
            owner.ledger.lock().reserved += preadmitted;
            owner.retained += preadmitted;
        }
        initial.bytes = 0;
        let value = {
            let mut binding = ConstructionBinding::new(&mut initial, Some(increments.clone()));
            build(&mut binding)?
        };
        Ok(AdmittedOwned {
            value,
            reservation,
            increments: Some(increments),
        })
    }

    fn construction_credit(&mut self) -> Result<Arc<Mutex<Retention>>, IncompleteReason> {
        let control = size_of::<Mutex<Retention>>() + arc_overhead::<Mutex<Retention>>()?;
        let mut reservation = self.mint_retention(0, 0, control)?;
        reservation.floor = control;
        Ok(Arc::new(Mutex::new(reservation)))
    }

    /// External caches/returns must mint their own receipts, never inherit a
    /// compiler construction binding. Restore that binding on unwind too.
    pub(crate) fn external<R>(&mut self, call: impl FnOnce(&mut Self) -> R) -> R {
        let mut binding = ConstructionBinding::new(self, None);
        call(&mut binding)
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
        Ok(AdmittedOwned {
            value,
            reservation,
            increments: None,
        })
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
        Ok(AdmittedOwned {
            value,
            reservation,
            increments: None,
        })
    }

    fn mint_retention(
        &mut self,
        work: usize,
        byte_work: usize,
        retained: usize,
    ) -> Result<Retention, IncompleteReason> {
        let control_bytes = match self.counters {
            Counters::Inline(..) => size_of::<Ledger>()
                .checked_add(arc_overhead::<Ledger>()?)
                .ok_or(IncompleteReason::MalformedSource)?,
            Counters::Shared(_) => 0,
        };
        let mut binding = ConstructionBinding::new(self, None);
        binding.charge(
            work,
            byte_work,
            retained
                .checked_add(control_bytes)
                .ok_or(IncompleteReason::MalformedSource)?,
        )?;
        drop(binding);
        if let Counters::Inline(usage, allowance) = &self.counters {
            // The ledger/control allocation occurs only AFTER its full credit
            // is admitted. Its storage lasts while the root or any receipt does.
            self.counters = Counters::Shared(Arc::new(Ledger {
                state: Mutex::new(LedgerState {
                    usage: *usage,
                    allowance: *allowance,
                    reserved: 0,
                    raw_report_floor: 0,
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
            floor: 0,
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
    /// Usage and the granted allowance.
    Inline(BudgetSnapshot, Allowance),
    Shared(Arc<Ledger>),
}

impl Counters {
    fn with_snapshot_mut<T>(
        &mut self,
        f: impl FnOnce(&mut BudgetSnapshot, &mut Allowance) -> T,
    ) -> T {
        match self {
            Self::Inline(current, allowance) => f(current, allowance),
            Self::Shared(ledger) => {
                let state = &mut *ledger.lock();
                f(&mut state.usage, &mut state.allowance)
            }
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
    allowance: Allowance,
    reserved: usize,
    // R17 raw report handoff only; unrelated legacy scratch cannot release it.
    raw_report_floor: usize,
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
    floor: usize,
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

struct ConstructionBinding<'b, 'a> {
    budget: &'b mut CompileTimeBudget<'a>,
    previous: Option<Arc<Mutex<Retention>>>,
}

/// Allocation-free owner of the historic first metadata boundary until the
/// exact admission transfers to the construction cell. Controls can refuse
/// between these steps; such refusal must not orphan the initial invoice.
struct InitialInvoice<'b, 'a> {
    budget: &'b mut CompileTimeBudget<'a>,
    bytes: usize,
}
impl<'a> Deref for InitialInvoice<'_, 'a> {
    type Target = CompileTimeBudget<'a>;
    fn deref(&self) -> &Self::Target {
        self.budget
    }
}
impl std::ops::DerefMut for InitialInvoice<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.budget
    }
}
impl Drop for InitialInvoice<'_, '_> {
    fn drop(&mut self) {
        let bytes = self.bytes;
        self.budget.external(|budget| budget.release(bytes));
    }
}
impl<'b, 'a> ConstructionBinding<'b, 'a> {
    fn new(budget: &'b mut CompileTimeBudget<'a>, selected: Option<Arc<Mutex<Retention>>>) -> Self {
        let previous = std::mem::replace(&mut budget.construction, selected);
        Self { budget, previous }
    }
}
impl<'a> Deref for ConstructionBinding<'_, 'a> {
    type Target = CompileTimeBudget<'a>;
    fn deref(&self) -> &Self::Target {
        self.budget
    }
}
impl std::ops::DerefMut for ConstructionBinding<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.budget
    }
}
impl Drop for ConstructionBinding<'_, '_> {
    fn drop(&mut self) {
        self.budget.construction = self.previous.take();
    }
}

/// An admitted, move-only payload. Fields are private so no operation can move
/// its value out and refund the credits while leaving that payload live.
#[derive(Debug)]
pub struct AdmittedOwned<T> {
    // Field order guarantees value destruction before the reservation refunds.
    value: T,
    reservation: Retention,
    increments: Option<Arc<Mutex<Retention>>>,
}

impl<T> Deref for AdmittedOwned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> AdmittedOwned<T> {
    pub(crate) fn budget_snapshot(&self) -> BudgetSnapshot {
        self.reservation.ledger.lock().usage
    }

    pub(crate) fn shares_root<U>(&self, other: &AdmittedOwned<U>) -> bool {
        Arc::ptr_eq(&self.reservation.ledger, &other.reservation.ledger)
    }

    pub(crate) fn shares_shared_root<U>(&self, other: &AdmittedShared<U>) -> bool {
        Arc::ptr_eq(&self.reservation.ledger, &other.handle.ledger)
    }

    /// Explicit R17 raw finish only. Move actual minted backing credits into a
    /// protected floor in their own ledger, retiring the dead inline carrier.
    pub(crate) fn into_raw_report_floor(mut self) {
        self.seal_increments().expect("unbound report receipt");
        let Self {
            value,
            mut reservation,
            ..
        } = self;
        drop(value);
        let control = size_of::<Self>();
        let mut state = reservation.ledger.lock();
        state.reserved -= reservation.retained;
        state.usage.retained_bytes -= control;
        state.raw_report_floor += reservation.retained - control;
        reservation.retained = 0;
    }

    /// Consume one actual owner into a different inline carrier. Owned backing
    /// and already minted increments move equal-sum, without a payload visitor
    /// or clone. The receiving carrier is admitted before transformation; any
    /// allocation in the private transform must already have its named invoice.
    pub(crate) fn map_value<U>(
        mut self,
        budget: &mut CompileTimeBudget<'_>,
        transform: impl FnOnce(T) -> U,
    ) -> Result<AdmittedOwned<U>, IncompleteReason> {
        if !self.is_admitted_by(budget) {
            return Err(IncompleteReason::MalformedSource);
        }
        self.seal_increments()?;
        let mut reservation = budget.mint_retention(0, 0, size_of::<AdmittedOwned<U>>())?;
        reservation.retained += self.reservation.retained;
        self.reservation.retained = 0;
        let Self { value, .. } = self;
        let value = transform(value);
        let mut result = AdmittedOwned {
            value,
            reservation,
            increments: None,
        };
        // The original inline carrier died during the consuming transform.
        // This is its originally minted control invoice, never payload length.
        result.retire_storage(size_of::<Self>())?;
        Ok(result)
    }

    fn seal_increments(&mut self) -> Result<(), IncompleteReason> {
        if let Some(cell) = self.increments.take() {
            let cell = match Arc::try_unwrap(cell) {
                Ok(cell) => cell,
                Err(cell) => {
                    self.increments = Some(cell);
                    return Err(IncompleteReason::MalformedSource);
                }
            };
            let mut credit = cell.into_inner().unwrap_or_else(|e| e.into_inner());
            self.reservation.retained += credit.retained - credit.floor;
            // The unique, unbound Arc/Mutex allocation is gone. Payload and
            // capacity credits remain; only that named control invoice retires.
            credit.retained = credit.floor;
        }
        Ok(())
    }
    /// Consume a named, independently admitted field into an already admitted
    /// destination. Move its minted credits BEFORE the payload mutation; this
    /// neither measures a callback interval nor extracts uncredited storage.
    pub(crate) fn move_field<U>(
        &mut self,
        mut field: AdmittedOwned<U>,
        budget: &CompileTimeBudget<'_>,
        install: impl FnOnce(&mut T, U),
    ) -> Result<(), IncompleteReason> {
        let carrier = size_of::<AdmittedOwned<U>>();
        if !self.is_admitted_by(budget)
            || !field.is_admitted_by(budget)
            || field.reservation.retained < carrier
        {
            return Err(IncompleteReason::MalformedSource);
        }
        field.seal_increments()?;
        self.reservation.retained += field.reservation.retained;
        field.reservation.retained = 0;
        let AdmittedOwned { value, .. } = field;
        install(&mut self.value, value);
        // This field's original inline carrier is gone; the destination's
        // receiving inline fields were admitted by its own constructor. Retire
        // only this named, originally minted control invoice, not payload len.
        self.retire_storage(carrier)?;
        Ok(())
    }

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
        // Prior private construction bindings have ended. Seal their exact
        // invoices before named backing replacement can retire an old buffer.
        self.seal_increments()?;
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
        if self.increments.is_none() {
            self.increments = Some(budget.construction_credit()?);
        }
        let mut binding = ConstructionBinding::new(budget, self.increments.clone());
        Ok(update(&mut self.value, &mut binding))
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
        // Mutation is complete. Move its already-minted increments into this
        // backing reservation, retiring only the dead unique binding control.
        self.seal_increments()?;
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
            construction: None,
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
    allowance: Allowance,
    work: usize,
    byte_work: usize,
    retained: usize,
) -> Result<BudgetSnapshot, ChargeFailure> {
    let check = |current: usize, added: usize, maximum: usize, name| {
        admit(current, added, maximum, name)
            .map_err(|reason| (reason, (name, current.saturating_add(added), maximum)))
    };
    Ok(BudgetSnapshot {
        work: check(
            current.work,
            work,
            limits.max_work.saturating_add(allowance.work),
            "max_work",
        )?,
        byte_work: check(
            current.byte_work,
            byte_work,
            limits.max_byte_work.saturating_add(allowance.byte_work),
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
