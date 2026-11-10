//! Process-wide allocator settings, applied by the server binary before it
//! spawns any thread.
//!
//! glibc gives each new thread an arena, up to eight per core, and keeps
//! what a thread freed in its arena. Each parallel index batch runs its
//! extraction on new scoped threads, so a long indexing run spreads its
//! working set over dozens of arenas that stay resident. Two arenas bound
//! that: on an 8,670-member project with persistence on, the post-crawl
//! high water mark fell from 7,017 to 5,412 MiB at equal progress (measured
//! with `MALLOC_ARENA_MAX=2`), below the persistence-off run, at a 5-10 %
//! cost in extraction throughput (l4d-zf1b.8, root-cause pass 3).

use std::ffi::OsString;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The arena count the server sets when the user did not choose one.
pub const ARENA_MAX: i32 = 2;

/// The `M_ARENA_MAX` to set, if any: [`ARENA_MAX`] on glibc unless the
/// environment (`lookup`) already chooses an arena count, through
/// `MALLOC_ARENA_MAX` or a `glibc.malloc.arena_max` tunable. Explicit
/// settings win, and other allocators are left alone.
pub fn arena_limit(lookup: impl Fn(&str) -> Option<OsString>) -> Option<i32> {
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return None;
    }
    if lookup("MALLOC_ARENA_MAX").is_some() {
        return None;
    }
    if lookup("GLIBC_TUNABLES").is_some_and(|tunables| {
        tunables
            .to_string_lossy()
            .contains("glibc.malloc.arena_max")
    }) {
        return None;
    }
    Some(ARENA_MAX)
}

/// Applies [`arena_limit`] for this process's environment and traces the
/// decision (`PASCAL_LSP_TRACE` only: stdout is the LSP channel). Call it
/// first: arenas that threads already created are not given back.
pub fn limit_malloc_arenas() {
    let limit = arena_limit(|name| std::env::var_os(name));
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Some(arenas) = limit {
        // SAFETY: glibc's `mallopt` takes the allocator's own lock, so it
        // is sound from any thread at any time; it only changes tuning.
        // Calling it before other threads start is what makes it
        // effective, not what makes it safe.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, arenas);
        }
    }
    crate::trace::trace!("{}", arena_decision(limit));
}

/// Gives the allocator's free pages back to the system, at most every ten
/// seconds. glibc keeps what threads free in its arenas and returns only
/// the top of each: on SFClient2 a 1.2 GiB live heap held 5.4 GiB of arena
/// after the open-file crawl, 4.1 GiB of it free (l4d-10). Called by the
/// warmer between units and batch members, while no interactive request
/// runs: a trim holds each arena's lock for 15 ms typically, 150 ms at most.
pub(crate) fn release_free_memory() {
    const MIN_INTERVAL: Duration = Duration::from_secs(10);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|last| last.elapsed() < MIN_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let started = Instant::now();
        // SAFETY: malloc_trim only returns free allocator pages to the
        // system; it takes each arena's own lock.
        unsafe {
            libc::malloc_trim(0);
        }
        crate::trace::trace!(
            "malloc_trim elapsed_ms={:.1}",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// The startup trace line for an [`arena_limit`] outcome.
fn arena_decision(limit: Option<i32>) -> String {
    match limit {
        Some(arenas) => format!("malloc arenas: {arenas}"),
        None if cfg!(all(target_os = "linux", target_env = "gnu")) => {
            "malloc arenas: skipped: MALLOC_ARENA_MAX or glibc.malloc.arena_max set".to_owned()
        }
        None => "malloc arenas: skipped: not glibc".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(variables: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let variables = variables
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect::<Vec<_>>();
        move |name| {
            variables
                .iter()
                .find(|(variable, _)| variable == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn glibc_gets_two_arenas_unless_the_environment_chooses() {
        assert_eq!(arena_limit(environment(&[])), Some(2));
        assert_eq!(
            arena_limit(environment(&[(
                "GLIBC_TUNABLES",
                "glibc.malloc.tcache_count=0"
            )])),
            Some(2)
        );
        assert_eq!(arena_limit(environment(&[("MALLOC_ARENA_MAX", "8")])), None);
        assert_eq!(arena_limit(environment(&[("MALLOC_ARENA_MAX", "")])), None);
        assert_eq!(
            arena_limit(environment(&[(
                "GLIBC_TUNABLES",
                "glibc.malloc.check=0:glibc.malloc.arena_max=4"
            )])),
            None
        );
    }

    #[test]
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    fn other_allocators_are_left_alone() {
        assert_eq!(arena_limit(environment(&[])), None);
    }

    #[test]
    fn the_trace_line_names_the_decision() {
        assert_eq!(arena_decision(Some(2)), "malloc arenas: 2");
        let skipped = arena_decision(None);
        assert!(skipped.starts_with("malloc arenas: skipped: "), "{skipped}");
    }
}
