use pascal_core::resolver::{
    CancellationToken, DirectoryListing, DirectoryRequest, FilesystemSourceStore, LoadedSource,
    NoCancellation, Resolution, ResolverError, ResolverLimits, SourceId, SourceKind, SourceRequest,
    SourceRevision, SourceStore, SourceStoreError, UnitResolveRequest, UnitResolver,
};
use pascal_project::{
    ProjectContext, ProjectPathEntry, ProjectPathProvenance, ReadPolicy, content_hash_bytes,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[test]
fn legacy_disk_source_uses_decoded_analysis_bytes_but_keeps_raw_revision_payload() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("Main.pas");
    let raw = b"unit Main;\n// caf\xe9\ninterface implementation end.\n";
    fs::write(&path, raw).unwrap();
    let context = ProjectContext {
        discovery_complete: true,
        read_policy: ReadPolicy::new(&[temp.path().to_path_buf()], &[], &[], &Default::default()),
        ..ProjectContext::default()
    };
    let mut resolver = UnitResolver::new(
        context,
        vec![temp.path().to_path_buf()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let loaded = resolver
        .load_source(&path, None, SourceKind::Unit, &NoCancellation)
        .unwrap();

    assert_eq!(&*loaded.bytes, raw);
    assert_eq!(
        loaded.decoded_text.as_deref(),
        Some("unit Main;\n// café\ninterface implementation end.\n")
    );
    assert_eq!(
        loaded.analysis_bytes().as_ref(),
        "unit Main;\n// café\ninterface implementation end.\n".as_bytes()
    );
    match loaded.revision {
        SourceRevision::Disk { content_hash, .. } => {
            assert_eq!(content_hash, content_hash_bytes(raw));
        }
        other => panic!("expected disk revision, got {other:?}"),
    }
}

#[test]
fn unknown_project_define_keeps_include_potentially_active() {
    let analysis = pascal_core::conditional::analyze_with_cancel(
        "{$IFDEF FEATURE}{$I body.inc}{$ENDIF}",
        &[],
        &NoCancellation,
    );
    assert!(analysis.complete);
    assert_eq!(analysis.directives.len(), 3);
    assert!(analysis.directives.iter().any(|directive| {
        directive.kind == pascal_core::conditional::DirectiveKind::Include
            && directive.activity == pascal_core::conditional::Truth::Unknown
            && directive.potentially_active()
    }));
}

#[test]
fn known_inactive_include_is_not_potentially_active() {
    let analysis = pascal_core::conditional::analyze_with_cancel(
        "{$IFNDEF FEATURE}{$I body.inc}{$ENDIF}",
        &["FEATURE".to_string()],
        &NoCancellation,
    );
    assert!(analysis.complete);
    assert!(analysis.directives.iter().any(|directive| {
        directive.kind == pascal_core::conditional::DirectiveKind::Include
            && !directive.potentially_active()
    }));
}

#[test]
fn explicit_mapping_wins_and_alias_is_applied_once() {
    let context = fixture_context_with_alias("compat", "Vendor.Errors");
    let mut resolver = fixture_resolver(context);
    let result = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Compat",
            importer_path: Path::new("/workspace/App.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    let found = match result.result {
        Resolution::Found(unit) => unit,
        other => panic!("expected explicit alias target, got {other:?}"),
    };
    assert_eq!(found.declared_name, "Vendor.Errors");
}

#[test]
fn ambiguity_at_current_tier_blocks_search_path_fallback() {
    let mut context = fixture_context();
    context.search_path_entries = vec![
        ProjectPathEntry::legacy(PathBuf::from("/workspace/App")),
        ProjectPathEntry::legacy(PathBuf::from("/workspace/Later")),
    ];
    context.search_paths = context
        .search_path_entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    let loads = Arc::new(Mutex::new(Vec::new()));
    let mut store = MemoryStore::with_log(loads.clone());
    store.add(
        "/workspace/App/Errors.pas",
        "unit Errors; interface implementation end.",
    );
    store.add(
        "/workspace/App/errors.PAS",
        "unit Errors; interface implementation end.",
    );
    store.add(
        "/workspace/Later/Errors.pas",
        "unit Errors; interface implementation end.",
    );
    let mut resolver = UnitResolver::new(
        context,
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let result = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: Path::new("/workspace/App/Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(matches!(result.result, Resolution::Ambiguous { .. }));
    assert!(
        !loads
            .lock()
            .expect("load log")
            .iter()
            .any(|path| path == "/workspace/Later/Errors.pas")
    );
}

#[test]
fn namespace_candidates_are_tried_only_for_unqualified_names() {
    let mut context = fixture_context();
    context.unit_namespaces = vec!["Alpha".to_string(), "Beta".to_string()];
    let loads = Arc::new(Mutex::new(Vec::new()));
    let mut store = MemoryStore::with_log(loads.clone());
    store.add(
        "/workspace/Alpha.Errors.pas",
        "unit Alpha.Errors; interface implementation end.",
    );
    store.add(
        "/workspace/Beta.Errors.pas",
        "unit Beta.Errors; interface implementation end.",
    );
    store.add(
        "/workspace/Alpha.Vendor.Errors.pas",
        "unit Alpha.Vendor.Errors; interface implementation end.",
    );
    let mut resolver = UnitResolver::new(
        context,
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let short = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: Path::new("/workspace/App.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    let short_found = match short.result {
        Resolution::Found(unit) => unit,
        other => panic!("expected first namespace tier to win, got {other:?}"),
    };
    assert_eq!(short_found.declared_name, "Alpha.Errors");
    let loaded_after_short = loads.lock().expect("load log").clone();
    assert!(
        loaded_after_short
            .iter()
            .any(|path| path == "/workspace/Alpha.Errors.pas")
    );
    assert!(
        !loaded_after_short
            .iter()
            .any(|path| path == "/workspace/Beta.Errors.pas")
    );

    let before_dotted = loads.lock().expect("load log").len();
    let dotted = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Vendor.Errors",
            importer_path: Path::new("/workspace/App.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(matches!(
        dotted.result,
        Resolution::Found(_) | Resolution::Unavailable { .. }
    ));
    let load_log = loads.lock().expect("load log");
    let dotted_loads = &load_log[before_dotted..];
    assert!(
        !dotted_loads
            .iter()
            .any(|path| path.contains("Alpha.Vendor.Errors"))
    );
}

#[test]
fn declared_name_mismatch_is_not_a_unit_match() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Errors.pas",
        "unit Different.Errors; interface implementation end.",
    );
    let mut resolver = UnitResolver::new(
        fixture_context(),
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let result = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: Path::new("/workspace/App.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(matches!(result.result, Resolution::Unavailable { .. }));
}

#[test]
fn project_graph_retains_active_include_payload_and_occurrence() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$I body.inc} implementation end.",
    );
    store.add("/workspace/body.inc", "const IncludedValue = 1;");
    let mut resolver = UnitResolver::new(
        fixture_context(),
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &NoCancellation,
        )
        .expect("project graph");

    assert!(project.complete);
    assert_eq!(project.includes.len(), 1);
    assert!(matches!(
        project.includes[0].target,
        pascal_core::ResolutionTarget::Found(_)
    ));
    assert_eq!(project.include_sources.len(), 1);
    assert_eq!(
        project.include_sources[0].path,
        PathBuf::from("/workspace/body.inc")
    );
}

#[test]
fn package_lookup_prefers_dpk_descriptor_over_dproj() {
    let directory = tempdir().expect("package fixture");
    let root = directory.path();
    let dpk = root.join("Core.dpk");
    let dproj = root.join("Core.dproj");
    let unit = root.join("pkg/Errors.pas");
    fs::create_dir_all(unit.parent().expect("unit parent")).expect("unit directory");
    fs::write(
        &dpk,
        "package Core; contains Errors in 'pkg/Errors.pas'; end.",
    )
    .expect("dpk");
    fs::write(
        &dproj,
        "<Project><PropertyGroup><MainSource>Core.dpk</MainSource></PropertyGroup></Project>",
    )
    .expect("dproj");
    fs::write(&unit, "unit Errors; interface implementation end.").expect("unit");

    let entry = ProjectPathEntry::legacy(root.to_path_buf());
    let context = ProjectContext {
        discovery_complete: true,
        project_file: Some(root.join("App.dproj")),
        packages: vec!["Core".to_string()],
        search_paths: vec![root.to_path_buf()],
        search_path_entries: vec![entry],
        read_policy: ReadPolicy::default(),
        ..ProjectContext::default()
    };
    let mut resolver = UnitResolver::new(
        context,
        vec![root.to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let result = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("App.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    let found = match result.result {
        Resolution::Found(unit) => unit,
        other => panic!("expected package unit, got {other:?}"),
    };
    assert_eq!(found.source.path, unit);
}

#[test]
fn missing_active_include_is_an_incomplete_project_target() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$I missing.inc} implementation end.",
    );
    let mut resolver = UnitResolver::new(
        fixture_context(),
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &NoCancellation,
        )
        .expect("project graph");

    assert!(!project.complete);
    assert!(matches!(
        project.includes[0].target,
        pascal_core::ResolutionTarget::Incomplete
    ));
}

#[test]
fn unsupported_active_directive_blocks_include_precision() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$UNSUPPORTED_DIRECTIVE} {$I body.inc} implementation end.",
    );
    store.add("/workspace/body.inc", "const IncludedValue = 1;");
    let mut resolver = UnitResolver::new(
        fixture_context(),
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &NoCancellation,
        )
        .expect("project graph");

    assert!(!project.complete);
    assert!(matches!(
        project.includes[0].target,
        pascal_core::ResolutionTarget::Incomplete
    ));
    assert!(project.include_sources.is_empty());
}

#[test]
fn include_file_limit_is_checked_before_loading_payload() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$I body.inc} implementation end.",
    );
    store.add("/workspace/body.inc", "const IncludedValue = 1;");
    let loads = store.loads.clone();
    let limits = ResolverLimits {
        max_include_files: 0,
        ..ResolverLimits::default()
    };
    let mut resolver = UnitResolver::new(
        fixture_context(),
        vec![PathBuf::from("/workspace")],
        store,
        limits,
    );

    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &NoCancellation,
        )
        .expect("project graph");

    assert!(!project.complete);
    assert!(matches!(
        project.includes[0].target,
        pascal_core::ResolutionTarget::Incomplete
    ));
    assert!(
        !loads
            .lock()
            .expect("load log")
            .iter()
            .any(|path| path == "/workspace/body.inc")
    );
}

#[test]
fn cancelled_project_returns_cancelled_error() {
    let cancel = AtomicBool::new(true);
    let mut resolver = UnitResolver::new(
        fixture_context(),
        vec![PathBuf::from("/workspace")],
        MemoryStore::default(),
        Default::default(),
    );

    assert!(matches!(
        resolver.resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &cancel,
        ),
        Err(ResolverError::Cancelled)
    ));
}

#[test]
fn cancelled_cached_unit_lookup_is_not_precise() {
    let mut store = MemoryStore::default();
    store.add("/workspace/A.pas", "unit A; interface implementation end.");
    let mut resolver = UnitResolver::new(review_like_context(), vec![], store, Default::default());
    let request = || UnitResolveRequest {
        requested_name: "A",
        importer_path: Path::new("/workspace/Main.pas"),
        legacy_route: None,
    };

    assert!(matches!(
        resolver
            .try_resolve_unit(request(), &NoCancellation)
            .expect("initial lookup")
            .result,
        Resolution::Found(_)
    ));
    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        resolver.try_resolve_unit(request(), &cancelled),
        Err(ResolverError::Cancelled)
    ));
    assert!(!resolver.finish().complete);
}

#[test]
fn unproven_importer_directory_does_not_gain_legacy_read_authority() {
    let allowed = tempdir().expect("allowed root");
    let outside = tempdir().expect("outside root");
    fs::write(
        outside.path().join("Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("outside unit");
    let mut resolver = UnitResolver::new(
        configured_disk_context(allowed.path()),
        vec![allowed.path().to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &outside.path().join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(!matches!(outcome.result, Resolution::Found(_)));
    assert!(!resolver
        .finish()
        .observations
        .iter()
        .any(|observation| matches!(observation, pascal_core::ResolutionObservation::Payload { path, .. } if path.starts_with(outside.path()))));
}

#[test]
fn unproven_include_owner_does_not_gain_legacy_read_authority() {
    let allowed = tempdir().expect("allowed root");
    let outside = tempdir().expect("outside root");
    fs::write(outside.path().join("body.inc"), "const Outside = 1;").expect("outside include");
    let mut resolver = UnitResolver::new(
        configured_disk_context(allowed.path()),
        vec![allowed.path().to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &outside.path().join("Main.pas"),
            byte_range: 0..1,
            requested_name: "body.inc",
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(!matches!(outcome.result, Resolution::Found(_)));
    assert!(!resolver
        .finish()
        .observations
        .iter()
        .any(|observation| matches!(observation, pascal_core::ResolutionObservation::Payload { path, .. } if path.starts_with(outside.path()))));
}

#[test]
fn unproven_catalogue_root_does_not_gain_legacy_read_authority() {
    let allowed = tempdir().expect("allowed root");
    let outside = tempdir().expect("outside root");
    fs::write(
        outside.path().join("Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("outside unit");
    let mut context = configured_disk_context(allowed.path());
    context.project_file = None;
    let mut resolver = UnitResolver::new(
        context,
        vec![outside.path().to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &outside.path().join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(!matches!(outcome.result, Resolution::Found(_)));
    assert!(!resolver
        .finish()
        .observations
        .iter()
        .any(|observation| matches!(observation, pascal_core::ResolutionObservation::Payload { path, .. } if path.starts_with(outside.path()))));
}

#[test]
fn untagged_search_paths_do_not_gain_legacy_read_authority() {
    let allowed = tempdir().expect("allowed root");
    let outside = tempdir().expect("outside root");
    fs::write(
        outside.path().join("Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("outside unit");
    let mut context = configured_disk_context(allowed.path());
    context.search_paths = vec![outside.path().to_path_buf()];
    let mut resolver = UnitResolver::new(
        context,
        vec![allowed.path().to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &allowed.path().join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(!matches!(outcome.result, Resolution::Found(_)));
    assert!(!resolver
        .finish()
        .observations
        .iter()
        .any(|observation| matches!(observation, pascal_core::ResolutionObservation::Payload { path, .. } if path.starts_with(outside.path()))));
}

#[test]
fn unproven_package_descriptor_does_not_gain_legacy_read_authority() {
    let allowed = tempdir().expect("allowed root");
    let outside = tempdir().expect("outside root");
    fs::write(
        outside.path().join("Core.dpk"),
        "package Core; contains Errors in 'Errors.pas'; end.",
    )
    .expect("outside package");
    fs::write(
        outside.path().join("Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("outside unit");
    let mut context = configured_disk_context(allowed.path());
    context.packages = vec!["Core".to_string()];
    let mut resolver = UnitResolver::new(
        context,
        vec![outside.path().to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &allowed.path().join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(!matches!(outcome.result, Resolution::Found(_)));
    assert!(!resolver
        .finish()
        .observations
        .iter()
        .any(|observation| matches!(observation, pascal_core::ResolutionObservation::Payload { path, .. } if path.starts_with(outside.path()))));
}

#[test]
fn source_store_rejects_mismatched_directory_and_payload_identity() {
    let allowed = tempdir().expect("allowed root");
    let outside = tempdir().expect("outside root");
    let outside_file = outside.path().join("private.pas");
    fs::write(&outside_file, "unit Private; interface implementation end.")
        .expect("outside source");
    let policy = ReadPolicy::new(
        &[allowed.path().to_path_buf()],
        &[],
        &[],
        &Default::default(),
    );
    let entry = ProjectPathEntry {
        path: allowed.path().to_path_buf(),
        provenance: ProjectPathProvenance::Configured,
    };
    let mut store = FilesystemSourceStore::new();

    assert!(matches!(
        store.list_directory(
            DirectoryRequest {
                directory: outside.path(),
                entry: &entry,
                read_policy: &policy,
            },
            &NoCancellation,
        ),
        Err(SourceStoreError::Unauthorized { .. })
    ));
    let payload_entry = ProjectPathEntry {
        path: allowed.path().join("private.pas"),
        provenance: ProjectPathProvenance::Configured,
    };
    assert!(matches!(
        store.load(
            SourceRequest {
                path: &outside_file,
                entry: &payload_entry,
                read_policy: &policy,
                legacy_route: None,
                kind: pascal_core::SourceKind::Unit,
                max_bytes: 1024,
            },
            &NoCancellation,
        ),
        Err(SourceStoreError::Unauthorized { .. })
    ));
}

#[test]
fn unknown_conditional_without_include_makes_project_incomplete() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$IFDEF UNKNOWN} const X=1; {$ENDIF} implementation end.",
    );
    let project = resolve_memory_project(fixture_context(), store, Default::default());
    assert!(!project.complete);
    assert!(!project.report.complete);
}

#[test]
fn nested_include_does_not_resurrect_parent_project_defines() {
    let mut context = fixture_context();
    context.defines = vec!["FEATURE".to_string()];
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$UNDEF FEATURE}{$I child.inc} implementation end.",
    );
    store.add(
        "/workspace/child.inc",
        "{$IFDEF FEATURE}{$I wrong.inc}{$ENDIF}",
    );
    store.add("/workspace/wrong.inc", "const Wrong = 1;");
    let project = resolve_memory_project(context, store, Default::default());

    assert!(!project.complete);
    assert!(project.includes.iter().any(|include| {
        include.requested_name == "wrong.inc"
            && matches!(include.target, pascal_core::ResolutionTarget::Incomplete)
    }));
}

#[test]
fn repeated_include_occurrences_obey_directive_budget() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$I n0.inc} implementation end.",
    );
    for n in 0..10 {
        store.add(
            &format!("/workspace/n{n}.inc"),
            &format!("{{$I n{}.inc}}{{$I n{}.inc}}", n + 1, n + 1),
        );
    }
    store.add("/workspace/n10.inc", "const X = 1;");
    let project = resolve_memory_project(
        fixture_context(),
        store,
        ResolverLimits {
            max_include_directives: 25,
            ..Default::default()
        },
    );

    assert!(!project.complete);
    assert!(project.includes.len() <= 25);
    assert!(!project.report.complete);
}

#[test]
fn malformed_include_analysis_obeys_the_aggregate_directive_budget() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface {$I bad.inc} implementation end.",
    );
    store.add(
        "/workspace/bad.inc",
        "{$IF True}{$I one.inc}{$I two.inc}{$I three.inc}{$I four.inc}",
    );
    let project = resolve_memory_project(
        review_like_context(),
        store,
        ResolverLimits {
            max_include_directives: 1,
            ..Default::default()
        },
    );

    assert!(!project.complete);
    assert_eq!(project.includes.len(), 1);
    assert!(!project.report.complete);
}

#[test]
fn dependency_limit_does_not_publish_orphan_found_target() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface uses A,B; implementation end.",
    );
    store.add("/workspace/A.pas", "unit A; interface implementation end.");
    store.add("/workspace/B.pas", "unit B; interface implementation end.");
    let project = resolve_memory_project(
        fixture_context(),
        store,
        ResolverLimits {
            max_dependency_units: 1,
            ..Default::default()
        },
    );

    assert_eq!(project.units.len(), 1);
    assert!(project.imports.iter().any(|import| {
        import.site.requested_name == "B"
            && matches!(import.target, pascal_core::ResolutionTarget::Incomplete)
    }));
    assert!(!project.imports.iter().any(|import| {
        matches!(&import.target, pascal_core::ResolutionTarget::Found(id) if !project.units.iter().any(|unit| unit.source.id == *id) && *id != project.root.source.id)
    }));
}

#[test]
fn standalone_imports_retain_dependencies_after_project_resolution() {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface uses A; implementation end.",
    );
    store.add("/workspace/A.pas", "unit A; interface implementation end.");
    let mut resolver = UnitResolver::new(review_like_context(), vec![], store, Default::default());
    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &NoCancellation,
        )
        .expect("project graph");
    let standalone = resolver
        .resolve_imports(
            &project.root,
            &[project.imports[0].site.clone()],
            &NoCancellation,
        )
        .expect("standalone imports");

    assert!(matches!(
        standalone.bindings[0].target,
        pascal_core::ResolutionTarget::Found(_)
    ));
    assert_eq!(standalone.dependencies.len(), 1);
}

#[test]
fn unsafe_package_omission_is_incomplete_before_unique_selection() {
    let directory = tempdir().expect("package root");
    let root = directory.path();
    fs::create_dir_all(root.join("pkg")).expect("package units");
    fs::write(
        root.join("pkg/Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("authorized unit");
    fs::write(
        root.join("Core.dpk"),
        "package Core; contains Errors in 'pkg/Errors.pas', Errors in '../outside/Errors.pas'; end.",
    )
    .expect("package descriptor");
    let mut context = configured_disk_context(root);
    context.packages = vec!["Core".to_string()];
    let mut resolver = UnitResolver::new(
        context,
        vec![root.to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Incomplete { .. }));
    let report = resolver.finish();
    assert!(!report.complete);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("outside authorized read roots"))
    );
}

#[test]
fn unsafe_package_windows_omission_is_incomplete_before_unique_selection() {
    let directory = tempdir().expect("package root");
    let root = directory.path();
    fs::create_dir_all(root.join("pkg")).expect("package units");
    fs::write(
        root.join("pkg/Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("authorized unit");
    fs::write(
        root.join("Core.dpk"),
        "package Core; contains Errors in 'pkg/Errors.pas', Errors in 'C:\\Unmapped\\Errors.pas'; end.",
    )
    .expect("package descriptor");
    let mut context = configured_disk_context(root);
    context.packages = vec!["Core".to_string()];
    let mut resolver = UnitResolver::new(
        context,
        vec![root.to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Incomplete { .. }));
    assert!(!resolver.finish().complete);
}

#[test]
fn authorized_package_overlay_can_supply_a_disk_absent_unit() {
    let directory = tempdir().expect("package root");
    let root = directory.path();
    fs::create_dir(root.join("pkg")).expect("package units");
    fs::write(
        root.join("Core.dpk"),
        "package Core; contains Errors in 'pkg/Errors.pas'; end.",
    )
    .expect("package descriptor");
    let mut context = configured_disk_context(root);
    context.packages = vec!["Core".to_string()];
    let mut store = FilesystemSourceStore::new();
    store.insert_overlay(
        root.join("pkg/Errors.pas"),
        1,
        b"unit Errors; interface implementation end.".to_vec(),
    );
    let mut resolver =
        UnitResolver::new(context, vec![root.to_path_buf()], store, Default::default());

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(
        outcome.result,
        Resolution::Found(unit) if unit.source.path == root.join("pkg/Errors.pas")
    ));
    assert!(resolver.finish().complete);
}

#[test]
fn descendant_overlay_does_not_enter_an_immediate_directory_tier() {
    let directory = tempdir().expect("overlay root");
    let root = directory.path();
    fs::create_dir_all(root.join("later")).expect("later search path");
    fs::write(
        root.join("later/Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("later unit");
    let mut context = configured_disk_context(root);
    context.search_path_entries = vec![ProjectPathEntry {
        path: root.join("later"),
        provenance: ProjectPathProvenance::Configured,
    }];
    let mut store = FilesystemSourceStore::new();
    store.insert_overlay(
        root.join("unsearched/Errors.pas"),
        1,
        b"unit Errors; interface implementation end.".to_vec(),
    );
    let mut resolver =
        UnitResolver::new(context, vec![root.to_path_buf()], store, Default::default());

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(
        outcome.result,
        Resolution::Found(unit) if unit.source.path == root.join("later/Errors.pas")
    ));
}

#[test]
fn qualifiers_name_the_selected_alias_and_declaration_only() {
    let mut context = fixture_context();
    context
        .unit_aliases
        .insert("Compat".to_string(), "Vendor.Errors".to_string());
    context.unit_namespaces = vec!["Alpha".to_string(), "Beta".to_string()];
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Main.pas",
        "unit Main; interface uses Compat; implementation end.",
    );
    store.add(
        "/workspace/Vendor.Errors.pas",
        "unit Vendor.Errors; interface implementation end.",
    );
    let project = resolve_memory_project(context, store, Default::default());

    assert_eq!(
        project.imports[0].authorized_qualifiers,
        ["Compat", "Vendor.Errors", "Errors"]
    );
}

#[test]
fn higher_precedence_match_does_not_scan_failing_lower_directory() {
    let mut context = fixture_context();
    let lower = PathBuf::from("/workspace/lower");
    context
        .search_path_entries
        .push(ProjectPathEntry::legacy(lower.clone()));
    context.search_paths.push(lower.clone());
    let mut store = FailingDirectoryStore {
        inner: MemoryStore::default(),
        failing_directory: lower,
    };
    store.inner.add(
        "/workspace/Errors.pas",
        "unit Errors; interface implementation end.",
    );
    let mut resolver = UnitResolver::new(
        context,
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: Path::new("/workspace/Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Found(_)));
}

#[test]
fn projectless_catalogue_skips_excluded_subtrees_after_local_match() {
    let directory = tempdir().expect("catalogue root");
    let root = directory.path();
    fs::create_dir(root.join(".git")).expect("excluded directory");
    fs::write(
        root.join("Errors.pas"),
        "unit Errors; interface implementation end.",
    )
    .expect("local unit");
    let mut context = configured_disk_context(root);
    context.project_file = None;
    let mut resolver = UnitResolver::new(
        context,
        vec![root.to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Found(_)));
}

#[test]
fn exact_nested_namespace_directory_is_resolved() {
    let directory = tempdir().expect("namespace root");
    let root = directory.path();
    fs::create_dir(root.join("Vendor")).expect("namespace directory");
    fs::write(
        root.join("Vendor/Errors.pas"),
        "unit Vendor.Errors; interface implementation end.",
    )
    .expect("namespace unit");
    let mut resolver = UnitResolver::new(
        configured_disk_context(root),
        vec![root.to_path_buf()],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Vendor.Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Found(_)));
}

#[test]
#[cfg(unix)]
fn exact_restricted_provenance_is_not_downgraded_by_directory_candidates() {
    let directory = tempdir().expect("restricted root");
    let root = directory.path();
    let target = root.join("actual.pas");
    let link = root.join("Errors.pas");
    fs::write(&target, "unit Vendor.Errors; interface implementation end.").expect("target unit");
    std::os::unix::fs::symlink(&target, &link).expect("restricted symlink");
    let restricted = ProjectPathEntry {
        path: link.clone(),
        provenance: ProjectPathProvenance::Configured,
    };
    let mut context = configured_disk_context(root);
    context
        .explicit_unit_entries
        .insert("vendor.errors".to_string(), vec![restricted.clone()]);
    context.unit_namespaces = vec!["Vendor".to_string()];
    context.search_path_entries = vec![ProjectPathEntry::legacy(root.to_path_buf())];
    assert_eq!(context.path_entry_for(&link), Some(restricted));
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let mut resolver = UnitResolver::new(
        context,
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Errors",
            importer_path: &root.join("Main.pas"),
            legacy_route: Some(&route),
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Incomplete { .. }));
    assert!(!resolver.finish().complete);
}

#[test]
#[cfg(unix)]
fn case_adjusted_include_reapplies_exact_restricted_provenance() {
    let directory = tempdir().expect("restricted include root");
    let root = directory.path();
    let target = root.join("actual.pas");
    let link = root.join("Errors.pas");
    fs::write(&target, "unit Vendor.Errors; interface implementation end.")
        .expect("target include");
    std::os::unix::fs::symlink(&target, &link).expect("restricted include symlink");
    let mut context = configured_disk_context(root);
    context.explicit_unit_entries.insert(
        "vendor.errors".to_string(),
        vec![ProjectPathEntry {
            path: link.clone(),
            provenance: ProjectPathProvenance::Configured,
        }],
    );
    context.search_path_entries = vec![ProjectPathEntry::legacy(root.to_path_buf())];
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let mut resolver = UnitResolver::new(
        context,
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &root.join("Main.pas"),
            byte_range: 0..10,
            requested_name: "errors.pas",
            legacy_route: Some(&route),
        },
        &NoCancellation,
    );

    assert!(matches!(outcome.result, Resolution::Incomplete { .. }));
    assert!(!resolver.finish().complete);
}

#[test]
#[cfg(unix)]
fn case_adjusted_mapped_include_retains_mapping_restriction() {
    let directory = tempdir().expect("mapped include root");
    let root = directory.path();
    let target = root.join("actual.inc");
    let link = root.join("Body.inc");
    fs::write(&target, "const Included = 1;").expect("target include");
    std::os::unix::fs::symlink(&target, &link).expect("mapped include symlink");
    let overrides = pascal_project::delphi_overrides::EffectiveOverrides {
        path_mappings: vec![pascal_project::delphi_overrides::PathMapping {
            from: "c:/vendor".into(),
            to: root.to_path_buf(),
            config_file: root.join("local.toml"),
        }],
        ..Default::default()
    };
    let mut context = configured_disk_context(root);
    context.read_policy = ReadPolicy::new(&[root.to_path_buf()], &[], &[], &overrides);
    context.overrides = overrides;
    context.search_path_entries = vec![ProjectPathEntry::legacy(root.to_path_buf())];
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let mut resolver = UnitResolver::new(
        context,
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let outcome = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &root.join("Main.pas"),
            byte_range: 0..10,
            requested_name: "C:\\vendor\\body.inc",
            legacy_route: Some(&route),
        },
        &NoCancellation,
    );

    assert!(
        !matches!(outcome.result, Resolution::Found(_)),
        "mapped include must remain strict after case adjustment: {:?}",
        outcome.result
    );
}

#[test]
fn authorized_parent_relative_and_absolute_include_case_adjustment_is_preserved() {
    let directory = tempdir().expect("include root");
    let root = directory.path();
    fs::create_dir(root.join("src")).expect("source directory");
    fs::create_dir(root.join("shared")).expect("shared directory");
    fs::write(root.join("shared/body.inc"), "const Included = 1;").expect("include");
    let mut resolver = UnitResolver::new(
        configured_disk_context(root),
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let parent_relative = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &root.join("src/Main.pas"),
            byte_range: 0..10,
            requested_name: "../shared/BODY.INC",
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(matches!(
        parent_relative.result,
        Resolution::Found(source) if source.path == root.join("shared/body.inc")
    ));

    let absolute = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &root.join("src/Main.pas"),
            byte_range: 11..21,
            requested_name: &format!("{}/shared/BODY.INC", root.display()),
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(matches!(
        absolute.result,
        Resolution::Found(source) if source.path == root.join("shared/body.inc")
    ));
}

#[test]
#[cfg(unix)]
fn proven_legacy_symlink_route_is_read_but_configured_symlink_is_not() {
    let directory = tempdir().expect("legacy root");
    let root = directory.path();
    let target = root.join("actual.pas");
    let link = root.join("Errors.pas");
    fs::write(&target, "unit Errors; interface implementation end.").expect("target source");
    std::os::unix::fs::symlink(&target, &link).expect("legacy symlink");
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let legacy_entry = ProjectPathEntry::legacy(link.clone());
    let policy = ReadPolicy::new(&[root.to_path_buf()], &[], &[], &Default::default());
    let mut store = FilesystemSourceStore::new();

    assert!(
        store
            .load(
                SourceRequest {
                    path: &link,
                    entry: &legacy_entry,
                    read_policy: &policy,
                    legacy_route: Some(&route),
                    kind: pascal_core::SourceKind::Unit,
                    max_bytes: 1024,
                },
                &NoCancellation,
            )
            .is_ok()
    );
    let configured_entry = ProjectPathEntry {
        path: link.clone(),
        provenance: ProjectPathProvenance::Configured,
    };
    assert!(matches!(
        store.load(
            SourceRequest {
                path: &link,
                entry: &configured_entry,
                read_policy: &policy,
                legacy_route: Some(&route),
                kind: pascal_core::SourceKind::Unit,
                max_bytes: 1024,
            },
            &NoCancellation,
        ),
        Err(SourceStoreError::NotRegularFile { .. }) | Err(SourceStoreError::Unauthorized { .. })
    ));
}

#[test]
#[cfg(unix)]
fn project_walk_preserves_a_proven_legacy_route_for_imports() {
    let directory = tempdir().expect("legacy root");
    let root = directory.path();
    let target = root.join("actual.pas");
    let link = root.join("Errors.pas");
    fs::write(
        root.join("Main.pas"),
        "unit Main; interface uses Errors; implementation end.",
    )
    .expect("main source");
    fs::write(&target, "unit Errors; interface implementation end.").expect("target source");
    std::os::unix::fs::symlink(&target, &link).expect("legacy symlink");
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let mut resolver = UnitResolver::new(
        ProjectContext {
            discovery_complete: true,
            ..ProjectContext::default()
        },
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: &root.join("Main.pas"),
                legacy_route: Some(&route),
            },
            &[],
            &NoCancellation,
        )
        .expect("legacy project graph");

    assert!(project.complete);
    assert_eq!(project.units.len(), 1);
    assert_eq!(project.units[0].declared_name, "Errors");
}

#[test]
#[cfg(unix)]
fn project_walk_preserves_a_proven_legacy_route_for_includes() {
    let directory = tempdir().expect("legacy root");
    let root = directory.path();
    let target = root.join("actual.inc");
    let link = root.join("Link.inc");
    fs::write(
        root.join("Main.pas"),
        "unit Main; interface {$I Link.inc} implementation end.",
    )
    .expect("main source");
    fs::write(&target, "const Included = 1;").expect("target include");
    std::os::unix::fs::symlink(&target, &link).expect("legacy include symlink");
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let mut resolver = UnitResolver::new(
        ProjectContext {
            discovery_complete: true,
            ..ProjectContext::default()
        },
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let project = resolver
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: &root.join("Main.pas"),
                legacy_route: Some(&route),
            },
            &[],
            &NoCancellation,
        )
        .expect("legacy include graph");

    assert!(project.complete);
    assert!(project.includes.iter().any(|include| {
        include.requested_name == "Link.inc"
            && matches!(include.target, pascal_core::ResolutionTarget::Found(_))
    }));
    assert_eq!(project.include_sources.len(), 1);
}

#[test]
#[cfg(unix)]
fn cached_legacy_symlink_payload_cannot_bypass_a_later_strict_lookup() {
    let directory = tempdir().expect("legacy root");
    let root = directory.path();
    let target = root.join("actual.inc");
    let link = root.join("Link.inc");
    fs::write(&target, "const Included = 1;").expect("target include");
    std::os::unix::fs::symlink(&target, &link).expect("legacy include symlink");
    let mut context = ProjectContext {
        discovery_complete: true,
        search_path_entries: vec![ProjectPathEntry::legacy(root.to_path_buf())],
        ..ProjectContext::default()
    };
    context.search_paths = vec![root.to_path_buf()];
    let route = pascal_core::LegacyRoute {
        source_path: root.join("Main.pas"),
        sibling_directory: root.to_path_buf(),
    };
    let mut resolver = UnitResolver::new(
        context,
        vec![],
        FilesystemSourceStore::new(),
        Default::default(),
    );

    let first = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &root.join("Main.pas"),
            byte_range: 0..1,
            requested_name: "Link.inc",
            legacy_route: Some(&route),
        },
        &NoCancellation,
    );
    assert!(matches!(first.result, Resolution::Found(_)));

    let second = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: &root.join("Other.pas"),
            byte_range: 0..1,
            requested_name: "Link.inc",
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(!matches!(second.result, Resolution::Found(_)));
}

#[test]
fn mid_load_cancellation_is_returned_as_a_typed_error() {
    struct CancelLoad(MemoryStore);
    impl SourceStore for CancelLoad {
        fn list_directory(
            &mut self,
            request: DirectoryRequest<'_>,
            cancel: &dyn CancellationToken,
        ) -> Result<DirectoryListing, SourceStoreError> {
            self.0.list_directory(request, cancel)
        }

        fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf> {
            self.0.overlay_candidates(roots, names)
        }

        fn load(
            &mut self,
            _request: SourceRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<LoadedSource, SourceStoreError> {
            Err(SourceStoreError::Cancelled)
        }
    }
    let mut store = MemoryStore::default();
    store.add("/workspace/A.pas", "unit A; interface implementation end.");
    let source = LoadedSource {
        id: SourceId::new("source:/workspace/Main.pas"),
        path: PathBuf::from("/workspace/Main.pas"),
        bytes: Arc::from(&b""[..]),
        decoded_text: None,
        revision: SourceRevision::Overlay {
            version: 1,
            content_hash: 0,
        },
    };
    let importer = pascal_core::ResolvedUnit {
        requested_name: "Main".to_string(),
        declared_name: "Main".to_string(),
        source,
    };
    let mut resolver = UnitResolver::new(
        review_like_context(),
        vec![],
        CancelLoad(store),
        Default::default(),
    );

    assert!(matches!(
        resolver.resolve_imports(
            &importer,
            &[pascal_core::ImportSite {
                byte_range: 0..1,
                requested_name: "A".to_string(),
                section: pascal_core::ImportSection::Interface,
            }],
            &NoCancellation,
        ),
        Err(ResolverError::Cancelled)
    ));
}

#[test]
fn incomplete_outcome_taints_the_final_report() {
    let mut resolver = UnitResolver::new(
        review_like_context(),
        vec![],
        MemoryStore::default(),
        Default::default(),
    );
    let outcome = resolver.resolve_include(
        pascal_core::IncludeResolveRequest {
            including_path: Path::new("/workspace/Main.pas"),
            byte_range: 0..5,
            requested_name: "",
            legacy_route: None,
        },
        &NoCancellation,
    );
    assert!(matches!(outcome.result, Resolution::Incomplete { .. }));
    assert!(!resolver.finish().complete);
}

fn fixture_context() -> ProjectContext {
    let root = PathBuf::from("/workspace");
    let entry = ProjectPathEntry {
        path: root.clone(),
        provenance: ProjectPathProvenance::LegacyNative,
    };
    ProjectContext {
        discovery_complete: true,
        search_paths: vec![root.clone()],
        search_path_entries: vec![entry],
        read_policy: ReadPolicy::default(),
        ..ProjectContext::default()
    }
}

fn configured_disk_context(root: &Path) -> ProjectContext {
    ProjectContext {
        discovery_complete: true,
        project_file: Some(root.join("App.dproj")),
        read_policy: ReadPolicy::new(&[root.to_path_buf()], &[], &[], &Default::default()),
        ..ProjectContext::default()
    }
}

fn review_like_context() -> ProjectContext {
    let mut context = fixture_context();
    context.project_file = Some(PathBuf::from("/workspace/App.dproj"));
    context
}

fn resolve_memory_project(
    context: ProjectContext,
    store: MemoryStore,
    limits: ResolverLimits,
) -> pascal_core::ResolvedProject {
    UnitResolver::new(context, vec![PathBuf::from("/workspace")], store, limits)
        .resolve_project(
            UnitResolveRequest {
                requested_name: "Main",
                importer_path: Path::new("/workspace/Main.pas"),
                legacy_route: None,
            },
            &[],
            &NoCancellation,
        )
        .expect("project graph")
}

fn fixture_context_with_alias(alias: &str, target: &str) -> ProjectContext {
    let mut context = fixture_context();
    context
        .unit_aliases
        .insert(alias.to_string(), target.to_string());
    context.explicit_units.insert(
        target.to_ascii_lowercase(),
        vec![PathBuf::from("/workspace/Vendor.Errors.pas")],
    );
    context.explicit_unit_entries.insert(
        target.to_ascii_lowercase(),
        vec![ProjectPathEntry::legacy(PathBuf::from(
            "/workspace/Vendor.Errors.pas",
        ))],
    );
    context
}

fn fixture_resolver(context: ProjectContext) -> UnitResolver<MemoryStore> {
    let mut store = MemoryStore::default();
    store.add(
        "/workspace/Vendor.Errors.pas",
        "unit Vendor.Errors; interface implementation end.",
    );
    UnitResolver::new(
        context,
        vec![PathBuf::from("/workspace")],
        store,
        Default::default(),
    )
}

#[derive(Default)]
struct MemoryStore {
    sources: HashMap<String, Vec<u8>>,
    loads: Arc<Mutex<Vec<String>>>,
}

impl MemoryStore {
    fn with_log(loads: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            sources: HashMap::new(),
            loads,
        }
    }

    fn add(&mut self, path: &str, source: &str) {
        self.sources
            .insert(path.to_string(), source.as_bytes().to_vec());
    }
}

impl SourceStore for MemoryStore {
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError> {
        let prefix = format!(
            "{}/",
            request.directory.to_string_lossy().trim_end_matches('/')
        );
        let files = self
            .sources
            .keys()
            .filter(|path| path.starts_with(&prefix) && !path[prefix.len()..].contains('/'))
            .map(PathBuf::from)
            .collect();
        Ok(DirectoryListing {
            files,
            directories: Vec::new(),
            stamp: None,
            complete: true,
        })
    }

    fn overlay_candidates(&self, _roots: &[PathBuf], _names: &[String]) -> Vec<PathBuf> {
        Vec::new()
    }

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let key = request.path.to_string_lossy().to_string();
        let bytes = self
            .sources
            .get(&key)
            .cloned()
            .ok_or_else(|| SourceStoreError::NotFound {
                path: request.path.to_path_buf(),
            })?;
        self.loads.lock().expect("load log").push(key.clone());
        Ok(LoadedSource {
            id: SourceId::new(format!("source:{key}")),
            path: request.path.to_path_buf(),
            bytes: Arc::<[u8]>::from(bytes),
            decoded_text: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 0,
            },
        })
    }
}

struct FailingDirectoryStore {
    inner: MemoryStore,
    failing_directory: PathBuf,
}

impl SourceStore for FailingDirectoryStore {
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError> {
        if request.directory == self.failing_directory {
            return Err(SourceStoreError::Incomplete {
                path: request.directory.to_path_buf(),
                reason: "test directory failure".to_string(),
            });
        }
        self.inner.list_directory(request, &NoCancellation)
    }

    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf> {
        self.inner.overlay_candidates(roots, names)
    }

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.inner.load(request, cancel)
    }
}
