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

#[test]
fn directory_entries_are_charged_for_the_bytes_the_lookup_reads() {
    let small = usage_with_fillers(500);
    let large = usage_with_fillers(1000);
    // Each extra entry is listed (the filesystem store charges its 255-byte
    // name bound per listing) and name-matched a few times; sort-key bounds
    // over the whole path are charged only for matching entries.
    let per_entry = (large.byte_work - small.byte_work) / 500;
    assert!(
        per_entry < 1024,
        "{per_entry} byte work per directory entry"
    );
}

/// l4d-4nbs R8: listing a directory costs per entry; nothing is read from
/// the listed files, so their sizes are not scanned bytes. Projects/Superform
/// holds a 2.1 GB locks.txt, and after four listings of it every later
/// listing in the request was reported as a "directory scan limit".
fn directory_with_a_huge_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    std::fs::write(&path, ROOT).unwrap();
    std::fs::write(
        dir.path().join("U.pas"),
        "unit U; interface const Marker = 1; implementation end.",
    )
    .unwrap();
    std::fs::write(dir.path().join("chosen.inc"), "type A = Byte;").unwrap();
    // Sparse: 3 GiB of length, no data blocks.
    std::fs::File::create(dir.path().join("locks.txt"))
        .unwrap()
        .set_len(3 * 1024 * 1024 * 1024)
        .unwrap();
    (dir, path)
}

#[test]
fn repeated_listings_of_a_directory_with_a_huge_file_stay_complete() {
    let (dir, path) = directory_with_a_huge_file();
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
    source.path = path.clone();
    for round in 0..5 {
        let analysis = resolver
            .analyze_compile_time(&source, &NoCancellation)
            .unwrap();
        assert!(
            analysis.root.complete,
            "round {round}: {:?}",
            analysis.reasons
        );
        let outcome = resolver
            .try_resolve_unit(
                pascal_core::UnitResolveRequest {
                    requested_name: "u",
                    importer_path: &path,
                    legacy_route: None,
                },
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(outcome.result, pascal_core::Resolution::Found(_)),
            "round {round}: {:?}",
            outcome.result
        );
    }
    let report = resolver.report();
    assert!(
        report.complete,
        "{:?} {:?}",
        report.incomplete_reasons, report.warnings
    );
}

#[test]
fn the_filesystem_store_lists_a_directory_with_a_huge_file_repeatedly() {
    use pascal_core::{DirectoryRequest, SourceStore};
    let (dir, path) = directory_with_a_huge_file();
    let ctx = ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
        .unwrap()
        .context;
    let directory = dir.path().to_path_buf();
    let entry = pascal_project::ProjectPathEntry {
        path: directory.clone(),
        provenance: pascal_project::ProjectPathProvenance::LegacyNative,
    };
    let policy = ctx.read_policy();
    let mut store = FilesystemSourceStore::new();
    for round in 0..5 {
        let listing = store
            .list_directory(
                DirectoryRequest {
                    directory: &directory,
                    entry: &entry,
                    read_policy: policy,
                },
                &NoCancellation,
            )
            .unwrap();
        assert!(listing.complete, "round {round}");
        assert_eq!(listing.files.len(), 4, "round {round}");
    }
}
