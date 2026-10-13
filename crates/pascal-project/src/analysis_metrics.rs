use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

#[repr(usize)]
#[derive(Clone, Copy, Debug)]
pub enum Metric {
    AuthorizedPayloadReadAttempts = 0,
    AuthorizedPayloadBytes = 1,
    AuthorizedPayloadReadFailures = 2,
    ProjectStampProbes = 3,
    LegacyHashBytes = 4,
    CoreParserInvocations = 5,
    LspDocumentPreparations = 6,
    ResolverMetadataParses = 7,
    ImportBindingCalls = 8,
    ImportBindingPairs = 9,
    ProjectCacheProbeCalls = 10,
    ProjectCacheProbeNanos = 11,
}

pub const METRIC_COUNT: usize = 12;

/// Process aggregate, not an atomic filesystem/index snapshot and not
/// per-request attribution.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricSnapshot {
    pub schema: u32,
    pub counts: [u64; METRIC_COUNT],
    pub saturated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaError {
    SchemaMismatch,
    Saturated,
    Regressed,
}

pub struct Counters {
    enabled: AtomicBool,
    counts: [AtomicU64; METRIC_COUNT],
    saturated: AtomicBool,
}

impl Counters {
    pub const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            counts: [const { AtomicU64::new(0) }; METRIC_COUNT],
            saturated: AtomicBool::new(false),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Relaxed)
    }

    pub fn record(&self, metric: Metric, amount: u64) {
        if !self.enabled.load(Relaxed) {
            return;
        }
        let previous = self.counts[metric as usize]
            .try_update(Relaxed, Relaxed, |n| Some(n.saturating_add(amount)))
            .expect("the update closure always returns Some");
        if previous.checked_add(amount).is_none() {
            self.saturated.store(true, Relaxed);
        }
    }

    /// Loads each scalar independently; no cross-field consistency while
    /// workers are active.
    pub fn snapshot(&self) -> MetricSnapshot {
        MetricSnapshot {
            schema: 1,
            counts: std::array::from_fn(|i| self.counts[i].load(Relaxed)),
            saturated: self.saturated.load(Relaxed),
        }
    }
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricSnapshot {
    pub fn value(&self, metric: Metric) -> u64 {
        self.counts[metric as usize]
    }

    pub fn checked_delta(&self, before: &Self) -> Result<Self, DeltaError> {
        if self.schema != before.schema {
            return Err(DeltaError::SchemaMismatch);
        }
        if self.saturated || before.saturated {
            return Err(DeltaError::Saturated);
        }
        let mut counts = [0; METRIC_COUNT];
        for (i, slot) in counts.iter_mut().enumerate() {
            *slot = self.counts[i]
                .checked_sub(before.counts[i])
                .ok_or(DeltaError::Regressed)?;
        }
        Ok(Self {
            schema: self.schema,
            counts,
            saturated: false,
        })
    }
}

pub static PROCESS_COUNTERS: Counters = Counters::new();

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<std::sync::Arc<Counters>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn record(metric: Metric, amount: u64) {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(scoped) = OVERRIDE.with(|o| o.borrow().clone()) {
        scoped.record(metric, amount);
        return;
    }
    PROCESS_COUNTERS.record(metric, amount);
}

pub fn enabled() -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(scoped) = OVERRIDE.with(|o| o.borrow().as_ref().map(|c| c.enabled())) {
        return scoped;
    }
    PROCESS_COUNTERS.enabled()
}

/// Current-thread override only; restores the previous override on drop and
/// never resets global counts.
#[cfg(any(test, feature = "test-support"))]
pub struct TestScope {
    collector: std::sync::Arc<Counters>,
    previous: Option<std::sync::Arc<Counters>>,
    not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestScope {
    pub fn new() -> Self {
        let collector = std::sync::Arc::new(Counters::new());
        collector.set_enabled(true);
        let previous = OVERRIDE.with(|o| o.borrow_mut().replace(collector.clone()));
        Self {
            collector,
            previous,
            not_send: std::marker::PhantomData,
        }
    }

    pub fn snapshot(&self) -> MetricSnapshot {
        self.collector.snapshot()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for TestScope {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        OVERRIDE.with(|o| *o.borrow_mut() = previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_counters_do_not_record() {
        let c = Counters::new();
        c.record(Metric::CoreParserInvocations, 1);
        assert_eq!(c.snapshot().value(Metric::CoreParserInvocations), 0);
    }

    #[test]
    fn saturation_invalidates_delta() {
        let c = Counters::new();
        c.set_enabled(true);
        let before = c.snapshot();
        c.record(Metric::LegacyHashBytes, u64::MAX);
        c.record(Metric::LegacyHashBytes, 1);
        assert_eq!(
            c.snapshot().checked_delta(&before),
            Err(DeltaError::Saturated)
        );
    }

    #[test]
    fn joined_workers_have_exact_aggregate() {
        let c = std::sync::Arc::new(Counters::new());
        c.set_enabled(true);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let c = std::sync::Arc::clone(&c);
                scope.spawn(move || {
                    for _ in 0..100 {
                        c.record(Metric::CoreParserInvocations, 1);
                    }
                });
            }
        });
        assert_eq!(c.snapshot().value(Metric::CoreParserInvocations), 200);
    }

    #[test]
    fn delta_rejects_schema_change() {
        let c = Counters::new();
        let after = c.snapshot();
        let mut before = c.snapshot();
        before.schema = 2;
        assert_eq!(
            after.checked_delta(&before),
            Err(DeltaError::SchemaMismatch)
        );
    }

    #[test]
    fn delta_rejects_regression() {
        let c = Counters::new();
        let after = c.snapshot();
        let mut before = c.snapshot();
        before.counts[Metric::ImportBindingCalls as usize] = 5;
        assert_eq!(after.checked_delta(&before), Err(DeltaError::Regressed));
    }

    #[test]
    fn delta_subtracts_each_metric() {
        let c = Counters::new();
        c.set_enabled(true);
        c.record(Metric::ImportBindingPairs, 3);
        let before = c.snapshot();
        c.record(Metric::ImportBindingPairs, 4);
        c.record(Metric::ProjectStampProbes, 1);
        let delta = c.snapshot().checked_delta(&before).unwrap();
        assert_eq!(delta.value(Metric::ImportBindingPairs), 4);
        assert_eq!(delta.value(Metric::ProjectStampProbes), 1);
        assert_eq!(delta.value(Metric::LegacyHashBytes), 0);
    }

    #[test]
    fn disable_reenable_never_resets_counts() {
        let c = Counters::new();
        c.set_enabled(true);
        c.record(Metric::LspDocumentPreparations, 2);
        c.set_enabled(false);
        c.record(Metric::LspDocumentPreparations, 5);
        c.set_enabled(true);
        c.record(Metric::LspDocumentPreparations, 1);
        assert_eq!(c.snapshot().value(Metric::LspDocumentPreparations), 3);
    }

    #[test]
    fn nested_test_scope_restores_parent() {
        let outer = TestScope::new();
        record(Metric::ResolverMetadataParses, 1);
        {
            let inner = TestScope::new();
            record(Metric::ResolverMetadataParses, 10);
            assert_eq!(inner.snapshot().value(Metric::ResolverMetadataParses), 10);
        }
        record(Metric::ResolverMetadataParses, 100);
        assert!(enabled());
        assert_eq!(outer.snapshot().value(Metric::ResolverMetadataParses), 101);
    }
}
