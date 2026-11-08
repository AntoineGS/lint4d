//! Test-server store fault points and growth limit (`l4d-zf1b.12`).
//!
//! `PASCAL_LSP_TEST_STORE_FAULT=<Point>|<entered>|<release>|<action>[|<nth>]`
//! acts on the nth arrival at a publication point, in any thread of this
//! process: `hold` creates `<entered>` and waits for `<release>`, `abort`
//! ends the process. Earlier arrivals pass. `PASCAL_LSP_TEST_STORE_GROWTH_PAGES`
//! limits the database's growth right after every open. The hook runs inside
//! a write transaction and never touches the store.

use pascal_cache::{Digest, FaultPoint, Namespace, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Hold,
    Abort,
}

#[derive(Clone, Debug)]
pub struct StoreFaultSpec {
    point: FaultPoint,
    entered: PathBuf,
    release: PathBuf,
    action: Action,
    nth: usize,
    hits: Arc<AtomicUsize>,
}

impl StoreFaultSpec {
    pub fn parse(spec: &str) -> Result<Self, String> {
        const FORMAT: &str = "must contain <Point>|<entered>|<release>|<hold|abort>[|<nth>]";
        let mut parts = spec.split('|');
        let (Some(point), Some(entered), Some(release), Some(action)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(FORMAT.to_string());
        };
        let nth = parts.next();
        if parts.next().is_some() {
            return Err(FORMAT.to_string());
        }
        let point =
            FaultPoint::from_name(point).ok_or_else(|| format!("unknown fault point {point:?}"))?;
        let action = match action {
            "hold" => Action::Hold,
            "abort" => Action::Abort,
            other => return Err(format!("unknown fault action {other:?}")),
        };
        let nth = match nth {
            None => 1,
            Some(nth) => nth
                .parse::<usize>()
                .ok()
                .filter(|nth| *nth >= 1)
                .ok_or_else(|| format!("fault nth {nth:?} must be an integer of at least 1"))?,
        };
        Ok(Self {
            point,
            entered: PathBuf::from(entered),
            release: PathBuf::from(release),
            action,
            nth,
            hits: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[derive(Default)]
struct Installed {
    fault: Option<StoreFaultSpec>,
    growth_pages: Option<i64>,
    build_fingerprint: Option<Digest>,
}

static INSTALLED: Mutex<Installed> = Mutex::new(Installed {
    fault: None,
    growth_pages: None,
    build_fingerprint: None,
});

/// Parses `PASCAL_LSP_TEST_NAMESPACE`: 64 hex characters standing in for the
/// build fingerprint, so one test binary can play several builds.
pub fn parse_build_fingerprint(text: &str) -> Result<Digest, String> {
    Digest::from_hex(text).ok_or_else(|| "must be 64 hexadecimal characters".to_string())
}

/// The namespace the test server opens its store with, when overridden.
pub(crate) fn namespace_override() -> Option<Namespace> {
    let installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    installed.build_fingerprint.map(Namespace::new)
}

/// Record what every store this process opens is armed with.
pub(crate) fn install(
    fault: Option<StoreFaultSpec>,
    growth_pages: Option<i64>,
    build_fingerprint: Option<Digest>,
) {
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    installed.fault = fault;
    installed.growth_pages = growth_pages;
    installed.build_fingerprint = build_fingerprint;
}

/// Arm a freshly opened store.
pub(crate) fn arm(store: &Store) {
    let (fault, growth) = {
        let installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
        (installed.fault.clone(), installed.growth_pages)
    };
    if let Some(spec) = fault {
        let point = spec.point;
        store.set_fault_for_test(point, move || act(&spec));
    }
    if let Some(pages) = growth
        && let Err(error) = store.limit_growth_for_test(pages)
    {
        eprintln!("pascal-lsp-test-server: could not limit store growth: {error:?}");
    }
}

fn act(spec: &StoreFaultSpec) {
    if spec.hits.fetch_add(1, Ordering::SeqCst) + 1 != spec.nth {
        return;
    }
    match spec.action {
        Action::Abort => std::process::abort(),
        Action::Hold => {
            if let Err(error) = crate::server::hold_at_test_paths(&spec.entered, &spec.release) {
                eprintln!("pascal-lsp-test-server: {error}");
            }
        }
    }
}

/// `pascal/testStoreGrowth`: limit the open store's growth; `false` without one.
pub(crate) fn limit_growth(cache: &super::PersistentCache, extra_pages: i64) -> bool {
    cache
        .job()
        .is_some_and(|job| job.store().limit_growth_for_test(extra_pages).is_ok())
}

/// `pascal/testStoreClock`: advance the open store's test clock, which ends
/// its health backoff; `false` without an open store.
pub(crate) fn advance_clock(cache: &super::PersistentCache, by: std::time::Duration) -> bool {
    cache.job().is_some_and(|job| {
        job.store().advance_clock_for_test(by);
        true
    })
}
