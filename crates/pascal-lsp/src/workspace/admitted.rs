//! The LSP's only way to open the persistent store (spec §10 amendment
//! 2026-11-02): the store's page cache and connection overhead (`Pages`)
//! and its scratch (`Scratch`) are reserved from `maxCacheBytes` before
//! `Store::open`, held for the store's lifetime, and a refused reservation
//! bypasses persistence without opening anything.
//!
//! One admitted store per process: each `Store` has its own page cache,
//! scratch and connections, so a second one would be charged again.
//! `l4d-zf1b.8` owns the single production open; this phase has no caller.
#![cfg_attr(not(test), allow(dead_code))]

use crate::project_cache::{ProjectCache, Reservation, ReservationKind};
use pascal_cache::{Bypass, Namespace, Store, StoreLimits};
use std::path::Path;

/// Why no admitted store is open. Either way analysis continues without
/// the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreBypass {
    /// `maxCacheBytes` has no room for the store's allowance.
    Memory,
    Store(Bypass),
}

pub(crate) struct AdmittedStore {
    // Declared first: the store closes before its reservations release.
    store: Store,
    _pages: Reservation,
    _scratch: Reservation,
}

impl std::fmt::Debug for AdmittedStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdmittedStore")
            .field("limits", &self.store.limits())
            .finish_non_exhaustive()
    }
}

impl AdmittedStore {
    /// Reserve `limits.page_admission_bytes()` as `Pages` and
    /// `limits.scratch_bytes` as `Scratch` together, evicting nothing unless
    /// both fit, then open the store. Invalid limits reserve nothing; a
    /// failed open releases both reservations.
    pub(crate) fn open(
        cache: &ProjectCache,
        directory: &Path,
        namespace: &Namespace,
        limits: StoreLimits,
    ) -> Result<Self, StoreBypass> {
        limits
            .validate()
            .map_err(|error| StoreBypass::Store(Bypass::InvalidLimits(error)))?;
        let bytes = |bytes: u64| usize::try_from(bytes).map_err(|_| StoreBypass::Memory);
        let reservations = cache
            .reserve_all(&[
                (
                    ReservationKind::Pages,
                    bytes(limits.page_admission_bytes())?,
                ),
                (ReservationKind::Scratch, bytes(limits.scratch_bytes)?),
            ])
            .ok_or(StoreBypass::Memory)?;
        let [pages, scratch] =
            <[Reservation; 2]>::try_from(reservations).expect("one reservation per request");
        let store = Store::open(directory, namespace, limits).map_err(StoreBypass::Store)?;
        Ok(Self {
            store,
            _pages: pages,
            _scratch: scratch,
        })
    }
}

impl std::ops::Deref for AdmittedStore {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.store
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::project_cache::{ImportValue, Lookup};
    use crate::workspace::persist::tests::private_dir;
    use lsp_types::Url;
    use pascal_cache::{DATABASE_FILE, Digest, LocationIssue, MAX_ROW_BATCH_BYTES};
    use pascal_project::ProjectContext;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn namespace() -> Namespace {
        Namespace::new(Digest::from_bytes([11; 32]))
    }

    #[test]
    fn opening_reserves_pages_and_scratch_for_the_store_s_lifetime() {
        let cache = ProjectCache::new(64 << 20);
        let directory = private_dir();
        let limits = StoreLimits::default();
        let store = AdmittedStore::open(&cache, directory.path(), &namespace(), limits).unwrap();
        let stats = cache.stats();
        assert_eq!(
            stats.reserved_pages_bytes as u64,
            limits.page_admission_bytes()
        );
        assert_eq!(stats.reserved_scratch_bytes as u64, limits.scratch_bytes);
        assert_eq!(stats.reserved_bytes() as u64, limits.admission_bytes());
        store.check_scope().unwrap();
        drop(store);
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }

    #[test]
    fn a_store_that_does_not_fit_is_never_opened() {
        let limits = StoreLimits::default();
        let cache = ProjectCache::new(limits.admission_bytes() as usize - 1);
        let directory = private_dir();
        assert_eq!(
            AdmittedStore::open(&cache, directory.path(), &namespace(), limits).err(),
            Some(StoreBypass::Memory)
        );
        assert!(
            !directory.path().join(DATABASE_FILE).exists(),
            "no connection was opened"
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "nothing was created in the cache directory"
        );
        assert_eq!(cache.stats().reserved_bytes(), 0);
        // Smaller limits fit the same budget.
        let small = StoreLimits {
            scratch_bytes: MAX_ROW_BATCH_BYTES as u64,
            ..limits
        };
        assert!(AdmittedStore::open(&cache, directory.path(), &namespace(), small).is_ok());
    }

    /// Pages alone fit only by evicting the cached entry; Pages and Scratch
    /// together never fit, so the open refuses before evicting anything.
    #[test]
    fn a_store_whose_scratch_does_not_fit_evicts_nothing() {
        let limits = StoreLimits::default();
        let cache = ProjectCache::new(limits.page_admission_bytes() as usize + 1);
        let importer = Url::from_file_path("/ws/Main.pas").unwrap();
        let no_cancel = AtomicBool::new(false);
        let Lookup::Compute(claim) = cache.imports(
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
        cache.store_imports(claim, value, 100, &no_cancel);
        assert_eq!(cache.ready_layers(&importer), ["Import"]);
        let directory = private_dir();
        assert_eq!(
            AdmittedStore::open(&cache, directory.path(), &namespace(), limits).err(),
            Some(StoreBypass::Memory)
        );
        assert_eq!(
            cache.ready_layers(&importer),
            ["Import"],
            "nothing was evicted"
        );
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }

    #[test]
    fn a_failed_open_releases_its_reservations() {
        let cache = ProjectCache::new(64 << 20);
        let directory = private_dir();
        std::fs::set_permissions(directory.path(), PermissionsExt::from_mode(0o755)).unwrap();
        assert!(matches!(
            AdmittedStore::open(
                &cache,
                directory.path(),
                &namespace(),
                StoreLimits::default()
            ),
            Err(StoreBypass::Store(Bypass::Location(
                LocationIssue::NotPrivate { .. }
            )))
        ));
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }

    #[test]
    fn invalid_limits_reserve_nothing() {
        let cache = ProjectCache::new(64 << 20);
        let directory = private_dir();
        let invalid = StoreLimits {
            readers: 0,
            ..StoreLimits::default()
        };
        assert!(matches!(
            AdmittedStore::open(&cache, directory.path(), &namespace(), invalid),
            Err(StoreBypass::Store(Bypass::InvalidLimits(_)))
        ));
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }
}
