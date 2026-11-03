//! Spec §10 and §12.1 (amendments 2026-11-03): every storage failure is a
//! bypass. Uncached analysis, pinned parses, overlays and buffer text are
//! unchanged, and persistence resumes once the store recovers.

use super::Workspace;
use super::admitted::{AdmittedStore, StoreBypass};
use super::eligible::tests::{budget, no_cancel};
use super::persist::tests::{PROVIDER_MAIN, SHAPES, open_store, private_dir, reuse};
use super::persist::{PersistError, Reuse};
use lsp_types::Url;
use pascal_cache::{Bypass, DATABASE_FILE, Digest, Namespace, Store, StoreLimits, StorePolicy};
use pascal_core::facts::ArtifactFacts;
use pascal_project::delphi_overrides::OverrideSession;
use std::path::{Path, PathBuf};

const BUFFER: &str = "unit Notes;\ninterface\nconst Unsaved = 1;\nimplementation\nend.\n";

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).unwrap()
}

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

fn providers() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("Main.pas"), PROVIDER_MAIN).unwrap();
    std::fs::write(root.join("Shapes.pas"), SHAPES).unwrap();
    (temp, root)
}

fn analysed(view: &Workspace, path: &Path) -> ArtifactFacts {
    view.source_facts(&uri(path), &mut budget(), &no_cancel())
        .unwrap()
        .extracted
        .facts
}

type Inject = fn(&Store, &Path);

fn failures() -> Vec<(&'static str, Bypass, Inject, Inject)> {
    vec![
        (
            "full",
            Bypass::Full,
            |s, _| s.limit_growth_for_test(0).unwrap(),
            |s, _| s.limit_growth_for_test(1 << 20).unwrap(),
        ),
        (
            "over budget",
            Bypass::OverBudget,
            |s, _| {
                s.set_policy(
                    0,
                    StorePolicy {
                        max_disk_bytes: 64 << 20,
                        wal_threshold_bytes: 8 << 20,
                    },
                )
                .unwrap();
                s.pretend_usage_for_test(60 << 20)
            },
            |s, _| s.pretend_usage_for_test(0),
        ),
        (
            "low space",
            Bypass::LowDiskSpace,
            |s, _| s.pretend_free_space_for_test(Some(0)),
            |s, _| s.pretend_free_space_for_test(None),
        ),
        (
            "corrupt",
            Bypass::Corrupt,
            |s, _| s.fail_next_for_test(Bypass::Corrupt),
            |_, _| {},
        ),
        (
            "recovering",
            Bypass::Recovering,
            |s, _| s.fail_next_for_test(Bypass::Recovering),
            |_, _| {},
        ),
        (
            "read-only",
            Bypass::ReadOnly,
            |s, _| s.fail_next_for_test(Bypass::ReadOnly),
            |_, _| {},
        ),
    ]
}

#[test]
fn each_storage_failure_leaves_uncached_analysis_and_editor_state_unchanged() {
    for (name, expected, inject, lift) in failures() {
        let (_tree, root) = providers();
        let (main, notes) = (root.join("Main.pas"), root.join("Notes.pas"));
        std::fs::write(&notes, "unit Notes;\ninterface\nimplementation\nend.\n").unwrap();
        let directory = private_dir();
        let mut session = Session::new(&[&root], directory.path());
        session.open(&notes, BUFFER);
        let request = session.request(&[&main, &notes]);
        let baseline = analysed(&request, &main);
        let overlays = request.overlay_inputs();
        let stats = request.project_cache.stats();
        inject(&session.store, directory.path());
        assert!(
            matches!(
                request.persist_source(&uri(&main), &session.store, &mut budget(), &no_cancel()),
                Err(PersistError::Store(ref bypass)) if *bypass == expected
            ),
            "{name}"
        );
        assert_eq!(
            analysed(&request, &main),
            baseline,
            "{name}: uncached analysis"
        );
        assert_eq!(
            request.project_cache.stats(),
            stats,
            "{name}: the cache is untouched"
        );
        let after = request.overlay_inputs();
        assert_eq!(after.len(), overlays.len(), "{name}");
        for (key, overlay) in overlays.iter() {
            assert_eq!(after[key].text, overlay.text, "{name}");
        }
        assert_eq!(
            session.main.open_documents[&uri(&notes)].text.as_deref(),
            Some(BUFFER),
            "{name}"
        );
        // Recovery: lift the failure, let the backoff end, persist again.
        lift(&session.store, directory.path());
        session
            .store
            .advance_clock_for_test(pascal_cache::MIN_HEALTH_BACKOFF);
        request
            .persist_source(&uri(&main), &session.store, &mut budget(), &no_cancel())
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let mut reader = session.request(&[&main]);
        assert!(
            matches!(
                reuse(&mut reader, &uri(&main), &session.store).unwrap(),
                Reuse::Hit { .. }
            ),
            "{name}"
        );
    }
}

#[test]
fn a_corrupt_store_is_never_opened_or_repaired_and_analysis_continues() {
    let (_tree, root) = providers();
    let directory = private_dir();
    let database = directory.path().join(DATABASE_FILE);
    std::fs::write(&database, vec![0x5a; 8192]).unwrap();
    let session = Workspace::with_override_session(
        vec![root.clone()],
        Default::default(),
        OverrideSession::new(None),
    );
    let cache = session.project_cache.clone();
    assert_eq!(
        AdmittedStore::open(
            &cache,
            directory.path(),
            &Namespace::new(Digest::from_bytes([11; 32])),
            StoreLimits::default()
        )
        .err(),
        Some(StoreBypass::Store(Bypass::Corrupt))
    );
    assert_eq!(
        std::fs::read(&database).unwrap(),
        vec![0x5a; 8192],
        "never rewritten"
    );
    assert_eq!(
        pascal_cache::admin::advice(&Bypass::Corrupt),
        pascal_cache::admin::RecoveryAdvice::FreshDirectory
    );
    let mut request = Workspace::from_analysis_input(&session.analysis_input());
    request
        .warm_with_cancel(&uri(&root.join("Main.pas")), &no_cancel())
        .expect("analysis without the cache");
}
