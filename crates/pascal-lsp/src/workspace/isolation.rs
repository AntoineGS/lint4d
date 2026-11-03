//! `l4d-zf1b.7` acceptance (spec §12.1 amendment 2026-11-02): two
//! `Workspace` sessions with distinct buffers over one store directory, in
//! one test process, which shares the store exactly as two LSP processes
//! do. Every session opens its own `Store` over the shared directory.

use super::ReconciliationBudget;
use super::Workspace;
use super::admitted::{AdmittedStore, StoreBypass};
use super::authorize::{Refused, authorize_record};
use super::eligible::tests::{EDITED_SHAPES, budget, draft, no_cancel};
use super::eligible::{Ineligible, publish_record};
use super::evidence::EvidenceGap;
use super::persist::tests::{PROVIDER_MAIN, SHAPES, open_store, private_dir, reuse};
use super::persist::{PersistError, Persisted, Reuse, ReuseMiss, ReuseVia};
use super::resolver::deletion_overrides_by_path;
use super::validate::{
    CurrentInputs, EvidenceCheck, Stale, Unproven, Validator, validate_evidence,
};
use crate::project_cache::{DEFAULT_MAX_CACHE_BYTES, ProjectCache, ReservationKind};
use lsp_types::Url;
use pascal_cache::{
    ContextKey as CacheContextKey, DATABASE_FILE, Digest, LocalArtifactKey, Lookup, Miss,
    Namespace, Published, SourceContent, Store, StoreLimits,
};
use pascal_core::facts::ArtifactFacts;
use pascal_core::resolver::ResolverLimits;
use pascal_project::delphi_overrides::OverrideSession;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).unwrap()
}

/// One editor session: its long-lived workspace holds the buffers and the
/// project cache, and its own store handle over the shared directory;
/// `request` builds the workspace a worker analyses in.
struct Session {
    main: Workspace,
    store: Store,
}

impl Session {
    fn new(roots: &[&Path], directory: &Path) -> Self {
        Self {
            main: Workspace::with_override_session(
                roots.iter().map(|root| root.to_path_buf()).collect(),
                Default::default(),
                OverrideSession::new(None),
            ),
            store: open_store(directory),
        }
    }

    fn open(&mut self, path: &Path, text: &str) {
        self.main
            .open_document(uri(path), text.to_owned(), 1)
            .unwrap();
    }

    fn request(&self, warm: &[&Path]) -> Workspace {
        let mut workspace = Workspace::from_analysis_input(&self.main.analysis_input());
        for path in warm {
            workspace
                .warm_with_cancel(&uri(path), &no_cancel())
                .expect("warm");
        }
        workspace
    }
}

/// `Main.pas` using `Shapes.pas`: `Main`'s facts depend on its provider,
/// so they persist only under evidence that reads `Shapes.pas`. The root
/// is canonical so recorded paths and test paths agree.
fn providers() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("Main.pas"), PROVIDER_MAIN).unwrap();
    std::fs::write(root.join("Shapes.pas"), SHAPES).unwrap();
    (temp, root)
}

/// What `view` extracts for `path` from its own analysis. AC1 compares
/// whole facts: `Wide` cannot tell the analyses apart, since the extractor
/// leaves `SizeOf(TPair)` unevaluated and `Wide`'s activity `Unknown`.
fn analysed(view: &Workspace, path: &Path) -> ArtifactFacts {
    view.source_facts(&uri(path), &mut budget(), &no_cancel())
        .unwrap()
        .extracted
        .facts
}

/// AC1: unsaved text and overlay dependencies of one session never reach
/// the other through persisted facts or bindings, including an overlay
/// provider behind a saved root and a save-then-close transition.
#[test]
fn ac1_unsaved_buffers_of_two_sessions_never_cross_through_the_store() {
    let (_tree, root) = providers();
    let (main, shapes) = (root.join("Main.pas"), root.join("Shapes.pas"));
    let directory = private_dir();
    let a = Session::new(&[&root], directory.path());
    let mut b = Session::new(&[&root], directory.path());
    b.open(&shapes, EDITED_SHAPES);

    // B: an overlay provider behind a saved root. Nothing B derived from it
    // is publishable.
    let b_view = b.request(&[&main]);
    let refused = b_view.persist_source(&uri(&main), &b.store, &mut budget(), &no_cancel());
    assert!(
        matches!(
            refused,
            Err(PersistError::Evidence(EvidenceGap::Overlay) | PersistError::NotPersistable(_))
        ),
        "{refused:?}"
    );
    let ineligible = b_view.eligible_member(&uri(&main), draft(), &mut budget(), &no_cancel());
    assert!(
        matches!(ineligible, Err(Ineligible::Artifact(_))),
        "{ineligible:?}"
    );
    assert_eq!(
        b_view
            .eligible_member(&uri(&shapes), draft(), &mut budget(), &no_cancel())
            .err(),
        Some(Ineligible::SessionPrivate(uri(&shapes)))
    );

    // A publishes its disk view.
    let a_view = a.request(&[&main]);
    assert!(matches!(
        a_view.persist_source(&uri(&main), &a.store, &mut budget(), &no_cancel()),
        Ok(Persisted::Expanded(_))
    ));
    let a_record = a_view
        .eligible_member(&uri(&main), draft(), &mut budget(), &no_cancel())
        .unwrap();
    publish_record(&a.store, &a_record).unwrap();

    // B never reuses A's facts while its buffer shadows the provider they
    // were proven with.
    let mut b_reader = b.request(&[]);
    assert_eq!(
        reuse(&mut b_reader, &uri(&main), &b.store).unwrap(),
        Reuse::Miss(ReuseMiss::Stale(Stale::Overlay(shapes.clone())))
    );
    // A reuses its own facts, which never saw B's text.
    let mut a_reader = a.request(&[]);
    let Reuse::Hit { facts, .. } = reuse(&mut a_reader, &uri(&main), &a.store).unwrap() else {
        panic!("A reuses what it published")
    };
    assert_eq!(*facts.facts, analysed(&a_view, &main));
    drop(facts);

    // B saves. The document stays open, so its analysis stays
    // overlay-sourced, even though its bytes now equal the disk's.
    std::fs::write(&shapes, EDITED_SHAPES).unwrap();
    b.main
        .save_document(&uri(&shapes), Some(EDITED_SHAPES.to_owned()))
        .unwrap();
    let b_saved = b.request(&[&main]);
    assert_eq!(
        b_saved
            .eligible_member(&uri(&shapes), draft(), &mut budget(), &no_cancel())
            .err(),
        Some(Ineligible::SessionPrivate(uri(&shapes)))
    );
    assert!(matches!(
        b_saved.eligible_member(&uri(&main), draft(), &mut budget(), &no_cancel()),
        Err(Ineligible::Artifact(_))
    ));

    // A's evidence read the old provider: once B's save reaches the disk, A
    // reuses nothing until an analysis of the saved text is published.
    let mut a_saved = a.request(&[]);
    assert_eq!(
        reuse(&mut a_saved, &uri(&main), &a.store).unwrap(),
        Reuse::Miss(ReuseMiss::Stale(Stale::Content(shapes.clone())))
    );

    // After close, a disk analysis establishes a separate eligible result.
    b.main.close_document(&uri(&shapes));
    let b_closed = b.request(&[&main]);
    assert!(matches!(
        b_closed.persist_source(&uri(&main), &b.store, &mut budget(), &no_cancel()),
        Ok(Persisted::Expanded(_))
    ));
    let b_record = b_closed
        .eligible_member(&uri(&main), draft(), &mut budget(), &no_cancel())
        .unwrap();
    assert_ne!(
        b_record.value().header.artifact,
        a_record.value().header.artifact,
        "a separate artifact ID: each is keyed by the provider bytes it saw"
    );
    publish_record(&b.store, &b_record).unwrap();

    // A's own evidence was shown stale above and an expanded artifact has
    // no local-key hit, so a hit through `Evidence` can only be B's disk
    // evidence, validated against the saved provider. (Both analyses
    // extract identical facts, so the facts alone cannot show the source.)
    let mut a_after = a.request(&[]);
    let Reuse::Hit {
        facts,
        via: ReuseVia::Evidence,
        ..
    } = reuse(&mut a_after, &uri(&main), &a.store).unwrap()
    else {
        panic!("B's saved disk analysis is reusable through its evidence")
    };
    assert_eq!(*facts.facts, analysed(&b_closed, &main));
}

/// AC2: an artifact created under wider read roots is refused by a
/// narrower requester although its rows are in the database.
#[test]
fn ac2_rows_published_under_wider_roots_are_refused_to_a_narrower_requester() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (app_main, lib) = (root.join("app/Main.pas"), root.join("lib/Lib.pas"));
    for (path, text) in [
        (
            &app_main,
            "unit Main;\ninterface\nuses Lib;\n{$IF SizeOf(TPair) = 8}\nconst Wide = 1;\n{$ENDIF}\nimplementation\nend.\n",
        ),
        (
            &lib,
            "unit Lib;\ninterface\ntype TPair = record A, B: Integer; end;\nimplementation\nend.\n",
        ),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let directory = private_dir();

    // The wide session publishes `Lib`'s record and `Main`'s facts, whose
    // evidence reads `lib/Lib.pas`.
    let wide = Session::new(&[&root], directory.path());
    let wide_view = wide.request(&[&lib, &app_main]);
    assert_eq!(
        wide_view
            .persist_source(&uri(&lib), &wide.store, &mut budget(), &no_cancel())
            .unwrap(),
        Persisted::Local(Published::Inserted)
    );
    let record = wide_view
        .eligible_member(&uri(&lib), draft(), &mut budget(), &no_cancel())
        .unwrap();
    let record_id = publish_record(&wide.store, &record).unwrap().record_id;
    assert!(matches!(
        wide_view.persist_source(&uri(&app_main), &wide.store, &mut budget(), &no_cancel()),
        Ok(Persisted::Expanded(_))
    ));
    let wide_context = &wide_view.contexts[&wide_view.document_contexts[&uri(&lib)]].context;
    let mut wide_reader = wide.request(&[]);
    assert!(matches!(
        reuse(&mut wide_reader, &uri(&app_main), &wide.store).unwrap(),
        Reuse::Hit {
            via: ReuseVia::Evidence,
            ..
        }
    ));

    let narrow = Session::new(&[&root.join("app")], directory.path());
    // The narrower session reads the rows through its own handle.
    let Lookup::Hit(header) = narrow.store.open_member_record(record_id).unwrap() else {
        panic!("the rows exist")
    };
    assert!(authorize_record(header.clone(), wide_context).is_ok());
    let mut narrow_view = narrow.request(&[&app_main]);
    let narrow_context =
        Arc::clone(&narrow_view.contexts[&narrow_view.document_contexts[&uri(&app_main)]].context);
    let refused = authorize_record(header, &narrow_context).err();
    assert!(
        matches!(refused, Some(Refused::NoPathEntry(_) | Refused::Policy(_))),
        "{refused:?}"
    );
    assert!(authorize_record(record.value().clone(), &narrow_context).is_err());

    // The narrower context's key differs, so reuse finds none of the wide
    // session's evidence.
    assert_eq!(
        reuse(&mut narrow_view, &uri(&app_main), &narrow.store).unwrap(),
        Reuse::Miss(ReuseMiss::Store(Miss::Absent))
    );
    // Found through the wide context's key, as a context-free member
    // record would lead to it, the wide evidence is still refused under the
    // narrower session's current inputs: it proves inputs only under the
    // context that recorded it.
    let source = wide_view
        .source_facts(&uri(&app_main), &mut budget(), &no_cancel())
        .unwrap();
    let local = LocalArtifactKey::new(
        &SourceContent::from_bytes(&std::fs::read(&app_main).unwrap()),
        &source.extracted.conditional_context,
        source.origin,
    );
    let wide_main_context =
        &wide_view.contexts[&wide_view.document_contexts[&uri(&app_main)]].context;
    let Lookup::Hit(headers) = narrow
        .store
        .find_evidence(&CacheContextKey::new(wide_main_context), &local)
        .unwrap()
    else {
        panic!("the wide evidence exists")
    };
    let overlays = narrow_view.overlay_inputs();
    let deleted = deletion_overrides_by_path(&narrow_view.deleted_overrides);
    let cancel = Arc::new(AtomicBool::new(false));
    let work = ReconciliationBudget::new(Arc::clone(&cancel));
    let narrow_key = CacheContextKey::new(&narrow_context);
    let mut validator = Validator::new(
        CurrentInputs {
            context: &narrow_key,
            read_policy: narrow_context.read_policy(),
            overlays: &overlays,
            deleted: &deleted,
            cache: &narrow_view.project_cache,
            max_file_bytes: narrow_view.options.limits.max_file_bytes,
            directory_entries: ResolverLimits::default().max_directory_entries,
        },
        &work,
        &cancel,
    );
    assert_eq!(headers.len(), 1);
    assert_eq!(
        validate_evidence(&narrow.store, &headers[0], &mut validator).unwrap(),
        EvidenceCheck::Stale(Stale::Context)
    );

    // The narrow session analysing `lib/Lib.pas` itself discovers a
    // standalone context that names it: it read that root under its own
    // policy, which is not authorization by a wider session.
    assert!(matches!(
        reuse(&mut narrow_view, &uri(&lib), &narrow.store).unwrap(),
        Reuse::Hit {
            via: ReuseVia::Local,
            ..
        }
    ));
}

/// AC3: totals stay within `maxCacheBytes` while it holds. After a runtime
/// decrease, what requests keep (facts, leased parses) stays accounted and
/// new reservations are refused, although the totals then exceed the
/// budget by design.
#[test]
fn ac3_held_charges_stay_accounted_and_new_reservations_are_refused_after_a_decrease() {
    const BUDGET: usize = 256 << 20;
    let (_tree, root) = providers();
    let main = root.join("Main.pas");
    let directory = private_dir();
    let a = Session::new(&[&root], directory.path());
    a.request(&[&main])
        .persist_source(&uri(&main), &a.store, &mut budget(), &no_cancel())
        .unwrap();
    let cache = a.main.project_cache().clone();
    cache.set_max_bytes(BUDGET);
    let within = |cache: &ProjectCache| {
        let stats = cache.stats();
        stats.bytes + stats.outstanding_bytes + stats.reserved_bytes() <= BUDGET
    };

    // A request keeps reused facts; a worker keeps the parse it took.
    let mut request = a.request(&[]);
    let Reuse::Hit { facts, .. } = reuse(&mut request, &uri(&main), &a.store).unwrap() else {
        panic!("the persisted facts are reused")
    };
    let held = facts.charge.bytes();
    assert!(held > 0);
    let worker = a.request(&[&main]);
    assert!(
        worker.cache_leases.contains_key(&uri(&main)),
        "the worker leases the parse it keeps"
    );
    assert!(within(&cache));
    assert_eq!(cache.stats().reserved_facts_bytes, held);

    // A runtime decrease evicts every unpinned entry. What requests keep
    // stays charged: facts as a reservation, parses as outstanding leases.
    cache.set_max_bytes(held);
    let stats = cache.stats();
    assert_eq!(stats.bytes, 0, "nothing unpinned is retained");
    assert_eq!(stats.reserved_facts_bytes, held, "kept facts stay charged");
    assert!(stats.outstanding_bytes > 0, "kept parses stay charged");
    assert!(
        cache.reserve(ReservationKind::Scratch, 1).is_none(),
        "a lower budget refuses new reservations"
    );
    let mut retry = a.request(&[]);
    assert_eq!(
        reuse(&mut retry, &uri(&main), &a.store).unwrap(),
        Reuse::Miss(ReuseMiss::Unproven(Unproven::Memory))
    );
    // A cold store under the lower budget evicts itself at once; its
    // parse is leased under the store's lock and counted as outstanding.
    let before = cache.stats().outstanding_bytes;
    let cold = a.request(&[&main]);
    assert!(cold.cache_leases.contains_key(&uri(&main)));
    assert!(cache.stats().outstanding_bytes > before);

    drop((facts, worker, cold, request, retry));
    let stats = cache.stats();
    assert_eq!((stats.reserved_bytes(), stats.outstanding_bytes), (0, 0));
    assert!(cache.reserve(ReservationKind::Scratch, 1).is_some());
}

/// AC4: a tiny budget gives bounded misses, never a loss of authoritative
/// editor state.
#[test]
fn ac4_a_tiny_budget_refuses_admission_without_touching_editor_state() {
    const BUFFER: &str = "unit Notes;\ninterface\nconst Unsaved = 1;\nimplementation\nend.\n";
    let (_tree, root) = providers();
    let (main, notes) = (root.join("Main.pas"), root.join("Notes.pas"));
    std::fs::write(&notes, "unit Notes;\ninterface\nimplementation\nend.\n").unwrap();
    let directory = private_dir();
    let publisher = Session::new(&[&root], directory.path());
    publisher
        .request(&[&main])
        .persist_source(&uri(&main), &publisher.store, &mut budget(), &no_cancel())
        .unwrap();

    let mut c = Session::new(&[&root], directory.path());
    c.open(&notes, BUFFER);
    let mut request = c.request(&[&notes]);
    let cache = request.project_cache.clone();
    let notes_uri = uri(&notes);
    let fingerprint = crate::navigation::compiled_dcu::project_context_fingerprint(
        &request.contexts[&request.document_contexts[&notes_uri]].context,
    );
    cache.pin(&notes_uri, vec![(notes_uri.clone(), fingerprint)]);
    assert!(
        cache.ready_layers(&notes_uri).contains(&"Unit".to_string()),
        "the open document's parse is cached"
    );
    let overlays = request.overlay_inputs();
    assert!(
        overlays.contains_key(&crate::workspace::canonical_file_uri(&notes_uri)),
        "the edited document is an overlay"
    );
    let pinned = cache.stats();
    assert!(pinned.bytes > 0 && pinned.units > 0);

    cache.set_max_bytes(1);
    let elsewhere = private_dir();
    assert_eq!(
        AdmittedStore::open(
            &cache,
            elsewhere.path(),
            &Namespace::new(Digest::from_bytes([11; 32])),
            StoreLimits::default(),
        )
        .err(),
        Some(StoreBypass::Memory)
    );
    assert!(
        !elsewhere.path().join(DATABASE_FILE).exists(),
        "no store was opened"
    );
    assert_eq!(
        reuse(&mut request, &uri(&main), &c.store).unwrap(),
        Reuse::Miss(ReuseMiss::Unproven(Unproven::Memory))
    );
    assert!(cache.reserve(ReservationKind::Facts, 1).is_none());

    // Authoritative editor state is untouched.
    assert!(
        cache.ready_layers(&notes_uri).contains(&"Unit".to_string()),
        "pinned parses are never evicted for admission"
    );
    let kept = cache.stats();
    assert_eq!(
        (kept.bytes, kept.units, kept.imports),
        (pinned.bytes, pinned.units, pinned.imports),
        "the pinned entries are unchanged"
    );
    let after = request.overlay_inputs();
    assert_eq!(after.len(), overlays.len());
    for (key, overlay) in overlays.iter() {
        assert_eq!(after[key].text, overlay.text);
        assert_eq!(after[key].version, overlay.version);
    }
    assert_eq!(
        request.open_documents[&notes_uri].text.as_deref(),
        Some(BUFFER)
    );
    assert_eq!(
        c.main.open_documents[&notes_uri].text.as_deref(),
        Some(BUFFER)
    );

    // The misses were retryable: with memory back, reuse hits.
    cache.set_max_bytes(DEFAULT_MAX_CACHE_BYTES);
    assert!(matches!(
        reuse(&mut request, &uri(&main), &c.store).unwrap(),
        Reuse::Hit { .. }
    ));
}
