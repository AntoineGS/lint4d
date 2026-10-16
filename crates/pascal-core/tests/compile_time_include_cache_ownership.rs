use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
use pascal_core::{
    FilesystemSourceStore, IncludeResolveRequest, NoCancellation, Resolution, ResolverLimits,
    UnitResolver,
};
use pascal_project::{ProjectContext, ProjectOptions};

#[test]
fn actual_include_decoded_cache_keeps_credits_until_its_last_owner_drops() {
    // Preserve real filesystem authorization/read behavior, but return raw
    // bytes as SourceStore permits. This selects UnitResolver::include's own
    // decode-and-cache mutation, rather than the default store's earlier decode.
    struct RawFileStore(FilesystemSourceStore);
    impl pascal_core::SourceStore for RawFileStore {
        fn list_directory_admitted(
            &mut self,
            request: pascal_core::DirectoryRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<
            pascal_core::compile_time::AdmittedOwned<pascal_core::DirectoryListing>,
            pascal_core::SourceStoreError,
        > {
            self.0.list_directory_admitted(request, budget, cancel)
        }
        fn overlay_candidates_admitted(
            &self,
            roots: &[std::path::PathBuf],
            names: &[String],
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<
            pascal_core::compile_time::AdmittedOwned<Vec<std::path::PathBuf>>,
            pascal_core::SourceStoreError,
        > {
            self.0
                .overlay_candidates_admitted(roots, names, budget, cancel)
        }
        fn load_admitted(
            &mut self,
            request: pascal_core::SourceRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<pascal_core::LoadedSource, pascal_core::SourceStoreError> {
            self.0.load_raw_admitted(request, budget, cancel)
        }
        fn list_directory(
            &mut self,
            request: pascal_core::DirectoryRequest<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<pascal_core::DirectoryListing, pascal_core::SourceStoreError> {
            self.0.list_directory(request, cancel)
        }
        fn overlay_candidates(
            &self,
            roots: &[std::path::PathBuf],
            names: &[String],
        ) -> Vec<std::path::PathBuf> {
            self.0.overlay_candidates(roots, names)
        }
        fn load(
            &mut self,
            request: pascal_core::SourceRequest<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<pascal_core::LoadedSource, pascal_core::SourceStoreError> {
            let mut source = self.0.load(request, cancel)?;
            source.decoded_text = None;
            Ok(source)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let root_path = dir.path().join("Main.pas");
    std::fs::write(&root_path, "unit Main; interface implementation end.").unwrap();
    // Invalid UTF-8 takes the actual callback's decoded-text cache path, not a
    // synthetic admitted Vec or a provider-fixture source backing.
    std::fs::write(dir.path().join("value.inc"), b"// \xe9\n").unwrap();
    let ctx =
        ProjectContext::discover(&root_path, &[dir.path().into()], &ProjectOptions::default())
            .unwrap()
            .context;
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        RawFileStore(FilesystemSourceStore::new()),
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    // Keep an admitted observer alive so ledger controls have the identical
    // lifetime on both sides of the payload comparison.
    let observer = budget
        .admit_owned(0, 0, 0, || ())
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let baseline = observer.budget_snapshot().retained_bytes;
    let outcome = CompileTimeResolver::include(
        &mut resolver,
        IncludeResolveRequest {
            including_path: &root_path,
            byte_range: 0..0,
            requested_name: "value.inc",
            legacy_route: None,
        },
        &mut budget,
        &NoCancellation,
    )
    .unwrap();
    let Resolution::Found(source) = &outcome.result else {
        panic!("authorized physical include must be found");
    };
    assert!(source.decoded_text.is_some(), "actual decode path required");
    drop(outcome);
    let cache_live = observer.budget_snapshot();
    assert!(
        cache_live.retained_bytes > baseline,
        "dropping the returned include must not refund the resolver's decoded cache"
    );
    drop(resolver);
    drop(budget);
    let cache_dead = observer.budget_snapshot();
    assert_eq!(cache_dead.work, cache_live.work);
    assert_eq!(cache_dead.byte_work, cache_live.byte_work);
    assert_eq!(
        cache_dead.retained_bytes, baseline,
        "the actual decoded cache and returned source are both dead; only admitted observer/control credits remain"
    );
}
