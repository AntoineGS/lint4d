use super::*;
use crate::project_cache::ProjectCache;
use crate::workspace::persist::tests::private_dir;
use pascal_cache::admin::{self, StoreStatus};
use std::sync::mpsc;

const SETTLE: Duration = Duration::from_secs(10);

fn namespace() -> Namespace {
    Namespace::new(pascal_cache::Digest::from_bytes([0x5c; 32]))
}

fn options(directory: &std::path::Path) -> CacheOptions {
    CacheOptions {
        directory: Some(directory.to_path_buf()),
        ..CacheOptions::default()
    }
}

/// Unit tests never schedule maintenance (Task 6 honours the switch).
fn start(project_cache: &ProjectCache, options: CacheOptions) -> PersistentCache {
    PersistentCache::start_unscheduled_for_test(
        options,
        project_cache.clone(),
        Arc::default(),
        namespace(),
    )
}

fn live_leases(directory: &std::path::Path) -> u64 {
    match admin::status(directory) {
        StoreStatus::Present(report) => report.live_leases,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_inert_handle_has_no_job_and_reports_disabled() {
    let cache = PersistentCache::default();
    assert!(cache.job().is_none());
    assert_eq!(cache.state(), CacheState::Disabled);
    assert_eq!(cache.status().to_json()["state"], "disabled");
    cache.shutdown();
}

#[test]
fn a_disabled_cache_creates_nothing_and_starts_no_thread() {
    let root = private_dir();
    let directory = root.path().join("cache");
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(
        &project_cache,
        CacheOptions {
            enabled: false,
            ..options(&directory)
        },
    );
    assert_eq!(cache.state(), CacheState::Disabled);
    assert!(cache.job().is_none());
    assert!(!cache.has_worker_for_test());
    assert!(!directory.exists());
    cache.shutdown();
}

#[test]
fn invalid_options_bypass_with_their_message() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(
        &project_cache,
        CacheOptions {
            invalid: Some("bad".into()),
            ..options(root.path())
        },
    );
    assert_eq!(
        cache.state(),
        CacheState::Bypassed(StatusCause::Configuration("bad".into()))
    );
    assert!(!cache.has_worker_for_test());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    // R12: reconfiguration and re-admission leave an invalid session alone.
    assert_eq!(
        cache.reconfigure_within(options(root.path()), SETTLE),
        Ack::Done
    );
    assert_eq!(cache.readmit(), Ack::Done);
    assert_eq!(
        cache.state(),
        CacheState::Bypassed(StatusCause::Configuration("bad".into()))
    );
    assert!(!cache.has_worker_for_test());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn an_enabled_cache_opens_one_admitted_store_off_the_calling_thread() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let job = cache.job().expect("enabled");
    job.store().check_scope().unwrap();
    assert_eq!(
        project_cache.stats().reserved_bytes() as u64,
        StoreLimits::default().admission_bytes()
    );
    assert_eq!(live_leases(root.path()), 1);
    let status = cache.status().to_json();
    assert_eq!(status["state"], "enabled");
    assert_eq!(status["resetEpoch"], 0);
    assert_eq!(status["directory"], root.path().to_str().unwrap());
    drop(job);
    cache.shutdown();
    assert_eq!(live_leases(root.path()), 0, "closing deletes the lease");
    assert_eq!(project_cache.stats().reserved_bytes(), 0);
    assert!(cache.job().is_none());
    assert_eq!(
        cache.status().to_json()["resetEpoch"],
        serde_json::Value::Null
    );
    // Idempotent, and nothing reopens after shutdown.
    cache.shutdown();
    cache.reconfigure(options(root.path()));
    assert_eq!(cache.state(), CacheState::Disabled);
}

#[test]
fn disable_without_jobs_is_acknowledged_at_once() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let generation = cache.generation();
    assert_eq!(
        cache.reconfigure_within(
            CacheOptions {
                enabled: false,
                ..options(root.path())
            },
            Duration::ZERO
        ),
        Ack::Done
    );
    assert_eq!(cache.state(), CacheState::Disabled);
    assert!(cache.generation() > generation);
    assert_eq!(live_leases(root.path()), 0);
    assert_eq!(project_cache.stats().reserved_bytes(), 0);
}

/// Spec §3 (D22): fence, cancel, wait for admitted jobs, then acknowledge.
#[test]
fn disable_waits_for_admitted_jobs_then_closes_the_store() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let job = cache.job().unwrap();
    let disabled = CacheOptions {
        enabled: false,
        ..options(root.path())
    };
    assert_eq!(
        cache.reconfigure_within(disabled.clone(), Duration::ZERO),
        Ack::Pending
    );
    assert!(matches!(cache.state(), CacheState::Fencing));
    assert_eq!(cache.status().to_json()["state"], "disabling");
    assert!(cache.job().is_none(), "no new job while fencing");
    assert!(job.is_cancelled(), "admitted work is told to stop");
    assert_eq!(
        live_leases(root.path()),
        1,
        "the admitted job still holds the store"
    );
    drop(job);
    assert_eq!(
        cache.state(),
        CacheState::Disabled,
        "the last job acknowledges"
    );
    assert_eq!(live_leases(root.path()), 0);
    assert_eq!(project_cache.stats().reserved_bytes(), 0);

    // A blocking disable returns once a job on another thread drops.
    cache.reconfigure(options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let job = cache.job().unwrap();
    let (sender, receiver) = mpsc::channel();
    let waiter = {
        let cache = cache.clone();
        std::thread::spawn(move || {
            let ack = cache.reconfigure_within(disabled, SETTLE);
            sender.send(()).unwrap();
            ack
        })
    };
    assert!(cache.wait_for(|state| matches!(state, CacheState::Fencing), SETTLE));
    assert!(receiver.try_recv().is_err(), "still waiting for the job");
    drop(job);
    assert_eq!(waiter.join().unwrap(), Ack::Done);
    assert_eq!(cache.state(), CacheState::Disabled);
}

/// An in-flight open is an admitted operation: disabling during it waits
/// for the open and closes its store before acknowledging.
#[test]
fn a_disable_during_opening_waits_for_the_open_and_closes_its_store() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let (entered, release) = (mpsc::channel(), mpsc::channel::<()>());
    let cache = PersistentCache::start_paused_for_test(
        options(root.path()),
        project_cache.clone(),
        Arc::default(),
        namespace(),
        entered.0,
        release.1,
    );
    entered.1.recv().unwrap(); // the open holds its admitted count and waits
    assert_eq!(
        cache.reconfigure_within(
            CacheOptions {
                enabled: false,
                ..options(root.path())
            },
            Duration::ZERO
        ),
        Ack::Pending
    );
    release.0.send(()).unwrap();
    assert_eq!(cache.settle(SETTLE), CacheState::Disabled);
    assert!(cache.job().is_none());
    assert_eq!(
        live_leases(root.path()),
        0,
        "the store opened late was closed before the acknowledgement"
    );
    assert_eq!(project_cache.stats().reserved_bytes(), 0);
}

#[test]
fn transitions_leave_project_cache_entries_untouched() {
    use crate::project_cache::{ImportValue, Lookup};
    use lsp_types::Url;
    use pascal_project::ProjectContext;
    use std::collections::HashMap;

    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let importer = Url::from_file_path("/ws/Main.pas").unwrap();
    let no_cancel = AtomicBool::new(false);
    let Lookup::Compute(claim) = project_cache.imports(
        &importer,
        &ProjectContext::default(),
        1,
        &HashMap::new(),
        &no_cancel,
    ) else {
        panic!("an empty cache misses");
    };
    let value = ImportValue {
        resolved: Arc::new(pascal_core::ResolvedImports {
            bindings: vec![],
            dependencies: vec![],
            complete: true,
            settled: true,
        }),
        report: Arc::new(pascal_core::ResolutionReport {
            observations: vec![],
            warnings: vec![],
            complete: true,
            incomplete_reasons: vec![],
        }),
        probes: vec![],
        watch_dirs: vec![],
    };
    project_cache.store_imports(claim, value, 100, &no_cancel);
    let entries = || {
        let stats = project_cache.stats();
        (
            project_cache.ready_layers(&importer),
            stats.units,
            stats.imports,
        )
    };
    let before = entries();
    assert_eq!(before.0, ["Import"]);

    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(entries(), before, "enable");
    let disabled = CacheOptions {
        enabled: false,
        ..options(root.path())
    };
    assert_eq!(cache.reconfigure(disabled), Ack::Done);
    assert_eq!(cache.state(), CacheState::Disabled);
    assert_eq!(entries(), before, "disable");
    cache.reconfigure(options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(entries(), before, "re-enable");
    cache.reconfigure(CacheOptions {
        scratch_bytes: 2 << 20,
        ..options(root.path())
    });
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(entries(), before, "budget change");
    cache.shutdown();
    assert_eq!(entries(), before, "shutdown");
}

#[test]
fn a_budget_change_reopens_under_new_limits() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let generation = cache.generation();
    let smaller = CacheOptions {
        scratch_bytes: 2 << 20,
        ..options(root.path())
    };
    cache.reconfigure(smaller.clone());
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert!(cache.generation() > generation);
    let job = cache.job().unwrap();
    assert_eq!(job.store().limits(), smaller.limits());
    assert_eq!(project_cache.stats().reserved_scratch_bytes, 2 << 20);
    assert_eq!(live_leases(root.path()), 1, "the old store was closed");
    drop(job);
    let generation = cache.generation();
    cache.reconfigure(smaller);
    assert_eq!(
        cache.generation(),
        generation,
        "unchanged options do nothing"
    );
}

#[test]
fn readmission_bypasses_a_store_that_no_longer_fits() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    project_cache.set_max_bytes(1 << 20);
    cache.readmit();
    assert_eq!(
        cache.settle(SETTLE),
        CacheState::Bypassed(StatusCause::Memory)
    );
    assert_eq!(project_cache.stats().reserved_bytes(), 0);
    assert_eq!(live_leases(root.path()), 0);
    // R11: re-admission retries a memory bypass once the budget allows it.
    project_cache.set_max_bytes(64 << 20);
    cache.readmit();
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(live_leases(root.path()), 1);
}

#[test]
fn a_location_failure_is_bypassed_and_the_directory_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let root = private_dir();
    std::fs::set_permissions(root.path(), PermissionsExt::from_mode(0o755)).unwrap();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert!(matches!(
        cache.settle(SETTLE),
        CacheState::Bypassed(StatusCause::Store(Bypass::Location(
            pascal_cache::LocationIssue::NotPrivate { .. }
        )))
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    assert_eq!(
        std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

/// D24: a reset fences the store; the handle reopens at the next epoch.
#[test]
fn a_reset_fences_the_store_and_the_handle_reopens_the_next_epoch() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(
        admin::reset(root.path(), &namespace(), StoreLimits::default()),
        Ok(true)
    );
    let job = cache.job().unwrap();
    let fenced = job.store().check_scope();
    assert_eq!(fenced, Err(Bypass::Fenced));
    job.observe(&Bypass::Fenced);
    assert!(cache.job().is_none());
    assert_eq!(cache.status().to_json()["state"], "reopening");
    drop(job);
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(cache.job().unwrap().store().scope().reset_epoch, 1);
    assert_eq!(
        cache.counters().unwrap().snapshot().bypasses[names::bypass_index(&Bypass::Fenced)],
        1
    );
}

#[test]
fn retry_delays_double_up_to_the_health_backoff() {
    assert_eq!(worker::retry_delay(None), pascal_cache::MIN_HEALTH_BACKOFF);
    let mut delay = None;
    for _ in 0..20 {
        delay = Some(worker::retry_delay(delay));
    }
    assert_eq!(delay, Some(pascal_cache::MAX_HEALTH_BACKOFF));
    assert!(worker::retries(&Bypass::NamespaceLockBusy));
    assert!(!worker::retries(&Bypass::Corrupt));
}

/// D24: an open refused with `Retry` advice is retried by the worker's
/// timer (after `MIN_HEALTH_BACKOFF`), with no command sent.
#[test]
fn an_open_refused_for_now_is_retried_after_the_backoff() {
    let root = private_dir();
    let holder = Arc::new(Store::open(root.path(), &namespace(), StoreLimits::default()).unwrap());
    // The holder's reclaim stops inside its write transaction.
    let (entered, release) = (mpsc::channel(), mpsc::channel::<()>());
    // One-shot: a later commit on the holder passes straight through.
    let hook = Mutex::new(Some((entered.0, release.1)));
    holder.set_before_commit_for_test(move || {
        if let Some((entered, release)) = lock(&hook).take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
    });
    let reclaim = {
        let holder = Arc::clone(&holder);
        std::thread::spawn(move || holder.reclaim(1).map(|_| ()))
    };
    entered.1.recv_timeout(SETTLE).unwrap();

    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    let bypass = match cache.settle(SETTLE) {
        CacheState::Bypassed(StatusCause::Store(bypass)) => bypass,
        other => panic!("{other:?}"),
    };
    assert!(worker::retries(&bypass), "{bypass:?}");
    assert_eq!(project_cache.stats().reserved_bytes(), 0);
    assert_eq!(
        cache.counters().unwrap().snapshot().bypasses[names::bypass_index(&bypass)],
        1
    );

    release.0.send(()).unwrap();
    reclaim.join().unwrap().unwrap();
    assert!(cache.wait_for(|state| *state == CacheState::Enabled, SETTLE));
    drop(holder);
    assert_eq!(live_leases(root.path()), 1);
}

/// A handle that is `Opening` with the open not yet claimed by the worker
/// (no worker, nothing admitted): the window between `request_open` and
/// the worker's first lock.
fn unclaimed_opening(project_cache: &ProjectCache, options: CacheOptions) -> PersistentCache {
    let cache = PersistentCache {
        inner: Some(Arc::new(Inner::new(
            options,
            project_cache.clone(),
            Arc::default(),
            namespace(),
            false,
        ))),
    };
    assert_eq!(cache.state(), CacheState::Opening);
    assert!(!cache.has_worker_for_test());
    cache
}

/// A fence that holds nothing (no job, no store, no open claimed yet)
/// finishes at once.
#[test]
fn a_disable_before_the_open_is_claimed_is_acknowledged_at_once() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = unclaimed_opening(&project_cache, options(root.path()));
    let generation = cache.generation();
    assert_eq!(
        cache.reconfigure_within(
            CacheOptions {
                enabled: false,
                ..options(root.path())
            },
            Duration::ZERO
        ),
        Ack::Done
    );
    assert_eq!(cache.state(), CacheState::Disabled);
    assert!(cache.generation() > generation);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn a_shutdown_before_the_open_is_claimed_finishes() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = unclaimed_opening(&project_cache, options(root.path()));
    cache.shutdown();
    assert_eq!(cache.state(), CacheState::Disabled);
    assert!(!cache.has_worker_for_test());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn a_budget_change_before_the_open_is_claimed_reopens() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = unclaimed_opening(&project_cache, options(root.path()));
    let smaller = CacheOptions {
        scratch_bytes: 2 << 20,
        ..options(root.path())
    };
    assert_eq!(
        cache.reconfigure_within(smaller.clone(), Duration::ZERO),
        Ack::Done
    );
    assert_ne!(cache.state(), CacheState::Fencing);
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(cache.job().unwrap().store().limits(), smaller.limits());
}

#[test]
fn a_readmission_before_the_open_is_claimed_reopens() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = unclaimed_opening(&project_cache, options(root.path()));
    assert_eq!(cache.readmit(), Ack::Done);
    assert_ne!(cache.state(), CacheState::Fencing);
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
}

fn report(
    cycle_complete: bool,
    pressure: pascal_cache::Pressure,
    skipped: bool,
) -> pascal_cache::StepReport {
    pascal_cache::StepReport {
        skipped,
        cancelled: false,
        phase: pascal_cache::Phase::Done,
        rows: 0,
        cycle_complete,
        pressure,
    }
}

/// The lease must be renewed at least every LEASE_EXPIRY - LEASE_RENEWAL
/// (9 minutes, `maintenance.rs:138-141`); every step renews when due.
#[test]
fn maintenance_steps_often_enough_to_keep_the_lease() {
    use pascal_cache::Pressure::{self, OverHeadroom, WalPressure};
    use worker::{IDLE_INTERVAL, PRESSURE_INTERVAL, RENEWAL_DEADLINE, next_delay};
    assert_eq!(
        next_delay(Option::None),
        Duration::ZERO,
        "the first step follows the open"
    );
    assert_eq!(
        next_delay(Some(&Ok(report(false, Pressure::None, false)))),
        Duration::ZERO
    );
    assert_eq!(
        next_delay(Some(&Ok(report(true, Pressure::None, false)))),
        IDLE_INTERVAL
    );
    assert_eq!(
        next_delay(Some(&Ok(report(true, OverHeadroom, false)))),
        PRESSURE_INTERVAL
    );
    assert_eq!(
        next_delay(Some(&Ok(report(false, WalPressure, false)))),
        PRESSURE_INTERVAL
    );
    assert_eq!(
        next_delay(Some(&Ok(report(false, Pressure::None, true)))),
        IDLE_INTERVAL
    );
    assert_eq!(next_delay(Some(&Err(Bypass::Busy))), IDLE_INTERVAL);
    let renewal = pascal_cache::LEASE_EXPIRY - pascal_cache::LEASE_RENEWAL;
    assert!(IDLE_INTERVAL <= renewal);
    assert!(RENEWAL_DEADLINE <= renewal);
}

/// D23: a step waits for the gate unless the lease renewal is overdue.
#[test]
fn an_overdue_renewal_steps_past_a_busy_gate() {
    use worker::{RENEWAL_DEADLINE, should_step};
    assert!(should_step(true, Duration::ZERO));
    assert!(!should_step(false, Duration::ZERO));
    assert!(!should_step(
        false,
        RENEWAL_DEADLINE - Duration::from_millis(1)
    ));
    assert!(should_step(false, RENEWAL_DEADLINE));
    assert!(should_step(false, RENEWAL_DEADLINE * 2));
}

#[test]
fn a_step_runs_under_a_job_and_records_usage() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(cache.status().to_json()["store"], serde_json::Value::Null);
    let report = cache.step_now().expect("enabled").unwrap();
    assert!(report.rows <= worker::STEP_BUDGET.rows);
    assert!(cache.counters().unwrap().snapshot().steps >= 1);
    let status = cache.status().to_json();
    assert!(
        status["store"]["databaseBytes"].as_u64().unwrap() > 0,
        "{status}"
    );
    // The step's job is gone: a disable is acknowledged at once.
    assert_eq!(
        cache.reconfigure_within(
            CacheOptions {
                enabled: false,
                ..options(root.path())
            },
            Duration::ZERO
        ),
        Ack::Done
    );
}

#[test]
fn a_disabled_cache_never_steps() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    cache.reconfigure(CacheOptions {
        enabled: false,
        ..options(root.path())
    });
    assert_eq!(cache.step_now(), None);
    assert_eq!(cache.status().to_json()["store"], serde_json::Value::Null);
    assert_eq!(cache.counters().unwrap().snapshot().steps, 0);
    assert_eq!(PersistentCache::default().step_now(), None);
}

/// R4: the scheduled path never waits at a busy gate; it defers without
/// holding a job, so a disable meanwhile is acknowledged at once.
#[test]
fn a_busy_gate_defers_the_scheduled_step_and_a_disable_meanwhile_is_acknowledged() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let gate = Arc::new(InteractiveGate::default());
    let cache = PersistentCache::start_unscheduled_for_test(
        options(root.path()),
        project_cache.clone(),
        gate.clone(),
        namespace(),
    );
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let steps = cache.counters().unwrap().snapshot().steps;
    gate.set(1);
    assert_eq!(cache.scheduled_step_for_test(), None);
    assert_eq!(cache.counters().unwrap().snapshot().steps, steps);
    assert_eq!(
        cache.reconfigure_within(
            CacheOptions {
                enabled: false,
                ..options(root.path())
            },
            Duration::ZERO
        ),
        Ack::Done
    );
    assert_eq!(cache.counters().unwrap().snapshot().steps, steps);
    // An idle gate lets the scheduled path step once enabled again.
    gate.set(0);
    cache.reconfigure(options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert!(matches!(cache.scheduled_step_for_test(), Some(Ok(_))));
    assert_eq!(cache.counters().unwrap().snapshot().steps, steps + 1);
}

/// R2: a production handle steps on its own once enabled (the first step
/// follows the open) and records the usage it read.
#[test]
fn a_scheduled_cache_steps_after_the_open() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = PersistentCache::start_with(
        options(root.path()),
        project_cache.clone(),
        Arc::default(),
        namespace(),
    );
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let inner = cache.inner.as_ref().unwrap();
    let shared = inner.wait_while(inner.lock(), SETTLE, |shared| shared.usage.is_none());
    assert!(shared.usage.is_some(), "no step recorded its usage");
    drop(shared);
    assert!(cache.counters().unwrap().snapshot().steps >= 1);
    cache.shutdown();
    assert_eq!(live_leases(root.path()), 0);
}

/// I1: a zero wait still tries each lock once, so closing deletes the lease.
#[test]
fn a_zero_wait_cache_enables_and_closing_deletes_its_lease() {
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(
        &project_cache,
        CacheOptions {
            wait: Duration::ZERO,
            ..options(root.path())
        },
    );
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(live_leases(root.path()), 1);
    cache.shutdown();
    assert_eq!(live_leases(root.path()), 0);
}

/// M2: a budget that fits exactly one store still reopens it, so the old
/// reservation is released before the new one is taken.
#[test]
fn a_reopen_within_a_budget_of_exactly_one_store_releases_before_it_reserves() {
    let root = private_dir();
    let budget = StoreLimits::default().admission_bytes() as usize;
    let project_cache = ProjectCache::new(budget);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let generation = cache.generation();
    cache.readmit();
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert!(cache.generation() > generation);
    let generation = cache.generation();
    cache.reconfigure(CacheOptions {
        wait: Duration::from_millis(5),
        ..options(root.path())
    });
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert!(cache.generation() > generation);
    assert_eq!(project_cache.stats().reserved_bytes(), budget);
    assert_eq!(live_leases(root.path()), 1);
    cache.shutdown();
}

/// I2: one reuse outcome is counted once, and a fenced reuse reopens.
#[test]
fn record_reuse_counts_each_outcome_once_and_a_fenced_reuse_reopens() {
    use crate::project_cache::ReservationKind;
    use crate::workspace::persist::{AdmittedFacts, PersistError, Reuse, ReuseMiss, ReuseVia};
    use counters::MissCause;
    let root = private_dir();
    let project_cache = ProjectCache::new(64 << 20);
    let cache = start(&project_cache, options(root.path()));
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    let job = cache.job().unwrap();
    let charge = project_cache.reserve(ReservationKind::Facts, 64).unwrap();
    job.record_reuse(&Ok(Reuse::Hit {
        facts: AdmittedFacts {
            facts: Arc::new(pascal_core::facts::ArtifactFacts::default()),
            charge,
        },
        via: ReuseVia::Local,
        reads: 0,
        artifact: pascal_cache::Digest::from_bytes([0; 32]),
        evidence: None,
        include_stamps: Vec::new(),
        root_hash: 0,
    }));
    job.record_reuse(&Ok(Reuse::Miss(ReuseMiss::Open)));
    job.record_reuse(&Err(PersistError::Cancelled));
    job.record_reuse(&Err(PersistError::Store(Bypass::Busy)));
    let snapshot = cache.counters().unwrap().snapshot();
    assert_eq!((snapshot.hits, snapshot.decoded_bytes), (1, 64));
    assert_eq!(snapshot.misses[MissCause::Input as usize], 1);
    assert_eq!(snapshot.misses[MissCause::Budget as usize], 1);
    assert_eq!(snapshot.bypasses[names::bypass_index(&Bypass::Busy)], 1);
    assert!(cache.job().is_some());
    job.record_reuse(&Err(PersistError::Store(Bypass::Fenced)));
    job.record_reuse(&Err(PersistError::Store(Bypass::Fenced)));
    assert!(
        cache.job().is_none(),
        "a fenced reuse fences the generation"
    );
    drop(job);
    assert_eq!(cache.settle(SETTLE), CacheState::Enabled);
    assert_eq!(
        cache.counters().unwrap().snapshot().bypasses[names::bypass_index(&Bypass::Fenced)],
        1
    );
    cache.shutdown();
}
