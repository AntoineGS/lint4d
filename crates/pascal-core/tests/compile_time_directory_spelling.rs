//! Functional directory spelling lookup: listing a symlink is not a read grant.
#[test]
fn short_directory_names_retire_unused_filename_reservation() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits};
    use pascal_core::{DirectoryRequest, FilesystemSourceStore, NoCancellation, SourceStore};
    use pascal_project::{ProjectContext, ProjectOptions};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    for n in 0..128 {
        std::fs::write(temp.path().join(format!("n{n}")), "").unwrap();
    }
    let context =
        ProjectContext::discover(&root, &[temp.path().into()], &ProjectOptions::default()).unwrap();
    let entry = context.path_entry_for(temp.path()).unwrap();
    let request = DirectoryRequest {
        directory: temp.path(),
        entry: &entry,
        read_policy: &context.read_policy,
    };
    let mut store = FilesystemSourceStore::new();
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: 32_768,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    let listing = store
        .list_directory_admitted(request, &mut budget, &NoCancellation)
        .unwrap();
    assert!(listing.complete);
    assert_eq!(listing.files.len(), 129);
    assert!(budget.snapshot().retained_bytes < 32_768);
}
#[cfg(unix)]
#[test]
fn admitted_spelling_listing_keeps_symlink_names_without_following_targets() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits};
    use pascal_core::{DirectoryRequest, FilesystemSourceStore, NoCancellation, SourceStore};
    use pascal_project::{ProjectContext, ProjectOptions};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Main.pas");
    std::fs::write(&root, "unit Main;").unwrap();
    let link = temp.path().join("Missing.inc");
    std::os::unix::fs::symlink("does-not-exist.inc", &link).unwrap();
    let context =
        ProjectContext::discover(&root, &[temp.path().into()], &ProjectOptions::default()).unwrap();
    let entry = context.path_entry_for(temp.path()).unwrap();
    let request = DirectoryRequest {
        directory: temp.path(),
        entry: &entry,
        read_policy: &context.read_policy,
    };
    let mut store = FilesystemSourceStore::new();
    let ordinary = store.list_directory(request, &NoCancellation).unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let admitted = store
        .list_directory_admitted(request, &mut budget, &NoCancellation)
        .unwrap();
    assert!(ordinary.complete);
    assert!(
        admitted.complete,
        "a name-only symlink must not make unrelated spelling lookups incomplete"
    );
    assert!(admitted.files.contains(&link));
    assert!(!link.exists(), "the dangling target was not materialized");
}
