//! Process-wide allocator settings, applied by the server binary before it
//! spawns any thread.
//!
//! glibc gives each new thread an arena, up to eight per core, and keeps
//! what a thread freed in its arena. Each parallel index batch runs its
//! extraction on new scoped threads, so a long indexing run spreads its
//! working set over dozens of arenas that stay resident. Two arenas bound
//! that: on a 8,670-member project with persistence on, the post-crawl high
//! water mark fell from 7.0 to 5.4 GiB at equal progress (below the
//! persistence-off run), at a 5-10 % cost in extraction throughput
//! (l4d-zf1b.8, root-cause pass 3).

use std::ffi::OsString;

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

/// Applies [`arena_limit`] for this process's environment. Call it before
/// any thread starts.
pub fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Some(arenas) = arena_limit(|name| std::env::var_os(name)) {
        // SAFETY: `mallopt` only changes the allocator's tuning; it is
        // called before the process starts other threads.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, arenas);
        }
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
}
