//! l4d-9bxx: one directory lookup is charged for the linear pass it performs,
//! not for a quadratic sort over every entry of the listing.
#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::BudgetSnapshot;
use pascal_core::{FilesystemSourceStore, NoCancellation, ResolverLimits, UnitResolver};
use pascal_project::{LayoutPlatform, ProjectContext, ProjectOptions};
use support::context;

const ROOT: &str =
    "unit T; interface uses U; {$IF U.Marker = 1}{$I Chosen.INC}{$ENDIF} implementation end.";

fn usage_with_fillers(fillers: usize) -> BudgetSnapshot {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    std::fs::write(&path, ROOT).unwrap();
    std::fs::write(
        dir.path().join("U.pas"),
        "unit U; interface const Marker = 1; implementation end.",
    )
    .unwrap();
    std::fs::write(dir.path().join("chosen.inc"), "type A = Byte;").unwrap();
    for index in 0..fillers {
        std::fs::write(
            dir.path()
                .join(format!("Some.Rather.Long.Namespace.Filler{index:05}.pas")),
            "",
        )
        .unwrap();
    }
    let mut ctx = ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
        .unwrap()
        .context;
    let mut parts = std::sync::Arc::unwrap_or_clone(std::mem::take(&mut ctx)).into_parts();
    parts.conditional_context = context(LayoutPlatform::Win32);
    ctx = ProjectContext::from_parts(parts).into();
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut source = support::loaded("T.pas", ROOT);
    source.path = path;
    let analysis = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(
        analysis.root.complete,
        "{fillers} fillers: {:?}",
        analysis.reasons
    );
    resolver.compile_time_usage()
}

#[test]
fn directory_lookup_charges_grow_linearly_with_entry_count() {
    let small = usage_with_fillers(500);
    let large = usage_with_fillers(1000);
    // Doubling the listing must not quadruple the charge.
    assert!(
        large.byte_work < small.byte_work * 5 / 2,
        "byte_work {} -> {}",
        small.byte_work,
        large.byte_work
    );
    assert!(
        large.work < small.work * 5 / 2,
        "work {} -> {}",
        small.work,
        large.work
    );
}

#[test]
fn large_directory_lookup_stays_far_below_session_limits() {
    let limits = pascal_core::compile_time::CompileTimeLimits::default();
    let usage = usage_with_fillers(3000);
    assert!(
        usage.byte_work < limits.max_byte_work / 8,
        "byte_work {} of {}",
        usage.byte_work,
        limits.max_byte_work
    );
    assert!(
        usage.work < limits.max_work / 8,
        "work {} of {}",
        usage.work,
        limits.max_work
    );
}
