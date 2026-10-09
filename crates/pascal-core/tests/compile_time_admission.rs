//! Allocation observations are per-thread and only cover the tested call.
use pascal_core::{
    FilesystemSourceStore, ImportSection, ImportSite, NoCancellation, ResolutionObservation,
    ResolverError, ResolverLimits, UnitResolver,
};
use pascal_project::{ProjectContext, ProjectOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct ObservedAllocator;
thread_local! {
    static OBSERVE: Cell<(usize, usize)> = const { Cell::new((usize::MAX, 0)) };
    static EXACT: Cell<(usize,usize)> = const { Cell::new((usize::MAX,0)) };
}
#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;
fn observe(size: usize) {
    let _ = EXACT.try_with(|cell| {
        let (wanted, count) = cell.get();
        if size == wanted {
            cell.set((wanted, count + 1));
        }
    });
    let _ = OBSERVE.try_with(|cell| {
        let (minimum, count) = cell.get();
        if size >= minimum {
            cell.set((minimum, count + 1));
        }
    });
}
// SAFETY: delegates every allocation/deallocation, with its original layout,
// to System; the observation has no effect on pointers or allocator ownership.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        observe(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        observe(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
fn allocations_at_least<T>(minimum: usize, call: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            OBSERVE.with(|c| c.set((usize::MAX, 0)));
        }
    }
    let _reset = Reset;
    OBSERVE.with(|c| c.set((minimum, 0)));
    let result = call();
    (result, OBSERVE.with(|c| c.get().1))
}

#[path = "support/compile_time.rs"]
mod support;

#[test]
fn include_r18_first_hit_has_no_loaded_source_candidate_vec_allocation() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
    use pascal_core::{IncludeResolveRequest, LoadedSource};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    std::fs::write(dir.path().join("body.inc"), "{body}").unwrap();
    let context =
        ProjectContext::discover(&root, &[dir.path().into()], &Default::default()).unwrap();
    let mut resolver = UnitResolver::new(
        context,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let request = IncludeResolveRequest {
        including_path: &root,
        byte_range: 0..0,
        requested_name: "body.inc",
        legacy_route: None,
    };
    let warm = resolver
        .include_capability(request.clone(), &mut budget, &NoCancellation)
        .unwrap();
    drop(warm);
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            EXACT.with(|cell| cell.set((usize::MAX, 0)));
        }
    }
    let reset = Reset;
    EXACT.with(|cell| cell.set((4 * std::mem::size_of::<LoadedSource>(), 0)));
    let out = resolver
        .include_capability(request, &mut budget, &NoCancellation)
        .unwrap();
    let allocations = EXACT.with(|cell| cell.get().1);
    drop(reset);
    assert!(
        matches!(out.result(),pascal_core::Resolution::Found(source) if source.path.ends_with("body.inc"))
    );
    assert_eq!(
        allocations, 0,
        "first-hit include must not allocate the legacy four-slot LoadedSource candidate Vec"
    );
}

#[test]
fn task9_b_receipt_control_refuses_before_first_ledger_or_payload_allocation() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, IncompleteReason};
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 0,
            ..Default::default()
        },
        &NoCancellation,
    );
    let (result, allocations) = allocations_at_least(1, || {
        budget.admit_owned(1, 10_000, 10_000, || vec![0u8; 10_000])
    });
    assert!(matches!(
        result,
        Err(IncompleteReason::Limit {
            name: "max_retained_bytes",
            ..
        })
    ));
    assert_eq!(
        allocations, 0,
        "even receipt/control allocation must follow admission"
    );
    assert_eq!(budget.snapshot().retained_bytes, 0);
}

#[test]
fn task9_b_shared_arc_alignment_control_refuses_before_backing_allocation() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, IncompleteReason};
    #[derive(Debug)]
    #[repr(align(4096))]
    struct Aligned;
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 6000,
            ..Default::default()
        },
        &NoCancellation,
    );
    let value = budget.admit_owned(1, 0, 0, || Aligned).unwrap();
    let (result, allocations) = allocations_at_least(8192, || value.into_shared(&mut budget));
    assert!(matches!(
        result,
        Err(IncompleteReason::Limit {
            name: "max_retained_bytes",
            ..
        })
    ));
    assert_eq!(
        allocations, 0,
        "Arc control padding must be admitted before allocation"
    );
    assert!(budget.snapshot().retained_bytes < 256);
}

#[test]
fn task9_b_overlay_directory_refuses_before_unrelated_path_copy() {
    use pascal_core::UnitResolveRequest;
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Root.pas");
    std::fs::write(&root, "unit Root; interface implementation end.").unwrap();
    let ctx =
        ProjectContext::discover(&root, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    let mut store = FilesystemSourceStore::new();
    store.insert_overlay(dir.path().join("X".repeat(100_000)), 1, &b"ignored"[..]);
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        store,
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 8_000,
            ..Default::default()
        },
        &NoCancellation,
    );
    let (result, allocations) = allocations_at_least(100_000, || {
        CompileTimeResolver::unit(
            &mut resolver,
            UnitResolveRequest {
                requested_name: "Missing",
                importer_path: &root,
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
    });
    assert_eq!(
        allocations, 0,
        "directory overlay names must be admitted before path clones"
    );
    assert!(
        matches!(
            result,
            Err(ResolverError::LimitExceeded {
                limit: "max_retained_bytes",
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn task9_b_provider_payload_refuses_before_physical_read_or_decode() {
    use pascal_core::UnitResolveRequest;
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Root.pas");
    std::fs::write(&root, "unit Root; interface implementation end.").unwrap();
    std::fs::write(
        dir.path().join("Provider.pas"),
        format!(
            "unit Provider; interface {{ {} }} implementation end.",
            "X".repeat(100_000)
        ),
    )
    .unwrap();
    let ctx =
        ProjectContext::discover(&root, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 8_000,
            ..Default::default()
        },
        &NoCancellation,
    );
    let (result, allocations) = allocations_at_least(100_000, || {
        CompileTimeResolver::unit(
            &mut resolver,
            UnitResolveRequest {
                requested_name: "Provider",
                importer_path: &root,
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
    });
    assert_eq!(
        allocations, 0,
        "payload read/decode must follow shared root admission"
    );
    assert!(
        matches!(
            result,
            Err(ResolverError::LimitExceeded {
                limit: "max_retained_bytes",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(
        !resolver
            .report()
            .observations
            .iter()
            .any(|o| matches!(o, ResolutionObservation::Payload { .. }))
    );
}

#[test]
fn fix2_i3a_zero_retained_long_unknown_keyword_does_not_allocate_classifier_scratch() {
    let text = format!(
        "unit T; interface {{$ {} }} implementation end.",
        "X".repeat(100_000)
    );
    let source = support::loaded("T.pas", &text);
    let importer = pascal_core::ResolvedUnit {
        requested_name: "T".into(),
        declared_name: "T".into(),
        source,
    };
    let mut resolver = UnitResolver::new(
        ProjectContext::default(),
        vec![],
        FilesystemSourceStore::new(),
        ResolverLimits {
            max_compile_time_retained_bytes: 0,
            ..Default::default()
        },
    );
    let (result, allocations) = allocations_at_least(100_000, || {
        resolver.resolve_imports_with_text(&importer, &[], &text, &NoCancellation)
    });
    assert!(matches!(result, Err(ResolverError::LimitExceeded { .. })));
    assert_eq!(
        allocations, 0,
        "classifier must borrow the keyword before retained refusal"
    );
    assert_eq!(resolver.compile_time_usage().retained_bytes, 0);
    assert!(resolver.report().observations.is_empty());
}

#[test]
fn fix2_i3b_large_caller_sites_refuse_before_clones_or_child_provider_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    std::fs::write(
        &path,
        "unit T; interface {$I forbidden.inc} implementation end.",
    )
    .unwrap();
    std::fs::write(dir.path().join("forbidden.inc"), "{forbidden}").unwrap();
    let ctx =
        ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits {
            max_compile_time_retained_bytes: 64_000,
            ..Default::default()
        },
    );
    let sites = (0..16)
        .map(|_| ImportSite {
            requested_name: "P".repeat(100_000),
            byte_range: 0..1,
            section: ImportSection::Interface,
        })
        .collect::<Vec<_>>();
    let (result, allocations) = allocations_at_least(100_000, || {
        resolver.resolve_project_from_source(&path, "T", &sites, None, &NoCancellation)
    });
    assert!(matches!(result, Err(ResolverError::LimitExceeded { .. })));
    assert_eq!(
        allocations, 0,
        "queued input/name copies must be admitted before cloning"
    );
    let report = resolver.report();
    assert!(
        !report
            .observations
            .iter()
            .any(|o| matches!(o, ResolutionObservation::Directory { .. })
                || matches!(o,
        ResolutionObservation::Candidate {path:p,..} if p != &path)),
        "queue refusal must precede child/provider lookup, not only payload loading"
    );
    let payloads = report
        .observations
        .iter()
        .filter_map(|o| match o {
            ResolutionObservation::Payload { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads, vec![path]);
    assert!(!report.complete);
    assert_eq!(resolver.compile_time_usage().retained_bytes, 0);
}

#[test]
fn fix2_i3b_cached_parsed_root_sites_refuse_before_vector_copy() {
    use pascal_core::{Resolution, UnitResolveRequest};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    let names = (0..2000).map(|n| format!("P{n:07}")).collect::<Vec<_>>();
    let text = format!(
        "unit T; interface uses {}; implementation end.",
        names.join(",")
    );
    std::fs::write(&path, &text).unwrap();
    let ctx =
        ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits {
            max_compile_time_retained_bytes: 64_000,
            ..Default::default()
        },
    );
    // Warm real parsed metadata without walking any provider/import.
    assert!(matches!(
        resolver
            .resolve_unit(
                UnitResolveRequest {
                    requested_name: "T",
                    importer_path: &path,
                    legacy_route: None
                },
                &NoCancellation
            )
            .result,
        Resolution::Found(_)
    ));
    let before = resolver.report().observations.len();
    let (result, allocations) =
        allocations_at_least(names.len() * std::mem::size_of::<ImportSite>(), || {
            resolver.resolve_project_from_source(&path, "T", &[], None, &NoCancellation)
        });
    assert!(matches!(result, Err(ResolverError::LimitExceeded { .. })));
    assert_eq!(
        allocations, 0,
        "cached metadata entries must be admitted before queue vector copy"
    );
    let report = resolver.report();
    assert!(!report.complete);
    assert!(!report.observations[before..].iter().any(|o| matches!(
        o,
        ResolutionObservation::Directory { .. }
    ) || matches!(o,
        ResolutionObservation::Candidate {path:p,..} if p != &path)));
    assert!(
        report.observations[before..]
            .iter()
            .all(|o| !matches!(o, ResolutionObservation::Payload {path:p,..} if p != &path))
    );
    assert_eq!(resolver.compile_time_usage().retained_bytes, 0);
}
