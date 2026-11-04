//! l4d-4nbs R13: a session reuses a typed directory listing while the
//! directory is proven unchanged (settled, equal stamp), so many lookups over
//! the same search directories list each once; a changed or unsettled
//! directory is listed again.
#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::{AdmittedOwned, CompileTimeBudget};
use pascal_core::{
    CancellationToken, DirectoryListing, DirectoryRequest, FilesystemSourceStore, LoadedSource,
    NoCancellation, ResolverLimits, SourceRequest, SourceStore, SourceStoreError, UnitResolver,
};
use pascal_project::{LayoutPlatform, ProjectContext, ProjectOptions, ProjectPathEntry};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type AfterListing = Box<dyn FnMut(&Path, usize)>;

/// The filesystem store, counting typed listings per directory and running
/// `after_listing` after each.
struct Counting {
    disk: FilesystemSourceStore,
    listings: Arc<Mutex<Vec<PathBuf>>>,
    loads: Arc<Mutex<Vec<PathBuf>>>,
    /// Rewrite this file right after its first read.
    rewrite: Arc<Mutex<Option<(PathBuf, String)>>>,
    after_listing: AfterListing,
}

impl SourceStore for Counting {
    fn list_directory_admitted(
        &mut self,
        request: DirectoryRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedOwned<DirectoryListing>, SourceStoreError> {
        let result = self.disk.list_directory_admitted(request, budget, cancel);
        let count = {
            let mut listings = self.listings.lock().unwrap();
            listings.push(request.directory.to_path_buf());
            listings.len()
        };
        (self.after_listing)(request.directory, count);
        result
    }
    fn overlay_candidates_admitted(
        &self,
        roots: &[PathBuf],
        names: &[String],
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedOwned<Vec<PathBuf>>, SourceStoreError> {
        self.disk
            .overlay_candidates_admitted(roots, names, budget, cancel)
    }
    fn load_admitted(
        &mut self,
        request: SourceRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.loads.lock().unwrap().push(request.path.to_path_buf());
        let result = self.disk.load_admitted(request, budget, cancel);
        let mut rewrite = self.rewrite.lock().unwrap();
        if rewrite
            .as_ref()
            .is_some_and(|(path, _)| path == request.path)
        {
            let (path, text) = rewrite.take().unwrap();
            std::fs::write(path, text).unwrap();
        }
        result
    }
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError> {
        self.disk.list_directory(request, cancel)
    }
    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf> {
        self.disk.overlay_candidates(roots, names)
    }
    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.disk.load(request, cancel)
    }
}

struct Layout {
    _dir: tempfile::TempDir,
    root: PathBuf,
    search: Vec<PathBuf>,
    loads: Arc<Mutex<Vec<PathBuf>>>,
    rewrite: Arc<Mutex<Option<(PathBuf, String)>>>,
    /// Unrelated configured roots that only enlarge the read policy.
    extra_roots: usize,
}

/// `directories` search directories with `fillers` files each; units
/// `U0..U{units}` live in the last one. The root imports them all and asks
/// whether a name none declares is declared, which looks every import up.
fn layout(directories: usize, fillers: usize, units: usize, root_text: &str) -> Layout {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("root");
    std::fs::create_dir(&base).unwrap();
    let root = base.join("T.pas");
    std::fs::write(&root, root_text).unwrap();
    let mut search = Vec::new();
    for d in 0..directories {
        let path = dir.path().join(format!("search{d:02}"));
        std::fs::create_dir(&path).unwrap();
        for f in 0..fillers {
            std::fs::write(path.join(format!("Some.Long.Filler.Name{f:04}.pas")), "").unwrap();
        }
        search.push(path);
    }
    let last = search.last().unwrap();
    for u in 0..units {
        std::fs::write(
            last.join(format!("U{u}.pas")),
            format!("unit U{u}; interface const C{u} = {u}; implementation end."),
        )
        .unwrap();
    }
    Layout {
        _dir: dir,
        root,
        search,
        loads: Arc::default(),
        rewrite: Arc::default(),
        extra_roots: 0,
    }
}

fn settle(layout: &Layout) {
    for path in layout
        .search
        .iter()
        .map(PathBuf::as_path)
        .chain(layout.root.parent())
    {
        pascal_project::backdate_for_test(path);
    }
}

fn importer(units: usize) -> String {
    let uses = (0..units)
        .map(|u| format!("U{u}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "unit T; interface uses {uses}; {{$IF Declared(Missing)}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
    )
}

fn analyze(
    layout: &Layout,
    limits: ResolverLimits,
    after_listing: AfterListing,
) -> (
    Result<pascal_core::compile_time::CompileTimeAnalysis, pascal_core::ResolverError>,
    Vec<PathBuf>,
    pascal_core::compile_time::BudgetSnapshot,
) {
    let base = layout.root.parent().unwrap().to_path_buf();
    let mut ctx = ProjectContext::discover(
        &layout.root,
        &[base.parent().unwrap().into()],
        &ProjectOptions::default(),
    )
    .unwrap()
    .context;
    let mut parts = Arc::unwrap_or_clone(std::mem::take(&mut ctx)).into_parts();
    parts.conditional_context = support::context(LayoutPlatform::Win32);
    parts.search_path_entries = layout
        .search
        .iter()
        .map(|p| ProjectPathEntry::legacy(p.clone()))
        .collect();
    if layout.extra_roots > 0 {
        // A project context: lookups use the search paths only, never the
        // projectless workspace catalogue.
        parts.project_file = Some(base.join("T.dproj"));
        parts.library_conditional_context = parts.conditional_context.clone();
        let roots = std::iter::once(base.parent().unwrap().to_path_buf())
            .chain((0..layout.extra_roots).map(|n| {
                PathBuf::from(format!(
                    "/nonexistent/an/unrelated/configured/root/number{n:04}"
                ))
            }))
            .collect::<Vec<_>>();
        parts.read_policy = pascal_project::ReadPolicy::new(&roots, &[], &[], &parts.overrides);
    }
    ctx = ProjectContext::from_parts(parts).into();
    let listings = Arc::new(Mutex::new(Vec::new()));
    let loads = Arc::clone(&layout.loads);
    let store = Counting {
        disk: FilesystemSourceStore::new(),
        listings: Arc::clone(&listings),
        loads,
        rewrite: Arc::clone(&layout.rewrite),
        after_listing,
    };
    let mut resolver = UnitResolver::new(ctx, vec![base.parent().unwrap().into()], store, limits);
    let text = std::fs::read_to_string(&layout.root).unwrap();
    let mut source = support::loaded("T.pas", &text);
    source.path = layout.root.clone();
    let result = resolver.analyze_compile_time(&source, &NoCancellation);
    let listed = listings.lock().unwrap().clone();
    (result, listed, resolver.compile_time_usage())
}

fn count(listed: &[PathBuf], directory: &Path) -> usize {
    listed.iter().filter(|p| p.as_path() == directory).count()
}

/// Log.pas: one session looks 40 imports up over the same 40 search
/// directories (1,600 listings, 45 MB of byte work in the LSP).
#[test]
fn many_lookups_over_settled_directories_list_each_once() {
    let units = 40;
    let layout = layout(40, 100, units, &importer(units));
    settle(&layout);
    let limits = ResolverLimits {
        max_compile_time_retained_bytes: 64 * 1024 * 1024,
        ..ResolverLimits::default()
    };
    let (result, listed, usage) = analyze(&layout, limits, Box::new(|_, _| ()));
    let analysis = result.unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    for directory in &layout.search {
        assert!(
            count(&listed, directory) <= 1,
            "{} listed {} times; {} listings, {usage:?}",
            directory.display(),
            count(&listed, directory),
            listed.len()
        );
    }
}

/// A directory that changes during the session (a new provider file) is
/// listed again and the new file is found.
#[test]
fn a_directory_changed_mid_session_is_listed_again() {
    let layout = layout(
        1,
        10,
        1,
        "unit T; interface uses U0, Late; {$IF Declared(Missing)}{$DEFINE HIT}{$ENDIF} {$IF Late.Value = 7}{$DEFINE LATE}{$ENDIF} implementation end.",
    );
    settle(&layout);
    let directory = layout.search[0].clone();
    let watched = directory.clone();
    let mut written = false;
    let (result, listed, _) = analyze(
        &layout,
        ResolverLimits {
            max_compile_time_retained_bytes: 64 * 1024 * 1024,
            ..ResolverLimits::default()
        },
        Box::new(move |listed: &Path, _| {
            if !written && listed == watched {
                written = true;
                std::fs::write(
                    watched.join("Late.pas"),
                    "unit Late; interface const Value = 7; implementation end.",
                )
                .unwrap();
            }
        }),
    );
    let analysis = result.unwrap();
    assert!(count(&listed, &directory) >= 2, "{listed:?}");
    assert_eq!(
        support::activity(&analysis, "DEFINE LATE"),
        pascal_core::conditional::Truth::True,
        "{:?}",
        analysis.reasons
    );
}

/// An unsettled directory proves nothing about its next listing.
#[test]
fn an_unsettled_directory_is_listed_for_every_lookup() {
    let units = 3;
    let layout = layout(1, 10, units, &importer(units));
    let directory = layout.search[0].clone();
    let (result, listed, _) = analyze(
        &layout,
        ResolverLimits {
            max_compile_time_retained_bytes: 64 * 1024 * 1024,
            ..ResolverLimits::default()
        },
        Box::new(|_, _| ()),
    );
    result.unwrap();
    assert!(count(&listed, &directory) >= units, "{listed:?}");
}

/// Every importer of Windows.pas looked it up and read the 1.3 MB file
/// again (4 to 5 reads per session in the LSP). A session reads a settled,
/// unchanged file once.
#[test]
fn a_unit_imported_by_several_units_is_read_once_per_session() {
    let layout = layout(
        1,
        0,
        0,
        "unit T; interface uses A0, A1, A2, A3; implementation {$IF CompilerVersion > 21.0}{$DEFINE HIT}{$IFEND} end.",
    );
    let search = &layout.search[0];
    for a in 0..4 {
        std::fs::write(
            search.join(format!("A{a}.pas")),
            format!("unit A{a}; interface uses W; implementation end."),
        )
        .unwrap();
    }
    let w = search.join("W.pas");
    std::fs::write(&w, "unit W; interface const X = 1; implementation end.").unwrap();
    pascal_project::backdate_for_test(&w);
    settle(&layout);
    let (result, _, _) = analyze(
        &layout,
        ResolverLimits {
            max_compile_time_retained_bytes: 64 * 1024 * 1024,
            ..ResolverLimits::default()
        },
        Box::new(|_, _| ()),
    );
    let analysis = result.unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    let loads = layout.loads.lock().unwrap();
    assert_eq!(count(&loads, &w), 1, "{loads:?}");
}

/// A file changed during the session is read again.
#[test]
fn a_unit_changed_mid_session_is_read_again() {
    let layout = layout(
        1,
        0,
        0,
        "unit T; interface uses A0, A1; implementation {$IF CompilerVersion > 21.0}{$DEFINE HIT}{$IFEND} end.",
    );
    let search = layout.search[0].clone();
    for a in 0..2 {
        std::fs::write(
            search.join(format!("A{a}.pas")),
            format!("unit A{a}; interface uses W; implementation end."),
        )
        .unwrap();
    }
    let w = search.join("W.pas");
    std::fs::write(&w, "unit W; interface const X = 1; implementation end.").unwrap();
    pascal_project::backdate_for_test(&w);
    settle(&layout);
    *layout.rewrite.lock().unwrap() = Some((
        w.clone(),
        "unit W; interface const X = 2; implementation end.".into(),
    ));
    let (result, _, _) = analyze(
        &layout,
        ResolverLimits {
            max_compile_time_retained_bytes: 64 * 1024 * 1024,
            ..ResolverLimits::default()
        },
        Box::new(|_, _| ()),
    );
    result.unwrap();
    let loads = layout.loads.lock().unwrap();
    assert!(count(&loads, &w) >= 2, "{loads:?}");
}

/// Reusing a listing must not copy the read policy per lookup: the LSP's
/// policy made each of 3,154 reuses cost 11.8 KB (37M of byte work). The
/// imports are missing, so the lookups do nothing but list and match.
#[test]
fn reusing_a_listing_does_not_copy_the_read_policy() {
    let usage = |extra_roots: usize| {
        let missing = (0..40)
            .map(|u| format!("Missing{u}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut layout = layout(
            20,
            20,
            0,
            &format!(
                "unit T; interface uses {missing}; implementation {{$IF CompilerVersion > 21.0}}{{$DEFINE HIT}}{{$IFEND}} end."
            ),
        );
        layout.extra_roots = extra_roots;
        settle(&layout);
        let (result, _, usage) = analyze(
            &layout,
            ResolverLimits {
                max_compile_time_retained_bytes: 64 * 1024 * 1024,
                ..ResolverLimits::default()
            },
            Box::new(|_, _| ()),
        );
        assert!(
            result.is_ok(),
            "{extra_roots} extra roots: {result:?} {usage:?}"
        );
        usage.byte_work
    };
    let (small, large) = (usage(1), usage(30));
    // Other per-lookup work grows with the policy (about 60 KB per lookup for
    // 30 roots); a policy copy per reused listing would add as much again.
    assert!(
        large < small + 3_500_000,
        "byte work {small} with a small policy, {large} with 30 more roots"
    );
}
