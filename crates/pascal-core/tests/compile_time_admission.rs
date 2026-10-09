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
}
#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;
fn observe(size: usize) {
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
