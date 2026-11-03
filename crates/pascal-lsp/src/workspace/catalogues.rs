//! Workspace access to the shared directory catalogues (l4d-20).
//!
//! Every lookup goes through a `CatalogueScope`, the only constructor of a
//! catalogue key, so the key's exclusion identity is always the matcher the
//! walk applies. A stored catalogue is validated by stamps at most once per
//! workspace view and request generation, unless it is watched (Task 8).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use super::{ExcludeMatcher, Workspace, absolute_path, path_stamp};
use crate::catalogue::{Bound, Catalogue, CatalogueKey, Depth, Exclusions, SharedCatalogue};

/// The catalogues this view validated during its current request.
#[derive(Debug, Default)]
pub(super) struct CatalogueValidations {
    request_generation: u64,
    validated: HashSet<CatalogueKey>,
}

#[derive(Debug, Clone)]
pub(super) struct CatalogueScope {
    key: CatalogueKey,
    matcher: Option<ExcludeMatcher>,
}

impl CatalogueScope {
    /// Every non-excluded directory under `root`; `None` excludes nothing.
    pub(super) fn recursive(root: &Path, matcher: Option<&ExcludeMatcher>) -> Self {
        let root = absolute_path(root.to_path_buf());
        let exclusions = matcher.map_or(Exclusions::Nothing, |matcher| matcher.exclusions(&root));
        Self {
            key: CatalogueKey {
                root,
                depth: Depth::Recursive,
                exclusions,
            },
            matcher: matcher.cloned(),
        }
    }

    /// The files directly in `directory`.
    pub(super) fn listing(directory: &Path) -> Self {
        Self {
            key: CatalogueKey {
                root: absolute_path(directory.to_path_buf()),
                depth: Depth::Listing,
                exclusions: Exclusions::Nothing,
            },
            matcher: None,
        }
    }

    pub(super) fn key(&self) -> &CatalogueKey {
        &self.key
    }

    fn excluded(&self, path: &Path) -> bool {
        self.matcher
            .as_ref()
            .is_some_and(|matcher| matcher.is_excluded(path, &self.key.root))
    }
}

/// Where a catalogue's build stands after a batch: the cursor
/// `l4d-zf1b.8` resumes from (the pending directories live in the stored
/// catalogue itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatalogueProgress {
    pub(crate) complete: bool,
    pub(crate) counted: usize,
    pub(crate) pending_directories: usize,
    /// The generation it was published under; `None` when it was not kept.
    pub(crate) generation: Option<u64>,
}

/// A complete catalogue, or one a limit stopped for good: advancing it again
/// lists nothing and must not republish it.
fn is_terminal(catalogue: &Catalogue) -> bool {
    catalogue.is_complete() || catalogue.limit().is_some()
}

impl Workspace {
    /// The complete (up to the scope cap) catalogue a workspace snapshot
    /// enumerates, cancellable between filesystem operations.
    pub(super) fn snapshot_catalogue(
        &mut self,
        scope: &CatalogueScope,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Arc<Catalogue>, String> {
        self.shared_catalogue(scope, crate::catalogue::MAX_CATALOGUE_ENTRIES, &mut || {
            super::check_workspace_cancel(Some(cancel))
        })
    }

    /// The files directly in `directory`, cancellable.
    pub(super) fn listing_catalogue_with_cancel(
        &mut self,
        directory: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Arc<Catalogue>, String> {
        self.snapshot_catalogue(&CatalogueScope::listing(directory), cancel)
    }

    /// The shared catalogue of `scope`, extended until it is complete or
    /// counts `need` entries. `before_io` runs before every filesystem
    /// operation (cancellation, budgets, test barriers); its error publishes
    /// nothing.
    pub(super) fn shared_catalogue(
        &mut self,
        scope: &CatalogueScope,
        need: usize,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<Arc<Catalogue>, String> {
        let current = self.validated_catalogue(scope, before_io)?;
        if let Some(shared) = &current
            && (is_terminal(&shared.catalogue) || shared.catalogue.counted() >= need)
        {
            return Ok(Arc::clone(&shared.catalogue));
        }
        Ok(self
            .extend_catalogue(scope, current, Bound::entries(need), before_io)?
            .0)
    }

    /// One bounded batch of `scope`'s catalogue, resumed from the stored one
    /// (the batch API `l4d-zf1b.8` drives).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn advance_catalogue(
        &mut self,
        scope: &CatalogueScope,
        bound: Bound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<CatalogueProgress, String> {
        let current = self.validated_catalogue(scope, before_io)?;
        let (catalogue, generation) = match current {
            Some(shared) if is_terminal(&shared.catalogue) => {
                (shared.catalogue, Some(shared.generation))
            }
            current => self.extend_catalogue(scope, current, bound, before_io)?,
        };
        Ok(CatalogueProgress {
            complete: catalogue.is_complete(),
            counted: catalogue.counted(),
            pending_directories: catalogue.frontier_len(),
            generation,
        })
    }

    fn extend_catalogue(
        &mut self,
        scope: &CatalogueScope,
        current: Option<SharedCatalogue>,
        bound: Bound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(Arc<Catalogue>, Option<u64>), String> {
        let (base, mut catalogue) = match current {
            Some(shared) => (Some(shared.generation), Catalogue::clone(&shared.catalogue)),
            None => (
                None,
                Catalogue::new(&scope.key.root, scope.key.depth, before_io)?,
            ),
        };
        catalogue.advance(bound, &|path| scope.excluded(path), before_io)?;
        let catalogue = Arc::new(catalogue);
        let generation =
            self.project_cache
                .publish_catalogue(scope.key.clone(), base, Arc::clone(&catalogue));
        self.note_catalogue_validated(&scope.key);
        Ok((catalogue, generation))
    }

    /// The stored catalogue of `scope` if it still holds: watched, already
    /// validated by this view this request, or every listed directory's
    /// settled stamp unchanged (a changed or unsettled one drops it, so a
    /// catalogue is confirmed watched only after its stamps proved it).
    fn validated_catalogue(
        &mut self,
        scope: &CatalogueScope,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<Option<SharedCatalogue>, String> {
        let Some(shared) = self.project_cache.catalogue(&scope.key) else {
            return Ok(None);
        };
        if shared.watched || self.catalogue_validated(&scope.key) {
            return Ok(Some(shared));
        }
        #[cfg(test)]
        crate::catalogue::test_record_stamp_check();
        for (directory, stamp) in shared.catalogue.directories() {
            before_io()?;
            // A recorded stamp that was not settled proves nothing (l4d-122):
            // drop the catalogue so the rebuild records fresh stamps.
            let unchanged = match (stamp, path_stamp(directory)) {
                (Some(recorded), Some(current)) => recorded.proves_unchanged(&current),
                (None, None) => true,
                _ => false,
            };
            if !unchanged {
                self.project_cache
                    .drop_catalogue(&scope.key, shared.generation);
                return Ok(None);
            }
        }
        if shared.watch_registered {
            // The watches existed before this check began, so the check
            // closed the gap between listing and registering.
            self.project_cache
                .confirm_catalogue_watched(&scope.key, shared.generation);
        }
        self.note_catalogue_validated(&scope.key);
        Ok(Some(shared))
    }

    fn catalogue_validated(&mut self, key: &CatalogueKey) -> bool {
        self.start_catalogue_validations_for_request();
        self.catalogue_validations.validated.contains(key)
    }

    fn note_catalogue_validated(&mut self, key: &CatalogueKey) {
        self.start_catalogue_validations_for_request();
        self.catalogue_validations.validated.insert(key.clone());
    }

    fn start_catalogue_validations_for_request(&mut self) {
        if self.catalogue_validations.request_generation != self.request_generation {
            self.catalogue_validations = CatalogueValidations {
                request_generation: self.request_generation,
                validated: HashSet::new(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::{
        Limit, MAX_CATALOGUE_ENTRIES, test_listings, test_reset_listings, test_reset_stamp_checks,
        test_stamp_checks,
    };
    use crate::workspace::WorkspaceOptions;
    use pascal_project::delphi_overrides::OverrideSession;
    use std::fs;
    use std::path::PathBuf;

    fn workspace(root: &Path) -> Workspace {
        Workspace::with_override_session(
            vec![root.to_path_buf()],
            WorkspaceOptions::default(),
            OverrideSession::new(None),
        )
    }

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"unit X; interface implementation end.\n").unwrap();
    }

    fn names(catalogue: &Catalogue, root: &Path) -> Vec<PathBuf> {
        let mut names = catalogue
            .pascal_files()
            .map(|path| path.strip_prefix(root).unwrap().to_path_buf())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn no_io() -> impl FnMut() -> Result<(), String> {
        || Ok(())
    }

    struct AcceptAll;

    impl crate::file_watch::DirectoryWatch for AcceptAll {
        fn watch(&mut self, _: &Path) -> bool {
            true
        }
        fn unwatch(&mut self, _: &Path) {}
    }

    #[test]
    fn a_watched_catalogue_is_served_without_stamp_checks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        pascal_project::backdate_tree_for_test(&root);
        let main = workspace(&root);
        main.project_cache().set_watcher(Box::new(AcceptAll));
        let input = main.analysis_input();
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        test_reset_stamp_checks();
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        assert_eq!(test_stamp_checks(), 1, "one confirming check, then none");
    }

    #[test]
    fn an_unsettled_catalogue_is_listed_again_and_never_confirmed_watched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        write(&root.join("sub/B.pas"));
        let main = workspace(&root);
        main.project_cache().set_watcher(Box::new(AcceptAll));
        let scope = CatalogueScope::recursive(&root, None);
        let input = main.analysis_input();
        let serve = || {
            Workspace::from_analysis_input(&input)
                .shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut no_io())
                .unwrap()
        };
        serve();
        for _ in 0..2 {
            test_reset_listings();
            serve();
            assert!(
                test_listings().contains(&root),
                "an unsettled stamp proves nothing: {:?}",
                test_listings()
            );
            let stored = main.project_cache().catalogue(scope.key()).unwrap();
            assert!(
                !stored.watched,
                "an unsettled catalogue was confirmed watched"
            );
        }
        pascal_project::backdate_tree_for_test(&root);
        serve();
        test_reset_listings();
        serve();
        assert!(test_listings().is_empty(), "{:?}", test_listings());
        assert!(main.project_cache().catalogue(scope.key()).unwrap().watched);
    }

    #[test]
    fn a_change_before_publication_is_caught_by_the_confirming_check() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        let view = workspace(&root);
        view.project_cache().set_watcher(Box::new(AcceptAll));
        let scope = CatalogueScope::recursive(&root, None);
        let mut built = Catalogue::new(&root, Depth::Recursive, &mut no_io()).unwrap();
        built
            .advance(
                Bound::entries(MAX_CATALOGUE_ENTRIES),
                &|_| false,
                &mut no_io(),
            )
            .unwrap();
        // The event arrives while the catalogue is built: nothing is stored yet.
        write(&root.join("New.pas"));
        view.project_cache().invalidate_path(&root.join("New.pas"));
        view.project_cache()
            .publish_catalogue(scope.key().clone(), None, Arc::new(built))
            .expect("published");
        let stored = view.project_cache().catalogue(scope.key()).unwrap();
        assert!(stored.watch_registered && !stored.watched);
        let input = view.analysis_input();
        let served = Workspace::from_analysis_input(&input)
            .shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        assert!(
            served.pascal_files_named("new.pas").next().is_some(),
            "the stamp check dropped the stale catalogue instead of confirming it"
        );
    }

    #[test]
    fn an_unwatched_catalogue_is_stamp_checked_by_every_request() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        let input = workspace(&root).analysis_input();
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        test_reset_stamp_checks();
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        assert_eq!(test_stamp_checks(), 2);
    }

    #[test]
    fn an_event_drops_a_catalogue_this_request_already_validated() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("sub/A.pas"));
        let mut view = workspace(&root);
        view.filename_catalogue(&root);
        write(&root.join("sub/New.pas"));
        assert!(
            view.filename_catalogue(&root)
                .pascal_files_named("new.pas")
                .next()
                .is_none(),
            "validated this request: no stamp check"
        );
        view.project_cache()
            .invalidate_path(&root.join("sub/New.pas"));
        assert!(
            view.filename_catalogue(&root)
                .pascal_files_named("new.pas")
                .next()
                .is_some(),
            "the event dropped the catalogue"
        );
    }

    #[test]
    fn worker_views_share_one_catalogue_and_list_nothing_the_second_time() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        pascal_project::backdate_tree_for_test(&root);
        let input = workspace(&root).analysis_input();
        let first = Workspace::from_analysis_input(&input).filename_catalogue(&root);
        test_reset_listings();
        let second = Workspace::from_analysis_input(&input).filename_catalogue(&root);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(test_listings().is_empty(), "{:?}", test_listings());
    }

    #[test]
    fn the_next_request_sees_a_created_deleted_or_renamed_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        write(&root.join("sub/C.pas"));
        let input = workspace(&root).analysis_input();
        let before = Workspace::from_analysis_input(&input).filename_catalogue(&root);
        assert_eq!(
            names(&before, &root),
            [PathBuf::from("A.pas"), PathBuf::from("sub/C.pas")]
        );
        write(&root.join("sub/New.pas"));
        fs::rename(root.join("A.pas"), root.join("B.pas")).unwrap();
        fs::remove_file(root.join("sub/C.pas")).unwrap();
        let after = Workspace::from_analysis_input(&input).filename_catalogue(&root);
        assert_eq!(
            names(&after, &root),
            [PathBuf::from("B.pas"), PathBuf::from("sub/New.pas")]
        );
    }

    #[test]
    fn one_stamp_check_per_catalogue_per_request() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        let input = workspace(&root).analysis_input();
        Workspace::from_analysis_input(&input).filename_catalogue(&root);
        let mut view = Workspace::from_analysis_input(&input);
        test_reset_stamp_checks();
        view.filename_catalogue(&root);
        view.filename_catalogue(&root);
        assert_eq!(test_stamp_checks(), 1);
        view.advance_request_generation();
        view.filename_catalogue(&root);
        assert_eq!(test_stamp_checks(), 2);
    }

    #[test]
    fn two_exclusion_sets_over_one_root_keep_separate_catalogues() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("Main.pas"));
        write(&root.join("vendor/Lib.pas"));
        let mut view = workspace(&root);
        let wide = CatalogueScope::recursive(&root, Some(&ExcludeMatcher::new(&root, &root, &[])));
        let narrow = CatalogueScope::recursive(
            &root,
            Some(&ExcludeMatcher::new(&root, &root, &["vendor".to_string()])),
        );
        assert_ne!(wide.key(), narrow.key());
        let wide = view
            .shared_catalogue(&wide, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        let narrow = view
            .shared_catalogue(&narrow, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        assert_eq!(
            names(&wide, &root),
            [PathBuf::from("Main.pas"), PathBuf::from("vendor/Lib.pas")]
        );
        assert_eq!(names(&narrow, &root), [PathBuf::from("Main.pas")]);
        assert_eq!(view.project_cache().catalogue_count(), 2);
    }

    #[test]
    fn a_cancelled_walk_publishes_nothing_and_keeps_the_previous_catalogue() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("a/A.pas"));
        write(&root.join("b/B.pas"));
        let mut view = workspace(&root);
        let scope = CatalogueScope::recursive(&root, None);
        let first = view
            .advance_catalogue(&scope, Bound::directories(1), &mut no_io())
            .unwrap();
        assert!(!first.complete);
        let mut calls = 0usize;
        let error = view
            .advance_catalogue(&scope, Bound::directories(usize::MAX), &mut || {
                calls += 1;
                if calls > 1 {
                    Err("cancelled".to_string())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(error, "cancelled");
        let stored = view.project_cache().catalogue(scope.key()).expect("kept");
        assert_eq!(Some(stored.generation), first.generation);
        assert_eq!(stored.catalogue.frontier_len(), first.pending_directories);
    }

    #[test]
    fn batches_through_the_store_end_at_the_one_shot_catalogue() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for name in ["R.pas", "a/A.pas", "a/x/X.pas", "b/B.pas"] {
            write(&root.join(name));
        }
        let scope = CatalogueScope::recursive(&root, None);
        let one_shot = workspace(&root)
            .shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        let mut batched = workspace(&root);
        let mut batches = 0;
        loop {
            let progress = batched
                .advance_catalogue(&scope, Bound::directories(1), &mut no_io())
                .unwrap();
            batches += 1;
            assert!(progress.generation.is_some(), "every batch is published");
            if progress.complete {
                break;
            }
            assert!(batches < 100);
        }
        let stored = batched.project_cache().catalogue(scope.key()).unwrap();
        assert_eq!(*stored.catalogue, *one_shot);
        assert_eq!(batches, 4, "root, a, b, a/x");
    }

    #[test]
    fn a_limit_stopped_catalogue_is_terminal_and_never_republished() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("A.pas"));
        write(&root.join("sub/B.pas"));
        pascal_project::backdate_tree_for_test(&root);
        let mut view = workspace(&root);
        let scope = CatalogueScope::recursive(&root, None);
        let mut stopped =
            Catalogue::new(&scope.key().root, Depth::Recursive, &mut no_io()).unwrap();
        stopped
            .advance(Bound::directories(1), &|_| false, &mut no_io())
            .unwrap();
        stopped.test_stop(Limit::Entries);
        let published = view
            .project_cache()
            .publish_catalogue(scope.key().clone(), None, Arc::new(stopped))
            .expect("published");
        let generation = view.project_cache().catalogue_generation();
        test_reset_listings();
        let shared = view
            .shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        assert_eq!(shared.limit(), Some(Limit::Entries));
        let progress = view
            .advance_catalogue(&scope, Bound::directories(usize::MAX), &mut no_io())
            .unwrap();
        assert!(!progress.complete);
        assert_eq!(progress.generation, Some(published));
        view.advance_request_generation();
        let again = view
            .shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        assert!(Arc::ptr_eq(&shared, &again));
        assert_eq!(view.project_cache().catalogue_generation(), generation);
        assert!(test_listings().is_empty(), "{:?}", test_listings());
    }

    /// l4d-vrw0: the store directory inside a root is not catalogued, and
    /// the exclusion is part of the key only for roots containing it.
    #[test]
    fn a_root_containing_the_store_catalogues_none_of_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("Main.pas"));
        let store = root.join(".cache/lint4d/analysis");
        fs::create_dir_all(store.join("namespaces")).unwrap();
        fs::write(store.join("analysis.sqlite3"), b"").unwrap();
        fs::write(store.join("namespaces/0123.lock"), b"").unwrap();
        let cache = crate::persistent_cache::CacheExclusion::new(&store);

        let plain = workspace(&root);
        let plain_scope = CatalogueScope::recursive(&root, Some(&plain.roots[0].excludes));
        let mut view = workspace(&root).with_cache_exclusion(Some(cache.clone()));
        let scope = CatalogueScope::recursive(&root, Some(&view.roots[0].excludes));
        assert_ne!(scope.key(), plain_scope.key());
        let catalogue = view
            .shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut no_io())
            .unwrap();
        assert_eq!(names(&catalogue, &root), [PathBuf::from("Main.pas")]);
        let directories: Vec<_> = catalogue
            .directories()
            .iter()
            .map(|(directory, _)| directory.clone())
            .collect();
        assert!(
            directories
                .iter()
                .all(|directory| !cache.contains(directory)),
            "{directories:?}"
        );
        assert!(
            directories.contains(&root.join(".cache/lint4d")),
            "{directories:?}"
        );
        assert!(!view.accepts_path(&store.join("Stray.pas")));
        assert!(view.accepts_path(&root.join("Main.pas")));

        // Request views and walks outside the roots leave it out too.
        let request = Workspace::from_analysis_input(&view.analysis_input());
        assert_eq!(
            CatalogueScope::recursive(&root, Some(&request.roots[0].excludes)).key(),
            scope.key()
        );
        let default = CatalogueScope::recursive(&root, Some(&view.default_excludes(&root)));
        assert!(matches!(
            default.key().exclusions,
            Exclusions::WithCache { .. }
        ));

        // A root not containing it keeps the key it had.
        let other = temp.path().join("other");
        write(&other.join("Other.pas"));
        let outside = workspace(&other).with_cache_exclusion(Some(cache));
        assert_eq!(
            CatalogueScope::recursive(&other, Some(&outside.roots[0].excludes)).key(),
            CatalogueScope::recursive(&other, Some(&workspace(&other).roots[0].excludes)).key(),
        );
    }
}
