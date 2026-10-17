//! The workspace's adapter from an indexed source to its compact facts.
//! Background indexing uses it from `l4d-zf1b.8` on.
#![cfg_attr(not(test), allow(dead_code))]

use super::Workspace;
use crate::navigation::{ExtractedFacts, extract_facts};
use lsp_types::Url;
use pascal_core::facts::{FactBudget, FactError};
use pascal_project::SourceOrigin;
use std::sync::atomic::AtomicBool;

#[derive(Debug)]
pub(crate) struct SourceFacts {
    pub(crate) extracted: ExtractedFacts,
    /// The source's origin in its project context: the local key's last
    /// input.
    pub(crate) origin: SourceOrigin,
}

impl Workspace {
    /// Facts of the indexed `uri`, as its current analysis sees it.
    pub(crate) fn source_facts(
        &self,
        uri: &Url,
        budget: &mut FactBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceFacts, FactError> {
        let parsed = self
            .index
            .parsed_document(uri)
            .ok_or(FactError::NotIndexed)?;
        let state = self
            .document_contexts
            .get(uri)
            .and_then(|key| self.contexts.get(key))
            .ok_or(FactError::NotIndexed)?;
        let path = uri.to_file_path().map_err(|()| FactError::NotIndexed)?;
        let expansion = self.expansions.get(uri).map(|record| &record.expanded);
        Ok(SourceFacts {
            extracted: extract_facts(&parsed, expansion, budget, cancel)?,
            origin: state.context.source_origin(&path),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pascal_core::facts::*;
    use pascal_project::delphi_overrides::OverrideSession;
    use std::path::PathBuf;

    pub(super) struct Indexed {
        _temp: tempfile::TempDir,
        pub(super) root: PathBuf,
        pub(super) workspace: Workspace,
    }

    /// Index `open` in a worker over a fresh directory holding `files`.
    pub(super) fn index(files: &[(&str, &[u8])], open: &[&str]) -> Indexed {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for (name, bytes) in files {
            std::fs::write(root.join(name), bytes).unwrap();
        }
        let main = Workspace::with_override_session(
            vec![root.clone()],
            Default::default(),
            OverrideSession::new(None),
        );
        let mut workspace = Workspace::from_analysis_input(&main.analysis_input());
        for name in open {
            let uri = Url::from_file_path(root.join(name)).unwrap();
            workspace
                .warm_with_cancel(&uri, &AtomicBool::new(false))
                .expect("warm");
            assert!(
                workspace.index.parsed_document(&uri).is_some(),
                "{name} must be indexed"
            );
        }
        Indexed {
            _temp: temp,
            root,
            workspace,
        }
    }

    impl Indexed {
        pub(super) fn uri(&self, name: &str) -> Url {
            Url::from_file_path(self.root.join(name)).unwrap()
        }

        pub(super) fn facts(&self, name: &str) -> SourceFacts {
            self.workspace
                .source_facts(
                    &self.uri(name),
                    &mut FactBudget::new(1 << 26),
                    &AtomicBool::new(false),
                )
                .unwrap()
        }
    }

    pub(super) fn posting<'a>(facts: &'a ArtifactFacts, name: &str) -> &'a PostingFact {
        facts
            .postings
            .iter()
            .find(|posting| posting.name == name)
            .unwrap_or_else(|| panic!("no posting {name}"))
    }

    pub(super) const MAIN: &str = "unit Main;\ninterface\n{$I Types.inc}\n{$I Types.inc}\n{$I Missing.inc}\nimplementation\nend.\n";
    pub(super) const TYPES: &str = "{$I Nested.inc}\nconst FromTypes = 1;\n";
    pub(super) const NESTED: &str = "const FromNested = 2;\n";
    pub(super) const CONDITIONAL: &[u8] = b"unit Cond;\ninterface\n{$IFDEF MAYBE}\nconst Maybe = 1;\n{$ENDIF}\n{$IF False}\nconst Never = 2;\n{$ENDIF}\nconst Always = 3;\nimplementation\nend.\n";
    pub(super) const LATIN: &[u8] = b"unit Latin;\ninterface\n{ caf\xe9 cr\xe8me }\nconst Greeting = 'd\xe9j\xe0';\nconst After = 1;\nimplementation\nend.\n";

    pub(super) const BROKEN: &[u8] =
        b"unit Broken;\ninterface\nprocedure Bad(;\nconst After = 1;\nimplementation\nend.\n";

    pub(super) fn includes() -> Indexed {
        index(
            &[
                ("Main.pas", MAIN.as_bytes()),
                ("Types.inc", TYPES.as_bytes()),
                ("Nested.inc", NESTED.as_bytes()),
            ],
            &["Main.pas"],
        )
    }

    #[test]
    fn include_facts_locate_every_occurrence_in_its_own_slot() {
        let indexed = includes();
        let source = indexed.facts("Main.pas");
        assert_eq!(source.extracted.persistable, Ok(()));
        let facts = source.extracted.facts;
        let shape: Vec<_> = facts
            .includes
            .iter()
            .map(|include| (include.slot, include.parent, include.expansion))
            .collect();
        use IncludeExpansion::{Expanded, Unresolved};
        assert_eq!(
            shape,
            vec![
                (1, 0, Expanded),
                (2, 1, Expanded),
                (3, 0, Expanded),
                (4, 3, Expanded),
                (5, 0, Unresolved)
            ]
        );
        let texts = [MAIN, TYPES, NESTED, TYPES, NESTED, ""];
        let lengths: Vec<u32> = texts.iter().map(|text| text.len() as u32).collect();
        assert_eq!(facts.slot_lengths, lengths);
        for include in &facts.includes {
            assert!(
                texts[include.parent as usize][include.directive.to_range()].starts_with("{$I ")
            );
        }
        let constants: Vec<_> = facts
            .declarations
            .iter()
            .filter(|declaration| declaration.kind == DeclarationKind::Constant)
            .map(|declaration| match declaration.location {
                FactLocation::Source { slot, range } => (
                    declaration.display.as_str(),
                    slot,
                    &texts[slot as usize][range.to_range()],
                ),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            constants,
            vec![
                ("FromNested", 2, "FromNested"),
                ("FromTypes", 1, "FromTypes"),
                ("FromNested", 4, "FromNested"),
                ("FromTypes", 3, "FromTypes"),
            ]
        );
        let slots: Vec<u32> = posting(&facts, "fromnested")
            .occurrences
            .iter()
            .map(|occurrence| occurrence.slot)
            .collect();
        assert_eq!(slots, vec![2, 4]);
        assert!(facts.coverage.contains(&CoverageReason::UnresolvedInclude));
        // Every physical segment of the source map is its slot's text.
        let expanded = indexed.workspace.expansions[&indexed.uri("Main.pas")]
            .expanded
            .text();
        assert_eq!(facts.segments.first().map(|s| s.expanded.start), Some(0));
        assert_eq!(
            facts.segments.last().map(|s| s.expanded.end),
            Some(facts.expanded_length)
        );
        for segment in &facts.segments {
            if let SegmentTarget::Source { slot, start } = segment.target {
                let start = start as usize;
                assert_eq!(
                    &expanded[segment.expanded.to_range()],
                    &texts[slot as usize][start..start + segment.expanded.len() as usize]
                );
            }
        }
        let parsed = indexed
            .workspace
            .index
            .parsed_document(&indexed.uri("Main.pas"))
            .unwrap();
        assert_eq!(
            crate::navigation::extract_facts(
                &parsed,
                None,
                &mut FactBudget::new(1 << 26),
                &AtomicBool::new(false)
            )
            .unwrap_err(),
            FactError::MissingSourceMap
        );
    }

    #[test]
    fn non_utf8_sources_are_located_in_decoded_coordinates() {
        let indexed = index(&[("Latin.pas", LATIN)], &["Latin.pas"]);
        let facts = indexed.facts("Latin.pas").extracted.facts;
        let decoded = crate::workspace::resolver::decode_source_bytes(LATIN);
        assert_eq!(facts.slot_lengths, vec![decoded.len() as u32]);
        let after = facts
            .declarations
            .iter()
            .find(|d| d.name == "after")
            .unwrap();
        let FactLocation::Source { slot: 0, range } = after.location else {
            panic!("{:?}", after.location)
        };
        assert_eq!(&decoded[range.to_range()], "After");
        let raw = LATIN
            .windows(5)
            .position(|window| window == b"After")
            .unwrap();
        assert_eq!(
            range.start as usize,
            raw + 4,
            "four Latin-1 bytes before it decode to two UTF-8 bytes each"
        );
        assert_eq!(posting(&facts, "caf").occurrences.len(), 1);
    }

    #[test]
    fn provider_dependent_compile_time_facts_are_not_persistable() {
        let indexed = index(
            &[
                (
                    "Main.pas",
                    b"unit Main;\ninterface\nuses Shapes;\n{$IF SizeOf(TPair) = 8}\nconst Wide = 1;\n{$ENDIF}\nimplementation\nend.\n".as_slice(),
                ),
                (
                    "Shapes.pas",
                    b"unit Shapes;\ninterface\ntype TPair = record A, B: Integer; end;\nimplementation\nend.\n".as_slice(),
                ),
            ],
            &["Main.pas"],
        );
        let source = indexed.facts("Main.pas");
        assert_eq!(
            source.extracted.persistable,
            Err(NotPersistable::ProviderDependent)
        );
        assert!(
            !source.extracted.facts.declarations.is_empty(),
            "the facts stay usable in memory"
        );
    }

    #[test]
    fn a_session_without_includes_has_a_root_only_artifact() {
        let indexed = index(&[("Cond.pas", CONDITIONAL)], &["Cond.pas"]);
        let source = indexed.facts("Cond.pas");
        assert_eq!(source.extracted.persistable, Ok(()));
        assert_eq!(
            source.origin,
            SourceOrigin::Library,
            "no project file names it"
        );
        let facts = &source.extracted.facts;
        assert!(facts.includes.is_empty() && facts.segments.is_empty());
        assert_eq!(facts.slot_lengths, vec![CONDITIONAL.len() as u32]);
        let names: Vec<_> = facts
            .declarations
            .iter()
            .map(|d| (d.name.as_str(), d.certainty.activity))
            .collect();
        assert_eq!(
            names,
            vec![
                ("cond", Activity::Active),
                ("maybe", Activity::Unknown),
                ("always", Activity::Active)
            ]
        );
        assert_eq!(
            posting(facts, "never")
                .occurrences
                .iter()
                .map(|o| o.activity)
                .collect::<Vec<_>>(),
            vec![PostingActivity::Inactive]
        );
        assert!(facts.coverage.contains(&CoverageReason::ConditionalUnknown));
    }

    #[cfg(unix)]
    mod persisted {
        use super::*;
        use pascal_cache::{
            ArtifactKeyRef, Bypass, ContextKey, Digest, ExpandedArtifactKey, LocalArtifactKey,
            Lookup, Namespace, Published, SourceContent, Store, StoreLimits, encode_facts,
        };
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        fn private_dir() -> tempfile::TempDir {
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), PermissionsExt::from_mode(0o700)).unwrap();
            directory
        }

        fn store(directory: &Path) -> Store {
            Store::open(
                directory,
                &Namespace::new(Digest::from_bytes([9; 32])),
                StoreLimits::default(),
            )
            .unwrap()
        }

        struct Keys {
            local: LocalArtifactKey,
            expanded: Option<ExpandedArtifactKey>,
        }

        impl Keys {
            fn artifact(&self) -> ArtifactKeyRef<'_> {
                match &self.expanded {
                    Some(expanded) => ArtifactKeyRef::expanded(expanded, &self.local).unwrap(),
                    None => ArtifactKeyRef::local(&self.local),
                }
            }
        }

        /// The keys of `name`'s facts. Built from the context the parse
        /// used, not `LocalArtifactKey::for_source`.
        fn keys(indexed: &Indexed, name: &str, source: &SourceFacts) -> Keys {
            let bytes = std::fs::read(indexed.root.join(name)).unwrap();
            let context = &source.extracted.conditional_context;
            let local =
                LocalArtifactKey::new(&SourceContent::from_bytes(&bytes), context, source.origin);
            let parsed = indexed
                .workspace
                .index
                .parsed_document(&indexed.uri(name))
                .unwrap();
            let expanded = parsed
                .compile_time_session()
                .map(|session| include_slots(session).unwrap())
                .filter(|slots| !slots.is_empty())
                .map(|slots| ExpandedArtifactKey::from_slots(&local, context, &slots).unwrap());
            Keys { local, expanded }
        }

        fn load(store: &Store, keys: &Keys) -> Lookup<ArtifactFacts> {
            store
                .load_facts(keys.artifact(), &mut FactBudget::new(1 << 26))
                .unwrap()
        }

        #[test]
        fn facts_round_trip_through_the_store_for_every_fixture() {
            let directory = private_dir();
            let store = store(directory.path());
            type Files<'a> = &'a [(&'a str, &'a [u8])];
            let fixtures: [(Files, &str, bool); 4] = [
                (&[("Cond.pas", CONDITIONAL)], "Cond.pas", false),
                (&[("Broken.pas", BROKEN)], "Broken.pas", false),
                (&[("Latin.pas", LATIN)], "Latin.pas", false),
                (
                    &[
                        ("Main.pas", MAIN.as_bytes()),
                        ("Types.inc", TYPES.as_bytes()),
                        ("Nested.inc", NESTED.as_bytes()),
                    ],
                    "Main.pas",
                    true,
                ),
            ];
            for (files, main, expanded) in fixtures {
                let indexed = index(files, &[main]);
                let source = indexed.facts(main);
                assert_eq!(source.extracted.persistable, Ok(()), "{main}");
                let keys = keys(&indexed, main, &source);
                assert_eq!(keys.expanded.is_some(), expanded, "{main}");
                assert_eq!(
                    store.publish_facts(keys.artifact(), &source.extracted.facts),
                    Ok(Published::Inserted),
                    "{main}"
                );
                // Equality covers certainty, coverage and mappings: nothing
                // is upgraded to proof on the way through the store.
                assert_eq!(
                    load(&store, &keys),
                    Lookup::Hit(source.extracted.facts.clone()),
                    "{main}"
                );
            }
        }

        #[test]
        fn identical_local_inputs_from_two_projects_share_one_artifact_without_their_paths() {
            const SHARED: &[u8] = b"unit Shared;\ninterface\n{$IFDEF FEATURE}\nconst Feature = 1;\n{$ENDIF}\nconst Plain = 2;\nimplementation\nend.\n";
            const LOOSE: &[u8] = b"const Loose = 1;\n";
            const IMPORTER: &[u8] = b"unit Main;\ninterface\nuses Shared;\nimplementation\nend.\n";
            let dproj = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>";
            let a = index(
                &[
                    ("App.dproj", dproj.as_bytes()),
                    ("Main.pas", IMPORTER),
                    ("Shared.pas", SHARED),
                    ("Snippet.pas", LOOSE),
                ],
                &["Shared.pas", "Snippet.pas"],
            );
            let b = index(
                &[
                    ("Other.dproj", dproj.as_bytes()),
                    ("Main.pas", IMPORTER),
                    ("Shared.pas", SHARED),
                    ("Fragment.pas", LOOSE),
                ],
                &["Shared.pas", "Fragment.pas"],
            );
            let context = |indexed: &Indexed, name: &str| {
                let key = &indexed.workspace.document_contexts[&indexed.uri(name)];
                ContextKey::new(&indexed.workspace.contexts[key].context)
            };
            assert_ne!(
                context(&a, "Shared.pas"),
                context(&b, "Shared.pas"),
                "two distinct project contexts"
            );
            let directory = private_dir();
            let (store_a, store_b) = (store(directory.path()), store(directory.path()));
            for (name_a, name_b) in [
                ("Shared.pas", "Shared.pas"),
                ("Snippet.pas", "Fragment.pas"),
            ] {
                let (facts_a, facts_b) = (a.facts(name_a), b.facts(name_b));
                let (keys_a, keys_b) = (keys(&a, name_a, &facts_a), keys(&b, name_b, &facts_b));
                assert_eq!(keys_a.local, keys_b.local, "{name_a}");
                assert!(keys_a.expanded.is_none() && keys_b.expanded.is_none());
                assert_eq!(facts_a.extracted.facts, facts_b.extracted.facts, "{name_a}");
                assert_eq!(
                    store_a.publish_facts(keys_a.artifact(), &facts_a.extracted.facts),
                    Ok(Published::Inserted)
                );
                assert_eq!(
                    store_b.publish_facts(keys_b.artifact(), &facts_b.extracted.facts),
                    Ok(Published::AlreadyPresent)
                );
                assert_eq!(
                    load(&store_b, &keys_b),
                    Lookup::Hit(facts_a.extracted.facts.clone())
                );
                let encoded = encode_facts(&facts_a.extracted.facts).unwrap();
                let mut bytes = encoded.supplemental.clone();
                for row in &encoded.rows {
                    bytes.extend(row.index_key.iter().flatten());
                    bytes.extend(&row.payload);
                }
                for needle in [
                    a.root.as_os_str().as_bytes(),
                    b.root.as_os_str().as_bytes(),
                    b"snippet",
                    b"Snippet",
                    b"fragment",
                    b"Fragment",
                    b".dproj",
                ] {
                    assert!(
                        !bytes.windows(needle.len()).any(|window| window == needle),
                        "{name_a}'s artifact holds {:?}",
                        String::from_utf8_lossy(needle)
                    );
                }
            }
        }

        #[test]
        fn facts_outlive_their_syntax_trees_and_are_admitted_before_retention() {
            fn owned<T: Send + Sync + 'static>() {}
            owned::<ArtifactFacts>();
            let indexed = includes();
            let mut small = FactBudget::new(512);
            assert!(matches!(
                indexed.workspace.source_facts(
                    &indexed.uri("Main.pas"),
                    &mut small,
                    &AtomicBool::new(false)
                ),
                Err(FactError::Budget { limit: 512 })
            ));
            assert!(small.used() <= 512);
            let source = indexed.facts("Main.pas");
            let keys = keys(&indexed, "Main.pas", &source);
            // Every parse and syntax tree of the workspace goes here.
            drop(indexed);
            let directory = private_dir();
            let store = store(directory.path());
            assert_eq!(
                store.publish_facts(keys.artifact(), &source.extracted.facts),
                Ok(Published::Inserted)
            );
            let mut tight = FactBudget::new(64);
            assert_eq!(
                store.load_facts(keys.artifact(), &mut tight),
                Err(Bypass::FactBudget)
            );
            assert_eq!(tight.used(), 0);
            assert_eq!(load(&store, &keys), Lookup::Hit(source.extracted.facts));
        }
    }
}
