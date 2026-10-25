//! Publishing a source's facts with the evidence that proves them, and
//! reusing persisted facts only after current inputs validate (spec §7).
#![cfg_attr(not(test), allow(dead_code))]

use super::Workspace;
use super::evidence::EvidenceGap;
use super::validate::{Check, CurrentInputs, Stale, Unproven, Validator};
use lsp_types::Url;
use pascal_cache::{
    ArtifactKeyRef, Bypass, ContextKey as CacheContextKey, EvidenceWrite, ExpandedArtifactKey,
    KeyError, LocalArtifactKey, Lookup, Miss, Published, PublishedEvidence, SessionDependence,
    SourceContent, Store, observations_digest,
};
use pascal_core::facts::{ArtifactFacts, FactBudget, FactError, NotPersistable, include_slots};
use pascal_core::resolver::ResolverLimits;
use pascal_project::{ProjectContext, ProjectPathProvenance, ProjectWorkBudget};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Persisted {
    Local(Published),
    Expanded(PublishedEvidence),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersistError {
    NotPersistable(NotPersistable),
    Fact(FactError),
    Evidence(EvidenceGap),
    Key(KeyError),
    Store(Bypass),
    /// The root on disk is no longer the one that was parsed.
    RootChanged,
    /// The root is outside its context's authorized roots.
    Unauthorized,
    Unreadable(String),
    Context(String),
    NotIndexed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reuse {
    Hit {
        facts: ArtifactFacts,
        via: ReuseVia,
        /// Payloads and listings validation read rather than witnessed,
        /// including those read for candidates it rejected first.
        reads: usize,
    },
    Miss(ReuseMiss),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReuseVia {
    /// The key from the current root bytes is its own proof.
    Local,
    /// Evidence validated against current inputs.
    Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReuseMiss {
    /// The root is open or deleted in the editor; its view is
    /// session-private.
    Open,
    Stale(Stale),
    Unproven(Unproven),
    Store(Miss),
}

impl Workspace {
    /// The saved root bytes of `uri`, read under `context`'s current policy.
    fn saved_root_bytes(
        &self,
        uri: &Url,
        context: &ProjectContext,
    ) -> Result<Vec<u8>, PersistError> {
        let path = uri.to_file_path().map_err(|()| PersistError::NotIndexed)?;
        let entry = context
            .path_entry_for(&path)
            .ok_or(PersistError::Unauthorized)?;
        let limit = self.options.limits.max_file_bytes as u64;
        let policy = context.read_policy();
        if matches!(entry.provenance, ProjectPathProvenance::LegacyNative) {
            policy.read_legacy_payload_bytes(&entry, limit)
        } else {
            policy.read_payload_bytes(&entry, limit)
        }
        .map_err(PersistError::Unreadable)
    }

    /// Publish `uri`'s facts: under its local key when its analysis
    /// consulted nothing but the root, otherwise under its expanded key
    /// with the evidence that proves it (spec §6.2: one transaction).
    pub(crate) fn persist_source(
        &self,
        uri: &Url,
        store: &Store,
        budget: &mut FactBudget,
        cancel: &AtomicBool,
    ) -> Result<Persisted, PersistError> {
        // The document's context labels the facts and the evidence, which
        // describe the context that produced the session.
        if self
            .expansions
            .get(uri)
            .is_some_and(|record| self.document_contexts.get(uri) != Some(&record.context_key))
        {
            return Err(PersistError::Fact(FactError::Inconsistent));
        }
        let source = self
            .source_facts(uri, budget, cancel)
            .map_err(PersistError::Fact)?;
        source
            .extracted
            .persistable
            .map_err(PersistError::NotPersistable)?;
        let state = self
            .document_contexts
            .get(uri)
            .and_then(|key| self.contexts.get(key))
            .ok_or(PersistError::NotIndexed)?;
        let bytes = self.saved_root_bytes(uri, &state.context)?;
        if Some(pascal_project::content_hash_bytes(&bytes)) != source.root_content_hash {
            return Err(PersistError::RootChanged);
        }
        let conditional = &source.extracted.conditional_context;
        let local = LocalArtifactKey::new(
            &SourceContent::from_bytes(&bytes),
            conditional,
            source.origin,
        );
        let parsed = self
            .index
            .parsed_document(uri)
            .ok_or(PersistError::NotIndexed)?;
        let slots = match parsed.compile_time_session() {
            Some(session) => {
                include_slots(session).map_err(|_| PersistError::Fact(FactError::Inconsistent))?
            }
            None => Vec::new(),
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(PersistError::Cancelled);
        }
        if slots.is_empty() && !source.extracted.provider_dependent {
            return store
                .publish_facts(ArtifactKeyRef::local(&local), &source.extracted.facts)
                .map(Persisted::Local)
                .map_err(PersistError::Store);
        }
        // The evidence must be the parse's own session's, as extraction
        // checks for the source map.
        let record = self
            .expansions
            .get(uri)
            .ok_or(PersistError::Evidence(EvidenceGap::NoSession))?;
        let own = record
            .expanded
            .session
            .as_deref()
            .zip(parsed.compile_time_session())
            .is_some_and(|(recorded, parsed)| std::ptr::eq(recorded, parsed));
        if !own {
            return Err(PersistError::Fact(FactError::Inconsistent));
        }
        let observations = self
            .source_evidence(uri, cancel)
            .map_err(PersistError::Evidence)?;
        let context = CacheContextKey::new(&state.context);
        let expanded = if source.extracted.provider_dependent {
            ExpandedArtifactKey::dependent_from_slots(
                &local,
                conditional,
                &slots,
                SessionDependence {
                    context: context.digest(),
                    observations: observations_digest(&observations),
                },
            )
        } else {
            ExpandedArtifactKey::from_slots(&local, conditional, &slots)
        }
        .map_err(PersistError::Key)?;
        let key = ArtifactKeyRef::expanded(&expanded, &local)
            .expect("an expanded key built from this root names it");
        if cancel.load(Ordering::Relaxed) {
            return Err(PersistError::Cancelled);
        }
        store
            .publish_facts_with_evidence(
                key,
                &source.extracted.facts,
                &EvidenceWrite {
                    context: &context,
                    observations: &observations,
                },
            )
            .map(Persisted::Expanded)
            .map_err(PersistError::Store)
    }

    /// `uri`'s persisted facts, only if current inputs prove them (spec §7):
    /// the current context and root bytes select the keys, and evidence
    /// must validate under the current read policy and editor state.
    /// Nothing is indexed or parsed.
    pub(crate) fn reuse_persisted_facts(
        &mut self,
        uri: &Url,
        store: &Store,
        work: &dyn ProjectWorkBudget,
        budget: &mut FactBudget,
        cancel: &AtomicBool,
    ) -> Result<Reuse, PersistError> {
        let overlays = self.overlay_inputs();
        let deleted = super::resolver::deletion_overrides_by_path(&self.deleted_overrides);
        let path = uri.to_file_path().map_err(|()| PersistError::NotIndexed)?;
        if self.is_open(uri)
            || overlays.contains_key(&super::canonical_file_uri(uri))
            || super::resolver::deletion_applies(&deleted, &super::absolute_path(path.clone()))
        {
            return Ok(Reuse::Miss(ReuseMiss::Open));
        }
        let key = self
            .context_for_uri_with_cancel(uri, Some(cancel))
            .map_err(PersistError::Context)?;
        let context = Arc::clone(
            &self
                .contexts
                .get(&key)
                .ok_or(PersistError::NotIndexed)?
                .context,
        );
        let bytes = self.saved_root_bytes(uri, &context)?;
        let local = LocalArtifactKey::new(
            &SourceContent::from_bytes(&bytes),
            &super::conditional_context_for_uri(&context, uri),
            context.source_origin(&path),
        );
        if let Lookup::Hit(facts) = store
            .load_facts(ArtifactKeyRef::local(&local), budget)
            .map_err(PersistError::Store)?
        {
            return Ok(Reuse::Hit {
                facts,
                via: ReuseVia::Local,
                reads: 0,
            });
        }
        let headers = match store
            .find_evidence(&CacheContextKey::new(&context), &local)
            .map_err(PersistError::Store)?
        {
            Lookup::Hit(headers) => headers,
            Lookup::Miss(miss) => return Ok(Reuse::Miss(ReuseMiss::Store(miss))),
        };
        let mut validator = Validator::new(
            CurrentInputs {
                read_policy: context.read_policy(),
                overlays: &overlays,
                deleted: &deleted,
                cache: &self.project_cache,
                max_file_bytes: self.options.limits.max_file_bytes,
                directory_entries: ResolverLimits::default().max_directory_entries,
            },
            work,
            cancel,
        );
        let mut last = ReuseMiss::Store(Miss::Absent);
        'candidates: for header in &headers {
            let mut reader = store.read_evidence(header);
            loop {
                match reader.next_batch().map_err(PersistError::Store)? {
                    Lookup::Hit(Some(batch)) => {
                        for observation in batch.observations() {
                            match validator.check(observation) {
                                Check::Holds => {}
                                Check::Stale(stale) => {
                                    last = ReuseMiss::Stale(stale);
                                    continue 'candidates;
                                }
                                Check::Unproven(unproven) => {
                                    return Ok(Reuse::Miss(ReuseMiss::Unproven(unproven)));
                                }
                            }
                        }
                    }
                    // Every observation held and the evidence verified.
                    Lookup::Hit(None) => break,
                    Lookup::Miss(miss) => {
                        last = ReuseMiss::Store(miss);
                        continue 'candidates;
                    }
                }
            }
            if cancel.load(Ordering::Relaxed) {
                return Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Cancelled)));
            }
            return Ok(
                match store
                    .load_facts_by_id(header.artifact_id(), budget)
                    .map_err(PersistError::Store)?
                {
                    Lookup::Hit(facts) => Reuse::Hit {
                        facts,
                        via: ReuseVia::Evidence,
                        reads: validator.reads(),
                    },
                    Lookup::Miss(miss) => Reuse::Miss(ReuseMiss::Store(miss)),
                },
            );
        }
        Ok(Reuse::Miss(last))
    }
}

#[cfg(all(test, unix))]
pub(in crate::workspace) mod tests {
    use super::*;
    use crate::workspace::ReconciliationBudget;
    use crate::workspace::facts::tests::{
        CONDITIONAL, Indexed, MAIN, NESTED, TYPES, index, index_with_buffers,
    };
    use pascal_cache::{Digest, Namespace, StoreLimits};
    use pascal_project::delphi_overrides::OverrideSession;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    pub(in crate::workspace) const PROVIDER_MAIN: &[u8] = b"unit Main;\ninterface\nuses Shapes;\n{$IF SizeOf(TPair) = 8}\nconst Wide = 1;\n{$ENDIF}\nimplementation\nend.\n";
    pub(in crate::workspace) const SHAPES: &[u8] =
        b"unit Shapes;\ninterface\ntype TPair = record A, B: Integer; end;\nimplementation\nend.\n";

    pub(in crate::workspace) fn private_dir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), PermissionsExt::from_mode(0o700)).unwrap();
        directory
    }

    pub(in crate::workspace) fn open_store(directory: &Path) -> Store {
        Store::open(
            directory,
            &Namespace::new(Digest::from_bytes([11; 32])),
            StoreLimits::default(),
        )
        .unwrap()
    }

    /// A new server over the same directory: fresh workspace, fresh project
    /// cache, nothing indexed.
    pub(in crate::workspace) fn restart(indexed: &Indexed) -> Workspace {
        restart_with_buffers(indexed, &[])
    }

    pub(in crate::workspace) fn restart_with_buffers(
        indexed: &Indexed,
        buffers: &[(&str, &str)],
    ) -> Workspace {
        restart_with_edits(indexed, buffers, &[])
    }

    /// `restart_with_buffers`, with `deleted` removed in the editor but
    /// still on disk.
    pub(in crate::workspace) fn restart_with_edits(
        indexed: &Indexed,
        buffers: &[(&str, &str)],
        deleted: &[&str],
    ) -> Workspace {
        let mut main = Workspace::with_override_session(
            vec![indexed.root.clone()],
            Default::default(),
            OverrideSession::new(None),
        );
        for (name, text) in buffers {
            main.open_document(
                Url::from_file_path(indexed.root.join(name)).unwrap(),
                (*text).to_owned(),
                1,
            )
            .unwrap();
        }
        for name in deleted {
            main.remember_deleted(&Url::from_file_path(indexed.root.join(name)).unwrap());
        }
        Workspace::from_analysis_input(&main.analysis_input())
    }

    pub(in crate::workspace) fn reuse(
        workspace: &mut Workspace,
        uri: &Url,
        store: &Store,
    ) -> Result<Reuse, PersistError> {
        let cancel = Arc::new(AtomicBool::new(false));
        let work = ReconciliationBudget::new(Arc::clone(&cancel));
        workspace.reuse_persisted_facts(uri, store, &work, &mut FactBudget::new(1 << 26), &cancel)
    }

    pub(in crate::workspace) fn persist(indexed: &Indexed, name: &str, store: &Store) -> Persisted {
        indexed
            .workspace
            .persist_source(
                &indexed.uri(name),
                store,
                &mut FactBudget::new(1 << 26),
                &AtomicBool::new(false),
            )
            .unwrap()
    }

    #[test]
    fn persisted_facts_are_reused_after_a_restart() {
        let inserted = PublishedEvidence {
            artifact: Published::Inserted,
            evidence: Published::Inserted,
        };
        type Files<'a> = &'a [(&'a str, &'a [u8])];
        let fixtures: [(Files, ReuseVia); 3] = [
            (&[("Cond.pas", CONDITIONAL)], ReuseVia::Local),
            (
                &[
                    ("Main.pas", MAIN.as_bytes()),
                    ("Types.inc", TYPES.as_bytes()),
                    ("Nested.inc", NESTED.as_bytes()),
                ],
                ReuseVia::Evidence,
            ),
            (
                &[("Main.pas", PROVIDER_MAIN), ("Shapes.pas", SHAPES)],
                ReuseVia::Evidence,
            ),
        ];
        for (files, expected_via) in fixtures {
            let name = files[0].0;
            let indexed = index(files, &[name]);
            let directory = private_dir();
            let store = open_store(directory.path());
            let persisted = persist(&indexed, name, &store);
            match expected_via {
                ReuseVia::Local => assert_eq!(persisted, Persisted::Local(Published::Inserted)),
                ReuseVia::Evidence => assert_eq!(persisted, Persisted::Expanded(inserted)),
            }
            let expected = indexed.facts(name).extracted.facts;
            drop(store);
            let store = open_store(directory.path());
            let mut restarted = restart(&indexed);
            match reuse(&mut restarted, &indexed.uri(name), &store).unwrap() {
                Reuse::Hit { facts, via, .. } => {
                    assert_eq!(via, expected_via, "{name}");
                    assert_eq!(facts, expected, "{name}");
                }
                miss => panic!("{name}: {miss:?}"),
            }
        }
    }

    #[test]
    fn provider_dependent_facts_have_no_local_artifact() {
        let indexed = index(
            &[("Main.pas", PROVIDER_MAIN), ("Shapes.pas", SHAPES)],
            &["Main.pas"],
        );
        let directory = private_dir();
        let store = open_store(directory.path());
        persist(&indexed, "Main.pas", &store);
        let source = indexed.facts("Main.pas");
        let bytes = std::fs::read(indexed.root.join("Main.pas")).unwrap();
        let local = LocalArtifactKey::new(
            &SourceContent::from_bytes(&bytes),
            &source.extracted.conditional_context,
            source.origin,
        );
        assert_eq!(
            store
                .load_facts(ArtifactKeyRef::local(&local), &mut FactBudget::new(1 << 26))
                .unwrap(),
            Lookup::Miss(Miss::Absent),
            "a key recomputed from the root alone cannot reach provider-dependent facts"
        );
        let Lookup::Hit(headers) = store
            .find_evidence(
                &CacheContextKey::new(
                    &indexed.workspace.contexts
                        [&indexed.workspace.document_contexts[&indexed.uri("Main.pas")]]
                        .context,
                ),
                &local,
            )
            .unwrap()
        else {
            panic!("the evidence is stored under the root and context")
        };
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn nothing_is_published_after_cancellation() {
        let indexed = index(
            &[
                ("Main.pas", MAIN.as_bytes()),
                ("Types.inc", TYPES.as_bytes()),
                ("Nested.inc", NESTED.as_bytes()),
            ],
            &["Main.pas"],
        );
        let directory = private_dir();
        let store = open_store(directory.path());
        let result = indexed.workspace.persist_source(
            &indexed.uri("Main.pas"),
            &store,
            &mut FactBudget::new(1 << 26),
            &AtomicBool::new(true),
        );
        assert!(
            matches!(
                result,
                Err(PersistError::Cancelled | PersistError::Fact(FactError::Cancelled))
            ),
            "{result:?}"
        );
        let mut restarted = restart(&indexed);
        assert!(matches!(
            reuse(&mut restarted, &indexed.uri("Main.pas"), &store).unwrap(),
            Reuse::Miss(_)
        ));
    }

    #[test]
    fn a_session_is_not_published_under_another_context() {
        let mut indexed = index(
            &[
                ("Main.pas", MAIN.as_bytes()),
                ("Types.inc", TYPES.as_bytes()),
                ("Nested.inc", NESTED.as_bytes()),
            ],
            &["Main.pas"],
        );
        let uri = indexed.uri("Main.pas");
        let workspace = &mut indexed.workspace;
        let produced = workspace.expansions[&uri].context_key.clone();
        // Another configuration over the same files: the root stays readable.
        let mut parts = (*workspace.contexts[&produced].context)
            .clone()
            .into_parts();
        parts.config = Some("Other".to_owned());
        let other_context = ProjectContext::from_parts(parts);
        let other = crate::workspace::ContextKey {
            config: Some("Other".to_owned()),
            ..produced.clone()
        };
        workspace.contexts.insert(
            other.clone(),
            crate::workspace::ContextState {
                context: Arc::new(other_context.clone()),
                ..Default::default()
            },
        );
        workspace.document_contexts.insert(uri.clone(), other);
        let directory = private_dir();
        let store = open_store(directory.path());
        assert_eq!(
            workspace.persist_source(
                &uri,
                &store,
                &mut FactBudget::new(1 << 26),
                &AtomicBool::new(false)
            ),
            Err(PersistError::Fact(FactError::Inconsistent))
        );
        let source = indexed.facts("Main.pas");
        let bytes = std::fs::read(indexed.root.join("Main.pas")).unwrap();
        let local = LocalArtifactKey::new(
            &SourceContent::from_bytes(&bytes),
            &source.extracted.conditional_context,
            source.origin,
        );
        assert_eq!(
            store
                .find_evidence(&CacheContextKey::new(&other_context), &local)
                .unwrap(),
            Lookup::Miss(Miss::Absent)
        );
        let mut restarted = restart(&indexed);
        assert!(matches!(
            reuse(&mut restarted, &uri, &store).unwrap(),
            Reuse::Miss(ReuseMiss::Store(Miss::Absent))
        ));
    }

    #[test]
    fn open_documents_are_neither_persisted_nor_reused() {
        let files: &[(&str, &[u8])] = &[("Cond.pas", CONDITIONAL)];
        let edited = "unit Cond;\ninterface\nconst Edited = 1;\nimplementation\nend.\n";
        let open = index_with_buffers(files, &[("Cond.pas", edited)], &["Cond.pas"]);
        let directory = private_dir();
        let store = open_store(directory.path());
        assert!(matches!(
            open.workspace.persist_source(
                &open.uri("Cond.pas"),
                &store,
                &mut FactBudget::new(1 << 26),
                &AtomicBool::new(false)
            ),
            Err(PersistError::NotPersistable(NotPersistable::OverlayRoot))
        ));
        let saved = index(files, &["Cond.pas"]);
        persist(&saved, "Cond.pas", &store);
        let mut restarted = restart_with_buffers(&saved, &[("Cond.pas", edited)]);
        assert!(matches!(
            reuse(&mut restarted, &saved.uri("Cond.pas"), &store).unwrap(),
            Reuse::Miss(ReuseMiss::Open)
        ));
    }
}

/// `l4d-zf1b.5` acceptance criteria, each against a real store and a
/// restarted workspace.
#[cfg(all(test, unix))]
mod acceptance {
    use super::tests::*;
    use super::*;
    use crate::workspace::facts::tests::{CONDITIONAL, Indexed, MAIN, NESTED, TYPES, index};
    use std::path::Path;

    const DPROJ: &str = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>include</DCC_IncludePath><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>";
    const SHARED_MAIN: &[u8] = b"unit Main;\ninterface\n{$I Shared.inc}\nimplementation\nend.\n";
    const SHARED: &[u8] = b"const Shared = 1;\n";

    fn includes() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("Main.pas", MAIN.as_bytes().to_vec()),
            ("Types.inc", TYPES.as_bytes().to_vec()),
            ("Nested.inc", NESTED.as_bytes().to_vec()),
        ]
    }

    fn include_path() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("App.dproj", DPROJ.as_bytes().to_vec()),
            ("Main.pas", SHARED_MAIN.to_vec()),
            ("include/Shared.inc", SHARED.to_vec()),
        ]
    }

    fn providers() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("Main.pas", PROVIDER_MAIN.to_vec()),
            ("Shapes.pas", SHAPES.to_vec()),
            ("Notes.txt", b"notes".to_vec()),
            (
                "elsewhere/Other.pas",
                b"unit Other;\ninterface\nimplementation\nend.\n".to_vec(),
            ),
        ]
    }

    /// Index `files`, persist `Main.pas`, close the store.
    fn persisted(files: &[(&str, Vec<u8>)]) -> (Indexed, tempfile::TempDir) {
        let borrowed: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(name, bytes)| (*name, bytes.as_slice()))
            .collect();
        let indexed = index(&borrowed, &["Main.pas"]);
        let directory = private_dir();
        let store = open_store(directory.path());
        persist(&indexed, "Main.pas", &store);
        (indexed, directory)
    }

    /// Restart and look `Main.pas` up again.
    fn after_restart(indexed: &Indexed, directory: &tempfile::TempDir) -> Reuse {
        let store = open_store(directory.path());
        let mut restarted = restart(indexed);
        reuse(&mut restarted, &indexed.uri("Main.pas"), &store).unwrap()
    }

    /// A same-size rewrite that keeps the modification time, as one within
    /// one timestamp tick or a restoring tool would.
    fn rewrite_keeping_the_stamp(path: &Path, bytes: &[u8]) {
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.len(), bytes.len() as u64, "same size");
        std::fs::write(path, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(metadata.modified().unwrap())
            .unwrap();
    }

    fn is_hit(reuse: &Reuse) -> bool {
        matches!(reuse, Reuse::Hit { .. })
    }

    #[test]
    fn ac1_same_size_same_mtime_edits_while_closed_are_rejected() {
        let (indexed, directory) = persisted(&includes());
        assert!(
            is_hit(&after_restart(&indexed, &directory)),
            "the baseline is reused"
        );
        rewrite_keeping_the_stamp(
            &indexed.root.join("Types.inc"),
            TYPES.replace("FromTypes", "FromTyped").as_bytes(),
        );
        assert!(matches!(
            after_restart(&indexed, &directory),
            Reuse::Miss(ReuseMiss::Stale(Stale::Content(path))) if path.ends_with("Types.inc")
        ));

        let (indexed, directory) = persisted(&includes());
        rewrite_keeping_the_stamp(
            &indexed.root.join("Main.pas"),
            MAIN.replace("interface", "INTERFACE").as_bytes(),
        );
        assert_eq!(
            after_restart(&indexed, &directory),
            Reuse::Miss(ReuseMiss::Store(Miss::Absent)),
            "the rehashed root selects no stored key"
        );
    }

    #[test]
    fn ac2_input_changes_invalidate_bindings_including_negative_lookups() {
        type Change = Box<dyn Fn(&Path)>;
        type Case = (&'static str, Vec<(&'static str, Vec<u8>)>, Change);
        let cases: Vec<Case> = vec![
            (
                "include content",
                includes(),
                Box::new(|root| {
                    std::fs::write(root.join("Nested.inc"), "const Changed = 22;\n").unwrap()
                }),
            ),
            (
                "a shadowing include earlier in the search order",
                include_path(),
                Box::new(|root| {
                    std::fs::write(root.join("Shared.inc"), "const Shadow = 1;\n").unwrap()
                }),
            ),
            (
                "a missing include appears",
                includes(),
                Box::new(|root| {
                    std::fs::write(root.join("Missing.inc"), "const Found = 1;\n").unwrap()
                }),
            ),
            (
                "a provider is deleted",
                providers(),
                Box::new(|root| std::fs::remove_file(root.join("Shapes.pas")).unwrap()),
            ),
            (
                "a provider is renamed",
                providers(),
                Box::new(|root| {
                    std::fs::rename(root.join("Shapes.pas"), root.join("Shapes2.pas")).unwrap()
                }),
            ),
            (
                "a provider appears where none was found",
                vec![("Main.pas", PROVIDER_MAIN.to_vec())],
                Box::new(|root| std::fs::write(root.join("Shapes.pas"), SHAPES).unwrap()),
            ),
        ];
        for (label, files, change) in cases {
            let (indexed, directory) = persisted(&files);
            assert!(
                is_hit(&after_restart(&indexed, &directory)),
                "{label}: the baseline is reused"
            );
            change(&indexed.root);
            let result = after_restart(&indexed, &directory);
            assert!(
                matches!(result, Reuse::Miss(ReuseMiss::Stale(_))),
                "{label}: {result:?}"
            );
        }

        // Project configuration is proven by the context key.
        let (indexed, directory) = persisted(&include_path());
        std::fs::write(
            indexed.root.join("App.dproj"),
            DPROJ.replace("FEATURE", "OTHER_FEATURE"),
        )
        .unwrap();
        let result = after_restart(&indexed, &directory);
        assert!(matches!(result, Reuse::Miss(_)), "{result:?}");
    }

    #[test]
    fn ac3_unrelated_changes_keep_artifacts() {
        let (indexed, directory) = persisted(&providers());
        std::fs::write(indexed.root.join("Notes.txt"), b"other notes").unwrap();
        std::fs::write(
            indexed.root.join("elsewhere/New.pas"),
            b"unit New;\ninterface\nimplementation\nend.\n",
        )
        .unwrap();
        let result = after_restart(&indexed, &directory);
        assert!(is_hit(&result), "{result:?}");
    }

    #[test]
    fn ac3_restart_and_overflow_never_assume_watcher_coverage() {
        let (indexed, directory) = persisted(&providers());
        pascal_project::backdate_tree_for_test(&indexed.root);
        let store = open_store(directory.path());
        let uri = indexed.uri("Main.pas");
        let mut restarted = restart(&indexed);
        let reads = |workspace: &mut Workspace| match reuse(workspace, &uri, &store).unwrap() {
            Reuse::Hit { reads, .. } => reads,
            miss => panic!("{miss:?}"),
        };
        let first = reads(&mut restarted);
        assert!(first > 0, "a new process reads what it validates");
        assert_eq!(
            reads(&mut restarted),
            0,
            "settled witnesses spare the rereads"
        );
        crate::file_watch::apply_watch_event(
            &restarted.project_cache,
            crate::file_watch::WatchEvent::Overflow,
        );
        assert_eq!(reads(&mut restarted), first, "overflow drops every witness");
        assert_eq!(reads(&mut restart(&indexed)), first, "so does a restart");
    }

    #[test]
    fn ac4_symlinks_and_source_origin_are_checked() {
        let (indexed, directory) = persisted(&includes());
        let elsewhere = indexed.root.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let types = indexed.root.join("Types.inc");
        std::fs::copy(&types, elsewhere.join("Types.inc")).unwrap();
        std::fs::remove_file(&types).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("Types.inc"), &types).unwrap();
        let result = after_restart(&indexed, &directory);
        assert!(
            !is_hit(&result),
            "identical bytes behind a symlink: {result:?}"
        );

        let files = vec![("Main.pas", CONDITIONAL.to_vec())];
        let (indexed, directory) = persisted(&files);
        assert!(is_hit(&after_restart(&indexed, &directory)));
        // A project that names the unit makes it a project source.
        std::fs::write(
            indexed.root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        )
        .unwrap();
        let result = after_restart(&indexed, &directory);
        assert!(!is_hit(&result), "a changed source origin: {result:?}");
    }

    #[test]
    fn ac4_a_search_directory_replaced_by_a_symlink_to_a_copy_is_stale() {
        let nested = vec![
            (
                "App.dproj",
                DPROJ
                    .replace(">include<", ">lib/include<")
                    .as_bytes()
                    .to_vec(),
            ),
            ("Main.pas", SHARED_MAIN.to_vec()),
            ("lib/include/Shared.inc", SHARED.to_vec()),
        ];
        // `include` is in the listed owner directory; `lib`, the parent of
        // `lib/include`, is never listed. The copy is outside the project's
        // readable roots, or beside the link inside them.
        let elsewhere = tempfile::tempdir().unwrap();
        for (files, searched, inside) in [
            (include_path(), "include", false),
            (nested.clone(), "lib/include", false),
            (nested, "lib/include", true),
        ] {
            let (indexed, directory) = persisted(&files);
            assert!(is_hit(&after_restart(&indexed, &directory)), "{searched}");
            let include = indexed.root.join(searched);
            let copy = if inside {
                indexed.root.join("lib/copy")
            } else {
                tempfile::tempdir_in(elsewhere.path()).unwrap().keep()
            };
            std::fs::create_dir_all(&copy).unwrap();
            std::fs::copy(include.join("Shared.inc"), copy.join("Shared.inc")).unwrap();
            std::fs::remove_dir_all(&include).unwrap();
            std::os::unix::fs::symlink(&copy, &include).unwrap();
            let result = after_restart(&indexed, &directory);
            // Where no listing covers the link, the hit's own evidence refuses it.
            let by_the_hit = matches!(
                result,
                Reuse::Miss(ReuseMiss::Stale(Stale::Presence(_) | Stale::Content(_)))
            );
            assert!(
                matches!(result, Reuse::Miss(ReuseMiss::Stale(_)))
                    && (searched == "include" || by_the_hit),
                "{searched}: identical bytes behind a symlinked search directory: {result:?}"
            );
        }
    }

    #[test]
    fn editor_deletions_are_editor_state_on_reuse() {
        let (indexed, directory) = persisted(&include_path());
        assert!(is_hit(&after_restart(&indexed, &directory)));
        let store = open_store(directory.path());
        let uri = indexed.uri("Main.pas");
        let mut restarted = restart_with_edits(&indexed, &[], &["include/Shared.inc"]);
        assert_eq!(
            reuse(&mut restarted, &uri, &store).unwrap(),
            Reuse::Miss(ReuseMiss::Stale(Stale::Overlay(
                indexed.root.join("include/Shared.inc")
            )))
        );
        let mut restarted = restart_with_edits(&indexed, &[], &["Main.pas"]);
        assert_eq!(
            reuse(&mut restarted, &uri, &store).unwrap(),
            Reuse::Miss(ReuseMiss::Open)
        );
        // A local key is its own proof, so only the editor state refuses it.
        let (indexed, directory) = persisted(&[("Main.pas", CONDITIONAL.to_vec())]);
        assert!(is_hit(&after_restart(&indexed, &directory)));
        let store = open_store(directory.path());
        let mut restarted = restart_with_edits(&indexed, &[], &["Main.pas"]);
        assert_eq!(
            reuse(&mut restarted, &indexed.uri("Main.pas"), &store).unwrap(),
            Reuse::Miss(ReuseMiss::Open)
        );
    }

    #[test]
    fn ac4_cancelled_validation_is_never_a_hit() {
        struct CancelOnFirstVisit<'a>(&'a AtomicBool);
        impl ProjectWorkBudget for CancelOnFirstVisit<'_> {
            fn check_cancelled(&self) -> Result<(), String> {
                if self.0.load(Ordering::Relaxed) {
                    Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string())
                } else {
                    Ok(())
                }
            }
            fn charge_path_visits(&self, _amount: usize) -> Result<(), String> {
                self.0.store(true, Ordering::Relaxed);
                Ok(())
            }
            fn ensure_file_read_fits(&self, _max_bytes: usize) -> Result<(), String> {
                self.check_cancelled()
            }
            fn charge_file_bytes(&self, _amount: usize) -> Result<(), String> {
                self.check_cancelled()
            }
        }
        let (indexed, directory) = persisted(&providers());
        let store = open_store(directory.path());
        let mut restarted = restart(&indexed);
        let cancel = AtomicBool::new(false);
        let result = restarted.reuse_persisted_facts(
            &indexed.uri("Main.pas"),
            &store,
            &CancelOnFirstVisit(&cancel),
            &mut FactBudget::new(1 << 26),
            &cancel,
        );
        assert_eq!(
            result,
            Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Cancelled)))
        );
    }
}
