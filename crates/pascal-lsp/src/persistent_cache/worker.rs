//! The `pascal-cache` thread (D23): opens and reopens the store, and
//! retries opens whose bypass advises it (D24). Task 6 adds maintenance.

use super::{CacheState, Inner, StatusCause};
use crate::workspace::admitted::{AdmittedStore, StoreBypass};
use pascal_cache::Bypass;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) enum Command {
    Open,
    Shutdown,
}

/// Holds a `Weak<Inner>`: dropping the last handle drops the sender, which
/// ends the loop.
pub(super) fn spawn(inner: &Arc<Inner>) -> (crossbeam_channel::Sender<Command>, JoinHandle<()>) {
    let (sender, commands) = crossbeam_channel::unbounded();
    let weak = Arc::downgrade(inner);
    let handle = std::thread::Builder::new()
        .name("pascal-cache".into())
        .spawn(move || run(&weak, &commands))
        .expect("spawn the pascal-cache thread");
    (sender, handle)
}

fn run(weak: &Weak<Inner>, commands: &crossbeam_channel::Receiver<Command>) {
    loop {
        let timer = match weak.upgrade() {
            Some(inner) => retry_timer(&inner),
            None => return,
        };
        crossbeam_channel::select! {
            recv(commands) -> command => match command {
                Ok(Command::Open) => {}
                Ok(Command::Shutdown) | Err(_) => return,
            },
            recv(timer) -> _ => {
                if let Some(inner) = weak.upgrade() {
                    retry_due(&inner);
                }
            },
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        // Transitions made on this thread never send to it: open whenever
        // the state asks for it.
        while open(&inner) {}
    }
}

fn retry_timer(inner: &Inner) -> crossbeam_channel::Receiver<Instant> {
    let shared = inner.lock();
    match shared.retry {
        Some((generation, due)) if generation == shared.generation => crossbeam_channel::at(due),
        _ => crossbeam_channel::never(),
    }
}

/// The retry timer fired: leave the bypass for `Opening` unless anything
/// moved the state since it was armed.
fn retry_due(inner: &Arc<Inner>) {
    let mut shared = inner.lock();
    let Some((generation, due)) = shared.retry else {
        return;
    };
    if generation != shared.generation
        || !matches!(shared.state, CacheState::Bypassed(_))
        || shared.shut_down
        || Instant::now() < due
    {
        return;
    }
    shared.retry = None;
    shared.set_state(CacheState::Opening);
    inner.changed.notify_all();
}

/// One open attempt, counted as an admitted job. Returns whether the state
/// is `Opening` again afterwards (a fence finished into a reopen).
fn open(inner: &Arc<Inner>) -> bool {
    let (generation, directory, limits) = {
        let mut shared = inner.lock();
        if shared.state != CacheState::Opening || shared.shut_down {
            return false;
        }
        shared.active += 1;
        (
            shared.generation,
            shared.directory.clone(),
            shared.options.limits(),
        )
    };
    #[cfg(test)]
    if let Some((entered, release)) = super::lock(&inner.open_hook).take() {
        let _ = entered.send(());
        let _ = release.recv();
    }
    let opened = directory
        .ok_or(())
        .or_else(|()| pascal_cache::default_directory())
        .map_err(StoreBypass::Store)
        .and_then(|directory| {
            AdmittedStore::open(&inner.project_cache, &directory, &inner.namespace, limits)
        });
    let mut shared = inner.lock();
    if shared.state != CacheState::Opening || shared.generation != generation {
        // Fenced meanwhile: close what was opened before acknowledging.
        drop(shared);
        drop(opened);
        let mut shared = inner.lock();
        shared.active -= 1;
        inner.finish_fence(&mut shared);
        let reopen = shared.state == CacheState::Opening;
        inner.unlock(shared);
        inner.changed.notify_all();
        return reopen;
    }
    shared.active -= 1;
    match opened {
        Ok(store) => {
            shared.reset_epoch = Some(store.scope().reset_epoch);
            shared.store = Some(Arc::new(store));
            shared.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            shared.retry_delay = None;
            shared.set_state(CacheState::Enabled);
        }
        Err(StoreBypass::Memory) => shared.set_state(CacheState::Bypassed(StatusCause::Memory)),
        Err(StoreBypass::Store(bypass)) => {
            inner.counters.record_bypass(&bypass);
            let retry = retries(&bypass);
            shared.set_state(CacheState::Bypassed(StatusCause::Store(bypass)));
            if retry {
                let delay = retry_delay(shared.retry_delay);
                shared.retry_delay = Some(delay);
                shared.retry = Some((shared.generation, Instant::now() + delay));
            }
        }
    }
    inner.unlock(shared);
    inner.changed.notify_all();
    false
}

pub(super) fn retries(bypass: &Bypass) -> bool {
    pascal_cache::admin::advice(bypass) == pascal_cache::admin::RecoveryAdvice::Retry
}

pub(super) fn retry_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(pascal_cache::MIN_HEALTH_BACKOFF, |delay| {
        delay
            .saturating_mul(2)
            .min(pascal_cache::MAX_HEALTH_BACKOFF)
    })
}
