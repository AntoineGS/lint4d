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
}
