//! Root-local ownership credits, independent of the borrowed cancellation token.
use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, IncompleteReason};
use pascal_core::{CancellationToken, NoCancellation};
use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Cancel(Cell<bool>);
impl CancellationToken for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.get()
    }
}

#[test]
fn task9_b_owned_provider_drop_keeps_cache_credit_and_cumulative_work() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let cache = budget.admit_owned(3, 20, 100, || "cache").unwrap();
    let after_cache = budget.snapshot();
    let returned = budget
        .admit_owned(4, 30, 200, || "returned provider")
        .unwrap();
    let both = budget.snapshot();
    assert!(both.retained_bytes > after_cache.retained_bytes + 200);
    drop(returned);
    assert_eq!(budget.snapshot().retained_bytes, after_cache.retained_bytes);
    assert_eq!(budget.snapshot().work, both.work);
    assert_eq!(budget.snapshot().byte_work, both.byte_work);
    drop(cache);
    let ledger_only = budget.snapshot().retained_bytes;
    assert!(
        ledger_only > 0,
        "the live root ledger is itself admitted storage"
    );
    let other = budget.admit_owned(0, 0, 100, || "other").unwrap();
    drop(other);
    assert_eq!(budget.snapshot().retained_bytes, ledger_only);
}

#[test]
fn task9_b_shared_graph_snapshot_admits_handle_not_backing_and_refunds_last_owner() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let graph = budget
        .admit_owned(1, 50_000, 50_000, || vec![0u8; 50_000])
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let one = budget.snapshot();
    let clone = graph.try_clone(&mut budget).unwrap();
    let two = budget.snapshot();
    assert!(
        two.retained_bytes - one.retained_bytes < 256,
        "not another graph charge"
    );
    assert!(std::ptr::eq(graph.as_slice(), clone.as_slice()));
    drop(clone);
    assert_eq!(budget.snapshot().retained_bytes, one.retained_bytes);
    let clone = graph.try_clone(&mut budget).unwrap();
    drop(graph);
    assert_eq!(budget.snapshot().retained_bytes, one.retained_bytes);
    assert_eq!(clone.len(), 50_000);
    drop(clone);
    assert!(
        budget.snapshot().retained_bytes < 256,
        "only the ledger remains"
    );
}

#[test]
fn task9_b_receipt_outlives_budget_and_cancel_and_drops_payload_before_refund() {
    struct Payload(Arc<AtomicBool>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let value = {
        let cancel = Cancel(Cell::new(false));
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &cancel);
        let value = budget
            .admit_owned(1, 0, 100, || Payload(dropped.clone()))
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        cancel.0.set(true);
        value
    };
    assert!(!dropped.load(Ordering::SeqCst));
    std::thread::spawn(move || drop(value)).join().unwrap();
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn task9_b_failed_and_cancelled_admission_never_calls_constructor_or_changes_credits() {
    let cancel = Cancel(Cell::new(false));
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 1024,
            ..Default::default()
        },
        &cancel,
    );
    let cache = budget.admit_owned(1, 0, 100, || 1).unwrap();
    let before = budget.snapshot();
    let called = Cell::new(false);
    let failure = budget.admit_owned(5, 20, 10_000, || {
        called.set(true);
        2
    });
    assert!(matches!(
        failure,
        Err(IncompleteReason::Limit {
            name: "max_retained_bytes",
            ..
        })
    ));
    assert!(!called.get());
    assert_eq!(budget.snapshot(), before);
    cancel.0.set(true);
    assert!(matches!(
        budget.admit_owned(1, 0, 1, || {
            called.set(true);
            2
        }),
        Err(IncompleteReason::Cancelled)
    ));
    assert_eq!(budget.snapshot(), before);
    assert!(!called.get());
    drop(cache);
    assert!(budget.snapshot().retained_bytes < before.retained_bytes);
}

#[test]
fn task9_b_legacy_release_cannot_forge_refunds_of_live_receipts_or_ledger() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let value = budget.admit_owned(1, 0, 200, || "live").unwrap();
    let owned = budget.snapshot();
    budget.charge(1, 0, 100).unwrap();
    budget.release(usize::MAX);
    assert_eq!(budget.snapshot().retained_bytes, owned.retained_bytes);
    assert_eq!(*value, "live");
    drop(value);
    assert!(budget.snapshot().retained_bytes < owned.retained_bytes);
}

#[test]
fn task9_b_wrong_root_cannot_clone_or_transfer_graph_credit() {
    let mut root = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let graph = root
        .admit_owned(1, 0, 100, || vec![1, 2, 3])
        .unwrap()
        .into_shared(&mut root)
        .unwrap();
    let before = root.snapshot();
    assert!(matches!(
        graph.try_clone(&mut other),
        Err(IncompleteReason::MalformedSource)
    ));
    assert_eq!(root.snapshot(), before);
    assert_eq!(other.snapshot().retained_bytes, 0);
    assert_eq!(graph.as_slice(), &[1, 2, 3]);
}

#[test]
fn task9_b_error_cleanup_and_graph_replacement_refund_only_actual_owners() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let cache = budget.admit_owned(1, 0, 100, || 0).unwrap();
    let cache_usage = budget.snapshot();
    let error: Result<(), &'static str> = (|| {
        let _scratch = budget.admit_owned(1, 0, 300, || vec![0; 300]).unwrap();
        Err("lookup failed")
    })();
    assert!(error.is_err());
    assert_eq!(budget.snapshot().retained_bytes, cache_usage.retained_bytes);
    let old = budget
        .admit_owned(1, 0, 500, || vec![0; 500])
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let snapshot = old.try_clone(&mut budget).unwrap();
    let new = budget
        .admit_owned(1, 0, 600, || vec![1; 600])
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    drop(old);
    let with_snapshot = budget.snapshot();
    assert_eq!(snapshot[0], 0);
    drop(snapshot);
    assert!(budget.snapshot().retained_bytes + 500 < with_snapshot.retained_bytes);
    drop(new);
    assert_eq!(budget.snapshot().retained_bytes, cache_usage.retained_bytes);
    drop(cache);
}

#[test]
fn task9_b_constructor_unwind_refunds_its_receipt_but_not_prior_cache_or_work() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let cache = budget.admit_owned(1, 0, 100, || "cache").unwrap();
    let before = budget.snapshot();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = budget.admit_owned::<()>(5, 60, 1000, || panic!("constructor failed"));
    }));
    assert!(failed.is_err());
    assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
    assert_eq!(budget.snapshot().work, before.work + 5);
    assert_eq!(budget.snapshot().byte_work, before.byte_work + 60);
    drop(cache);
}

#[test]
fn task9_b_work_and_byte_caps_are_transactional_before_first_ledger_allocation() {
    for (limits, expected) in [
        (
            CompileTimeLimits {
                max_work: 0,
                ..Default::default()
            },
            "max_work",
        ),
        (
            CompileTimeLimits {
                max_byte_work: 0,
                ..Default::default()
            },
            "max_byte_work",
        ),
    ] {
        let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
        let before = budget.snapshot();
        let called = Cell::new(false);
        assert!(
            matches!(budget.admit_owned(1, 1, 1, || { called.set(true); () }), Err(IncompleteReason::Limit { name, .. }) if name == expected)
        );
        assert_eq!(budget.snapshot(), before);
        assert!(!called.get());
    }
}

#[test]
fn task9_b_shared_handle_refusal_preserves_backing_and_prior_handles() {
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 1024,
            ..Default::default()
        },
        &NoCancellation,
    );
    let graph = budget
        .admit_owned(1, 0, 700, || vec![0; 700])
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let mut handles = Vec::new(); // test coordinator storage is not production storage
    loop {
        let before = budget.snapshot();
        match graph.try_clone(&mut budget) {
            Ok(handle) => handles.push(handle),
            Err(IncompleteReason::Limit {
                name: "max_retained_bytes",
                ..
            }) => {
                assert_eq!(budget.snapshot(), before);
                break;
            }
            other => panic!("unexpected clone result: {other:?}"),
        }
    }
    assert!(handles.len() > 1);
    let full = budget.snapshot();
    drop(handles);
    assert!(budget.snapshot().retained_bytes < full.retained_bytes);
    assert_eq!(graph.len(), 700);
    drop(graph);
    assert!(budget.snapshot().retained_bytes < 256);
}

#[test]
fn task9_b_cancelled_owned_to_shared_drops_only_the_consumed_owner() {
    let cancel = Cancel(Cell::new(false));
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &cancel);
    let cache = budget.admit_owned(1, 0, 100, || 1).unwrap();
    let before = budget.snapshot();
    let value = budget.admit_owned(1, 0, 100, || 2).unwrap();
    cancel.0.set(true);
    assert!(matches!(
        value.into_shared(&mut budget),
        Err(IncompleteReason::Cancelled)
    ));
    assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
    assert_eq!(budget.snapshot().work, before.work + 1);
    assert_eq!(*cache, 1);
    drop(cache);
}

#[test]
fn task9_b_cross_thread_last_owner_drop_cannot_lose_root_charges() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let graph = budget
        .admit_owned(1, 0, 1000, || vec![1; 1000])
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let initial = budget.snapshot();
    let handles = (0..1000)
        .map(|_| graph.try_clone(&mut budget).unwrap())
        .collect::<Vec<_>>();
    let before_work = budget.snapshot();
    let thread = std::thread::spawn(move || {
        for handle in handles {
            drop(handle);
            std::thread::yield_now();
        }
    });
    for _ in 0..1000 {
        budget.charge(1, 1, 1).unwrap();
        std::thread::yield_now();
    }
    thread.join().unwrap();
    assert_eq!(
        budget.snapshot().retained_bytes,
        initial.retained_bytes + 1000
    );
    assert_eq!(budget.snapshot().work, before_work.work + 1000);
    assert_eq!(budget.snapshot().byte_work, before_work.byte_work + 1000);
    drop(graph);
    budget.release(1000);
    assert!(budget.snapshot().retained_bytes < 256);
}

#[test]
fn task9_b_capability_validation_and_live_snapshot_survive_budget_output_transfer() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let value = budget.admit_owned(1, 0, 100, || "payload").unwrap();
    assert!(value.is_admitted_by(&budget));
    assert!(!value.is_admitted_by(&other));
    let shared = value.into_shared(&mut budget).unwrap();
    assert!(shared.is_admitted_by(&budget));
    assert!(!shared.is_admitted_by(&other));
    let before = budget.snapshot();
    assert_eq!(shared.budget_snapshot(), before);
    drop(budget);
    assert_eq!(shared.budget_snapshot(), before);
    assert_eq!(*shared, "payload");
}

#[test]
fn task9_b_payload_destructor_observes_its_credit_until_payload_is_dead() {
    use pascal_core::compile_time::AdmittedShared;
    use std::sync::Mutex;
    struct Payload {
        inspector: AdmittedShared<()>,
        seen: Arc<Mutex<Option<usize>>>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            *self.seen.lock().unwrap() = Some(self.inspector.budget_snapshot().retained_bytes);
        }
    }
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let inspector = budget
        .admit_owned(1, 0, 0, || ())
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let inspected = inspector.try_clone(&mut budget).unwrap();
    let seen = Arc::new(Mutex::new(None));
    let payload = budget
        .admit_owned(1, 0, 1000, || Payload {
            inspector: inspected,
            seen: seen.clone(),
        })
        .unwrap();
    let live = budget.snapshot().retained_bytes;
    drop(payload);
    assert_eq!(
        *seen.lock().unwrap(),
        Some(live),
        "payload must die before any of its credits refund"
    );
    assert!(budget.snapshot().retained_bytes + 1000 < live);
    drop(inspector);
}
