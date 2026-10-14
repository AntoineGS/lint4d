use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
use pascal_core::{
    DirectoryRequest, FilesystemSourceStore, IncludeResolveRequest, NoCancellation, Resolution,
    ResolverLimits, SourceKind, SourceRequest, SourceStore, SourceStoreError, UnitResolveRequest,
    UnitResolver,
};
use pascal_project::{ProjectContext, ProjectOptions};

fn edit_context<R>(
    context: &mut pascal_project::ProjectContext,
    edit: impl FnOnce(&mut pascal_project::ProjectContextParts) -> R,
) {
    let mut parts = std::mem::take(context).into_parts();
    edit(&mut parts);
    *context = pascal_project::ProjectContext::from_parts(parts);
}

fn context(root: &std::path::Path, dir: &std::path::Path) -> ProjectContext {
    ProjectContext::discover(root, &[dir.into()], &ProjectOptions::default())
        .unwrap()
        .context
}

#[test]
fn default_store_decode_and_returned_include_outlive_cache_and_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main; interface implementation end.").unwrap();
    std::fs::write(dir.path().join("value.inc"), b"// \xe9\n").unwrap();
    let mut resolver = UnitResolver::new(
        context(&root, dir.path()),
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let observer = budget
        .admit_owned(0, 0, 0, || ())
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let baseline = observer.budget_snapshot().retained_bytes;
    let outcome = CompileTimeResolver::include(
        &mut resolver,
        IncludeResolveRequest {
            including_path: &root,
            byte_range: 0..0,
            requested_name: "value.inc",
            legacy_route: None,
        },
        &mut budget,
        &NoCancellation,
    )
    .unwrap();
    let Resolution::Found(source) = &outcome.result else {
        panic!("physical include required");
    };
    assert!(source.decoded_text.is_some());
    assert!(
        source
            .compile_time_backing
            .as_ref()
            .unwrap()
            .is_admitted_by(&budget)
    );
    assert!(resolver.compile_time_usage().retained_bytes > baseline);
    let live = observer.budget_snapshot();
    drop(resolver);
    drop(budget);
    assert!(
        observer.budget_snapshot().retained_bytes > baseline,
        "returned source still owns backing"
    );
    assert!(
        observer.budget_snapshot().retained_bytes < live.retained_bytes,
        "dead cache containers refund separately"
    );
    drop(outcome);
    let dead = observer.budget_snapshot();
    assert_eq!(dead.retained_bytes, baseline);
    assert_eq!(dead.work, live.work);
    assert_eq!(dead.byte_work, live.byte_work);
}

#[test]
fn admitted_directory_and_overlay_growth_refund_after_payload_drop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    std::fs::write(dir.path().join("Other.pas"), "unit Other;").unwrap();
    let ctx = context(&root, dir.path());
    let entry = ctx.path_entry_for(dir.path()).unwrap();
    let mut store = FilesystemSourceStore::new();
    store.insert_overlay(dir.path().join("Overlay.pas"), 1, &b"unit Overlay;"[..]);
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let observer = budget
        .admit_owned(0, 0, 0, || ())
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let baseline = observer.budget_snapshot().retained_bytes;
    let listing = store
        .list_directory_admitted(
            DirectoryRequest {
                directory: dir.path(),
                entry: &entry,
                read_policy: ctx.read_policy(),
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
    assert!(listing.complete);
    assert_eq!(listing.files.len(), 3);
    let overlays = store
        .overlay_candidates_admitted(
            &[dir.path().into()],
            &["Overlay.pas".into()],
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
    assert_eq!(overlays.len(), 1);
    let live = observer.budget_snapshot();
    drop(listing);
    drop(overlays);
    assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
    assert_eq!(observer.budget_snapshot().work, live.work);
    assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
}

#[test]
fn denied_payload_is_rejected_before_root_admission_or_payload_read() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    let path = outside.path().join("Denied.pas");
    std::fs::write(&path, "unit Denied;").unwrap();
    let ctx = context(&root, dir.path());
    let entry = pascal_project::ProjectPathEntry {
        path: path.clone(),
        provenance: pascal_project::ProjectPathProvenance::Configured,
    };
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 0,
            ..Default::default()
        },
        &NoCancellation,
    );
    let result = FilesystemSourceStore::new().load_admitted(
        SourceRequest {
            path: &path,
            entry: &entry,
            read_policy: ctx.read_policy(),
            legacy_route: None,
            kind: SourceKind::Unit,
            max_bytes: 1024,
        },
        &mut budget,
        &NoCancellation,
    );
    assert!(matches!(
        result,
        Err(SourceStoreError::AdmissionUnavailable) | Err(SourceStoreError::Unauthorized { .. })
    ));
    assert_eq!(budget.snapshot(), Default::default());
}

#[test]
fn physical_equal_width_candidates_remain_ambiguous_and_dcu_is_not_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    std::fs::write(
        dir.path().join("P.pas"),
        "unit P; interface type T=Byte; implementation end.",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("p.pas"),
        "unit P; interface type T=Byte; implementation end.",
    )
    .unwrap();
    std::fs::write(dir.path().join("OnlyDcu.dcu"), b"not Pascal source").unwrap();
    let mut resolver = UnitResolver::new(
        context(&root, dir.path()),
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let ambiguous = resolver
        .unit_capability(
            UnitResolveRequest {
                requested_name: "P",
                importer_path: &root,
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
    assert!(
        matches!(ambiguous.result, Resolution::Ambiguous { ref candidates } if candidates.len() == 2)
    );
    let unavailable = resolver
        .unit_capability(
            UnitResolveRequest {
                requested_name: "OnlyDcu",
                importer_path: &root,
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
    assert!(matches!(unavailable.result, Resolution::Unavailable { .. }));
    assert!(!unavailable.observations.iter().any(|o| matches!(o,
        pascal_core::ResolutionObservation::Payload { path, .. } if path.extension().is_some_and(|e| e == "dcu"))));
}

#[test]
fn fresh_root_reloads_provider_revision_without_foreign_receipt_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    let path = dir.path().join("P.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    std::fs::write(&path, "unit P; interface type T=Byte; implementation end.").unwrap();
    let mut resolver = UnitResolver::new(
        context(&root, dir.path()),
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut first = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let old = resolver
        .unit_capability(
            UnitResolveRequest {
                requested_name: "P",
                importer_path: &root,
                legacy_route: None,
            },
            &mut first,
            &NoCancellation,
        )
        .unwrap();
    let Resolution::Found(old) = old.result else {
        panic!("first positive capability required");
    };
    std::fs::write(&path, "unit P; interface type T=Int64; implementation end.").unwrap();
    let mut second = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let new = resolver
        .unit_capability(
            UnitResolveRequest {
                requested_name: "P",
                importer_path: &root,
                legacy_route: None,
            },
            &mut second,
            &NoCancellation,
        )
        .unwrap();
    let Resolution::Found(new) = new.result else {
        panic!("fresh positive capability required");
    };
    assert_ne!(old.unit.source.revision, new.unit.source.revision);
    assert!(!old.is_admitted_by(&second));
    assert!(new.is_admitted_by(&second));
    assert!(
        new.unit
            .source
            .analysis_bytes()
            .windows(5)
            .any(|b| b == b"Int64")
    );
}

#[test]
fn cancellation_during_incremental_directory_construction_refunds_partial_payload() {
    struct Cancel(std::sync::atomic::AtomicUsize);
    impl pascal_core::CancellationToken for Cancel {
        fn is_cancelled(&self) -> bool {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 8
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    for n in 0..12 {
        std::fs::write(dir.path().join(format!("P{n}.pas")), "unit P;").unwrap();
    }
    let ctx = context(&root, dir.path());
    let entry = ctx.path_entry_for(dir.path()).unwrap();
    let cancel = Cancel(std::sync::atomic::AtomicUsize::new(0));
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &cancel);
    let observer = budget
        .admit_owned(0, 0, 0, || ())
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let baseline = observer.budget_snapshot();
    let result = FilesystemSourceStore::new().list_directory_admitted(
        DirectoryRequest {
            directory: dir.path(),
            entry: &entry,
            read_policy: ctx.read_policy(),
        },
        &mut budget,
        &cancel,
    );
    assert!(matches!(result, Err(SourceStoreError::Cancelled)));
    let after = observer.budget_snapshot();
    assert_eq!(after.retained_bytes, baseline.retained_bytes);
    assert!(after.work > baseline.work);
    assert!(after.byte_work > baseline.byte_work);
}

#[test]
fn actual_filesystem_provider_layout_is_positive_and_retains_payload_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    let provider = dir.path().join("P.pas");
    std::fs::write(
        &root,
        "unit Main; interface uses P; {$IF SizeOf(P.T)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
    )
    .unwrap();
    std::fs::write(
        &provider,
        "unit P; interface type T=Int64; implementation end.",
    )
    .unwrap();
    let mut ctx = context(&root, dir.path());
    edit_context(&mut ctx, |p| {
        p.conditional_context = pascal_project::ConditionalContext::default()
            .with_compiler_version(pascal_project::CompilerVersion::new(37, 0))
            .with_layout(pascal_project::LayoutContext {
                platform: Some(pascal_project::LayoutPlatform::Win32),
                defaults: pascal_project::LayoutSettings::default(),
            })
    });
    edit_context(&mut ctx, |p| {
        p.library_conditional_context = p.conditional_context.clone()
    });
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let loaded = resolver
        .load_source(&root, None, SourceKind::Unit, &NoCancellation)
        .unwrap();
    let analysis = resolver
        .analyze_compile_time(&loaded, &NoCancellation)
        .unwrap();
    assert_eq!(
        analysis
            .root
            .directives
            .iter()
            .find(|d| d.body == "DEFINE HIT")
            .unwrap()
            .activity,
        pascal_core::conditional::Truth::True
    );
    assert!(analysis.observations.iter().any(|o| matches!(o,
        pascal_core::ResolutionObservation::Payload { path, .. } if path == &provider)));
    let live = resolver.compile_time_usage();
    drop(analysis);
    let after = resolver.compile_time_usage();
    assert!(after.retained_bytes < live.retained_bytes);
    assert!(
        after.retained_bytes > 0,
        "actual admitted source/cache owners remain"
    );
    assert_eq!(after.work, live.work);
    assert_eq!(after.byte_work, live.byte_work);
}

#[test]
fn provider_metadata_drop_never_leaves_unowned_context_or_qualifier_credits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    std::fs::write(
        dir.path().join("P.pas"),
        "unit P; interface type T=Byte; implementation end.",
    )
    .unwrap();
    let mut resolver = UnitResolver::new(
        context(&root, dir.path()),
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let observer = budget
        .admit_owned(0, 0, 0, || ())
        .unwrap()
        .into_shared(&mut budget)
        .unwrap();
    let baseline = observer.budget_snapshot().retained_bytes;
    let outcome = resolver
        .unit_capability(
            UnitResolveRequest {
                requested_name: "P",
                importer_path: &root,
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
    assert!(matches!(outcome.result, Resolution::Found(_)));
    drop(outcome);
    let cache_live = observer.budget_snapshot();
    assert!(cache_live.retained_bytes > baseline);
    drop(resolver);
    drop(budget);
    let dead = observer.budget_snapshot();
    assert_eq!(dead.retained_bytes, baseline);
    assert_eq!(dead.work, cache_live.work);
    assert_eq!(dead.byte_work, cache_live.byte_work);
}
