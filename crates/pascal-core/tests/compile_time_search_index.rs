//! l4d-4nbs D19: a session indexes the names of its settled search
//! directories, so a unit lookup skips the directories that cannot hold it
//! instead of matching every one. Precedence is today's search order.
#[path = "support/compile_time.rs"]
mod support;
use pascal_core::conditional::Truth;
use pascal_core::{FilesystemSourceStore, NoCancellation, ResolverLimits, UnitResolver};
use pascal_project::{LayoutPlatform, ProjectContext, ProjectOptions, ProjectPathEntry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Project {
    _dir: tempfile::TempDir,
    base: PathBuf,
    search: Vec<PathBuf>,
}

/// `directories` search directories with `fillers` files each.
fn project(directories: usize, fillers: usize) -> Project {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("root");
    std::fs::create_dir(&base).unwrap();
    let mut search = Vec::new();
    for d in 0..directories {
        let path = dir.path().join(format!("search{d:03}"));
        std::fs::create_dir(&path).unwrap();
        for f in 0..fillers {
            std::fs::write(path.join(format!("Filler{f:03}.pas")), "").unwrap();
        }
        search.push(path);
    }
    Project {
        _dir: dir,
        base,
        search,
    }
}

fn unit(directory: &Path, file: &str, name: &str, marker: i32) {
    std::fs::write(
        directory.join(file),
        format!("unit {name}; interface const Marker = {marker}; implementation end."),
    )
    .unwrap();
}

fn settle(project: &Project) {
    for directory in project.search.iter().chain([&project.base]) {
        for file in std::fs::read_dir(directory).unwrap() {
            pascal_project::backdate_for_test(&file.unwrap().path());
        }
        pascal_project::backdate_for_test(directory);
    }
}

fn analyze(
    project: &Project,
    root: &str,
    namespaces: &[&str],
    explicit: &[(&str, PathBuf)],
) -> (
    pascal_core::compile_time::CompileTimeAnalysis,
    pascal_core::compile_time::BudgetSnapshot,
) {
    let path = project.base.join("T.pas");
    std::fs::write(&path, root).unwrap();
    settle(project);
    let workspace = project.base.parent().unwrap().to_path_buf();
    let mut ctx = ProjectContext::discover(
        &path,
        std::slice::from_ref(&workspace),
        &ProjectOptions::default(),
    )
    .unwrap()
    .context;
    let mut parts = Arc::unwrap_or_clone(std::mem::take(&mut ctx)).into_parts();
    parts.conditional_context = support::context(LayoutPlatform::Win32);
    parts.library_conditional_context = parts.conditional_context.clone();
    // A project context: lookups use the search paths, not the projectless
    // workspace catalogue.
    parts.project_file = Some(project.base.join("T.dproj"));
    parts.search_path_entries = project
        .search
        .iter()
        .map(|p| ProjectPathEntry::legacy(p.clone()))
        .collect();
    parts.unit_namespaces = namespaces.iter().map(|n| (*n).into()).collect();
    let mut entries: HashMap<String, Vec<ProjectPathEntry>> = HashMap::new();
    for (name, file) in explicit {
        entries
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(ProjectPathEntry::legacy(file.clone()));
    }
    parts.explicit_unit_entries = entries;
    ctx = ProjectContext::from_parts(parts).into();
    let mut resolver = UnitResolver::new(
        ctx,
        vec![workspace],
        FilesystemSourceStore::new(),
        ResolverLimits {
            max_compile_time_retained_bytes: 256 * 1024 * 1024,
            ..ResolverLimits::default()
        },
    );
    let mut source = support::loaded("T.pas", root);
    source.path = path;
    let analysis = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    (analysis, resolver.compile_time_usage())
}

fn root(uses: &str, conditions: &[(&str, &str)]) -> String {
    let checks = conditions
        .iter()
        .map(|(condition, define)| format!("{{$IF {condition}}}{{$DEFINE {define}}}{{$IFEND}}"))
        .collect::<String>();
    format!("unit T; interface uses {uses}; {checks} implementation end.")
}

fn defined(analysis: &pascal_core::compile_time::CompileTimeAnalysis, define: &str) -> Truth {
    support::activity(analysis, &format!("DEFINE {define}"))
}

#[test]
fn the_first_search_directory_holding_a_unit_wins() {
    let project = project(6, 3);
    unit(&project.search[1], "Foo.pas", "Foo", 1);
    unit(&project.search[4], "Foo.pas", "Foo", 2);
    unit(&project.search[5], "Bar.pas", "Bar", 3);
    let (analysis, _) = analyze(
        &project,
        &root(
            "Bar, Foo",
            &[("Bar.Marker = 3", "BAR"), ("Foo.Marker = 1", "FIRST")],
        ),
        &[],
        &[],
    );
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(defined(&analysis, "BAR"), Truth::True);
    assert_eq!(defined(&analysis, "FIRST"), Truth::True);
}

#[test]
fn file_name_case_does_not_change_precedence() {
    let project = project(4, 3);
    unit(&project.search[1], "FOO.PAS", "Foo", 1);
    unit(&project.search[3], "foo.pas", "Foo", 2);
    let (analysis, _) = analyze(
        &project,
        &root("Foo", &[("Foo.Marker = 1", "FIRST")]),
        &[],
        &[],
    );
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(defined(&analysis, "FIRST"), Truth::True);
}

#[test]
fn directory_order_precedes_the_namespace_tier() {
    let project = project(3, 3);
    unit(&project.search[0], "Sys.Qux.pas", "Sys.Qux", 1);
    unit(&project.search[2], "Qux.pas", "Qux", 2);
    let (analysis, _) = analyze(
        &project,
        &root("Qux", &[("Qux.Marker = 1", "NAMESPACED")]),
        &["Sys"],
        &[],
    );
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(defined(&analysis, "NAMESPACED"), Truth::True);
}

#[test]
fn a_dotted_unit_in_a_nested_directory_is_found() {
    let project = project(3, 3);
    let nested = project.search[1].join("Deep");
    std::fs::create_dir(&nested).unwrap();
    unit(&nested, "Thing.pas", "Deep.Thing", 4);
    let (analysis, _) = analyze(
        &project,
        &root("Deep.Thing", &[("Deep.Thing.Marker = 4", "NESTED")]),
        &[],
        &[],
    );
    let nested_marker = defined(&analysis, "NESTED");
    // The nested layout is resolved as it is without the index.
    assert_eq!(nested_marker, Truth::True, "{:?}", analysis.reasons);
}

#[test]
fn an_explicit_in_path_precedes_the_search_path() {
    let project = project(3, 3);
    unit(&project.search[0], "Foo.pas", "Foo", 1);
    let other = project.base.join("other");
    std::fs::create_dir(&other).unwrap();
    unit(&other, "Foo.pas", "Foo", 9);
    let (analysis, _) = analyze(
        &project,
        &root("Foo", &[("Foo.Marker = 9", "EXPLICIT")]),
        &[],
        &[("Foo", other.join("Foo.pas"))],
    );
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(defined(&analysis, "EXPLICIT"), Truth::True);
}

/// Log.pas: each of 77 lookups matched names over about 200 search
/// directories (about 1M of byte work per lookup). With the index a lookup
/// costs about a stat per directory it skips.
#[test]
fn a_lookup_skips_directories_that_cannot_hold_the_unit() {
    let usage = |directories: usize| {
        let project = project(directories, 5);
        let last = project.search.last().unwrap().clone();
        for u in 0..40 {
            unit(&last, &format!("U{u}.pas"), &format!("U{u}"), u);
        }
        let uses = (0..40)
            .map(|u| format!("U{u}"))
            .collect::<Vec<_>>()
            .join(", ");
        let (analysis, usage) = analyze(
            &project,
            &root(&uses, &[("Declared(Missing)", "HIT")]),
            &[],
            &[],
        );
        assert!(analysis.root.complete, "{:?}", analysis.reasons);
        usage.byte_work
    };
    let (few, many) = (usage(20), usage(200));
    let per_lookup_directory = (many - few) / (40 * 180);
    assert!(
        per_lookup_directory < 600,
        "{per_lookup_directory} byte work per lookup and skipped directory ({few} -> {many})"
    );
}

/// A unit closure of `units` units each importing `uses` others, in the
/// last of `dirs` search directories, below a root that imports them all
/// and asks a name only the closure could declare.
fn closure(units: usize, uses: usize, dirs: usize) -> (
    pascal_core::compile_time::CompileTimeAnalysis,
    pascal_core::compile_time::BudgetSnapshot,
) {
    let project = project(dirs, 10);
    let last = project.search.last().unwrap().clone();
    for u in 0..units {
        // Interface uses are acyclic, as Delphi requires.
        let deps = (u + 1..units.min(u + 1 + uses))
            .map(|k| format!("U{k}"))
            .collect::<Vec<_>>();
        let uses_clause = if deps.is_empty() {
            String::new()
        } else {
            format!("uses {};", deps.join(", "))
        };
        let mut text = format!("unit U{u}; interface {uses_clause}\ntype\n");
        for n in 0..300 {
            text.push_str(&format!(
                "  TC{u}_{n} = class(TObject) procedure P; function F(X: Integer): Integer; end;\n"
            ));
        }
        text.push_str("implementation end.");
        std::fs::write(last.join(format!("U{u}.pas")), text).unwrap();
    }
    let all = (0..units)
        .map(|u| format!("U{u}"))
        .collect::<Vec<_>>()
        .join(", ");
    analyze(
        &project,
        &format!(
            "unit T; interface uses {all}; implementation {{$IF CompilerVersion > 21.0}}{{$DEFINE HIT}}{{$IFEND}} end."
        ),
        &[],
        &[],
    )
}

/// The provider query fuse bounds one session's unit lookups by import
/// site. SFClient2's BOSFBase units import a closure (RTL, VCL, DB, IBX,
/// business objects) of more than 256 import sites; Log.pas needed 237.
#[test]
fn a_closure_of_three_hundred_import_sites_is_resolved() {
    let (analysis, usage) = closure(40, 9, 20);
    assert!(analysis.root.complete, "{:?} {usage:?}", analysis.reasons);
}
