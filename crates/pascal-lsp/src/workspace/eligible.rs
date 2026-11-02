//! Persistence eligibility as a typed proof (spec §4.4 amendment
//! 2026-11-02). The store cannot see editor state and does not check it.
//! An [`Eligible`] value is built only by [`Workspace::eligible_member`]
//! and [`Workspace::eligible_snapshot`], which derive what they certify from
//! this session's own analysis, and [`publish_record`] and
//! [`seal_snapshot`] accept nothing else. The disk-only baseline producer
//! (`l4d-zf1b.8`) is the first caller.
//!
//! The proof guards against omission (an unchecked record, an evidence set
//! no capture produced, an edited member left out of a snapshot), not
//! against deliberate forging inside this crate. That a binding's outcome
//! is what its captured lookup actually found is the producer's
//! (`l4d-zf1b.8`) responsibility.
#![cfg_attr(not(test), allow(dead_code))]

use super::capture::CapturedReads;
use super::evidence::{EvidenceGap, session_evidence};
use super::persist::PersistError;
use super::{ContextKey, Workspace};
use lsp_types::Url;
use pascal_cache::{
    Binding, Bypass, ContextKey as CacheContextKey, Digest, DiscoveryScope, EvidencePurpose,
    EvidenceSubject, EvidenceTarget, EvidenceWrite, LookupKey, MAX_ROW_BATCH_ROWS, MemberCoverage,
    MemberError, MemberHeader, MemberRecord, Observation, PublishedRecord, SnapshotCoverage,
    SnapshotHeader, SnapshotSeal, SourceIdentity, Store, evidence_id,
};
use pascal_core::facts::FactBudget;
use pascal_core::resolver::ResolutionObservation;
use pascal_project::ProjectObservations;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// Observations that passed the capture gap checks: no editor state, no
/// overlay listing, no overflow, no cancellation and nothing uncaptured
/// (`workspace::evidence::session_evidence`). The only route from a capture
/// to publishable binding or discovery evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedEvidence {
    observations: Vec<Observation>,
}

impl CapturedEvidence {
    pub(crate) fn observations(&self) -> &[Observation] {
        &self.observations
    }
}

/// Check what a lookup or a discovery scan consulted, as `reads` captured
/// it. `reads` must be the `ReadCapture` snapshot of the very analysis that
/// produced `observations`: the gap checks prove nothing about
/// observations paired with another analysis's capture.
pub(crate) fn capture_evidence(
    observations: &[ResolutionObservation],
    reads: &CapturedReads,
    context: &ProjectObservations,
    cancel: &AtomicBool,
) -> Result<CapturedEvidence, EvidenceGap> {
    session_evidence(observations, reads, context, cancel)
        .map(|observations| CapturedEvidence { observations })
}

/// A value this session proved publishable, with the context it was
/// checked under and the evidence still to publish before it.
#[derive(Debug, Clone)]
pub(crate) struct Eligible<T> {
    value: T,
    context: CacheContextKey,
    evidence: Vec<PendingEvidence>,
}

#[derive(Debug, Clone)]
enum PendingEvidence {
    Binding(LookupKey, CapturedEvidence),
    Discovery(DiscoveryScope, CapturedEvidence),
}

impl<T> Eligible<T> {
    pub(crate) fn value(&self) -> &T {
        &self.value
    }

    pub(crate) fn context(&self) -> &CacheContextKey {
        &self.context
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ineligible {
    /// Open, overlaid or deleted in this session's editor (spec §4.4).
    SessionPrivate(Url),
    /// The artifact or its member evidence cannot be persisted.
    Artifact(PersistError),
    /// The workspace analysed the root, or knows the context, under no
    /// context state.
    NoContext,
    /// The binding's evidence is not the set its capture proves, has no
    /// capture, or a capture was given for a binding that names none.
    BindingEvidence {
        ordinal: usize,
    },
    Record(MemberError),
    /// A document of the context has no member in the snapshot.
    MissingMember(Url),
    /// A member is no document of the context.
    UnexpectedMember(PathBuf),
    DuplicateMember(PathBuf),
    /// A member was checked under another context.
    OtherContext(PathBuf),
}

/// What the workspace does not hold yet for a member record; the producer
/// (`l4d-zf1b.8`) supplies it.
#[derive(Debug, Clone)]
pub(crate) struct MemberDraft {
    pub(crate) fallback_unit_name: Option<String>,
    pub(crate) coverage: MemberCoverage,
    /// Strictly ordered by `(kind, occurrence)`, as `MemberRecord` requires.
    pub(crate) bindings: Vec<CheckedBinding>,
}

#[derive(Debug, Clone)]
pub(crate) struct CheckedBinding {
    pub(crate) binding: Binding,
    /// The lookup and the capture the binding's evidence came from:
    /// present exactly when `binding.evidence` is.
    pub(crate) proof: Option<(LookupKey, CapturedEvidence)>,
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotDraft {
    pub(crate) members: Vec<Eligible<MemberRecord>>,
    pub(crate) discovery: (DiscoveryScope, CapturedEvidence),
    pub(crate) coverage: SnapshotCoverage,
}

/// A context's complete eligible membership and its discovery evidence.
#[derive(Debug, Clone)]
pub(crate) struct EligibleSnapshot {
    members: Vec<Eligible<MemberRecord>>,
    discovery: Digest,
    coverage: SnapshotCoverage,
}

impl EligibleSnapshot {
    pub(crate) fn members(&self) -> &[Eligible<MemberRecord>] {
        &self.members
    }
}

fn subject(
    purpose: EvidencePurpose,
    context: &CacheContextKey,
    subject: Digest,
) -> EvidenceSubject {
    EvidenceSubject {
        purpose,
        context: context.digest(),
        subject,
    }
}

impl Workspace {
    /// `uri`'s member record, if this session may publish it. The header
    /// (source identity, artifact, member evidence, origin) comes from this
    /// workspace's own analysis through `prepare_artifact`, which applies
    /// the phase-5 persistability and evidence-gap rules; each binding's
    /// evidence ID is recomputed from the capture it came from under the
    /// member's context.
    pub(crate) fn eligible_member(
        &self,
        uri: &Url,
        draft: MemberDraft,
        budget: &mut FactBudget,
        cancel: &AtomicBool,
    ) -> Result<Eligible<MemberRecord>, Ineligible> {
        if self.root_is_session_private(uri) {
            return Err(Ineligible::SessionPrivate(uri.clone()));
        }
        let prepared = self
            .prepare_artifact(uri, budget, cancel)
            .map_err(Ineligible::Artifact)?;
        let state = self
            .document_contexts
            .get(uri)
            .and_then(|key| self.contexts.get(key))
            .ok_or(Ineligible::NoContext)?;
        let context = CacheContextKey::new(&state.context);
        let mut bindings = Vec::with_capacity(draft.bindings.len());
        let mut evidence = Vec::new();
        for (ordinal, checked) in draft.bindings.into_iter().enumerate() {
            match (checked.binding.evidence, checked.proof) {
                (None, None) => {}
                (Some(id), Some((lookup, captured)))
                    if evidence_id(
                        &subject(EvidencePurpose::Binding, &context, lookup.key().digest()),
                        None,
                        captured.observations(),
                    ) == id =>
                {
                    evidence.push(PendingEvidence::Binding(lookup, captured));
                }
                _ => return Err(Ineligible::BindingEvidence { ordinal }),
            }
            bindings.push(checked.binding);
        }
        let path = uri
            .to_file_path()
            .map_err(|()| Ineligible::Artifact(PersistError::NotIndexed))?;
        let canonical = super::absolute_path(path);
        let record = MemberRecord {
            header: MemberHeader {
                source: SourceIdentity {
                    canonical: canonical.clone(),
                    logical: canonical,
                    spelling: None,
                    uri: Some(uri.to_string()),
                },
                artifact: Some(prepared.artifact_id()),
                member_evidence: prepared.member_evidence_id(),
                origin: prepared.origin,
                fallback_unit_name: draft.fallback_unit_name,
                coverage: draft.coverage,
            },
            bindings,
        };
        record.validate().map_err(Ineligible::Record)?;
        Ok(Eligible {
            value: record,
            context,
            evidence,
        })
    }

    /// `context`'s snapshot, if this session may publish it: the context's
    /// complete membership as this workspace holds it (every document whose
    /// context it is), each member eligible under that context, and
    /// discovery evidence from a checked capture. One session-private
    /// document makes the whole set ineligible, supplied or not, so leaving
    /// an edited member out cannot be expressed.
    pub(crate) fn eligible_snapshot(
        &self,
        context: &ContextKey,
        draft: SnapshotDraft,
    ) -> Result<Eligible<EligibleSnapshot>, Ineligible> {
        let state = self.contexts.get(context).ok_or(Ineligible::NoContext)?;
        let cache_context = CacheContextKey::new(&state.context);
        let mut membership = BTreeMap::new();
        for (uri, key) in &self.document_contexts {
            if key != context {
                continue;
            }
            if self.root_is_session_private(uri) {
                return Err(Ineligible::SessionPrivate(uri.clone()));
            }
            let Ok(path) = uri.to_file_path() else {
                return Err(Ineligible::MissingMember(uri.clone()));
            };
            membership.insert(super::absolute_path(path), uri.clone());
        }
        let mut supplied = BTreeSet::new();
        for member in &draft.members {
            let path = &member.value.header.source.canonical;
            if member.context != cache_context {
                return Err(Ineligible::OtherContext(path.clone()));
            }
            if !membership.contains_key(path) {
                return Err(Ineligible::UnexpectedMember(path.clone()));
            }
            if !supplied.insert(path.clone()) {
                return Err(Ineligible::DuplicateMember(path.clone()));
            }
        }
        if let Some(uri) = membership
            .iter()
            .find(|(path, _)| !supplied.contains(*path))
            .map(|(_, uri)| uri.clone())
        {
            return Err(Ineligible::MissingMember(uri));
        }
        let (scope, captured) = draft.discovery;
        let discovery = evidence_id(
            &subject(
                EvidencePurpose::Discovery,
                &cache_context,
                scope.key().digest(),
            ),
            None,
            captured.observations(),
        );
        Ok(Eligible {
            value: EligibleSnapshot {
                members: draft.members,
                discovery,
                coverage: draft.coverage,
            },
            context: cache_context,
            evidence: vec![PendingEvidence::Discovery(scope, captured)],
        })
    }
}

/// Publish an eligible record: first the binding evidence it was checked
/// with, then the record. Its artifact and member evidence must already be
/// published (`Workspace::persist_source`); otherwise the store refuses the
/// record (`Bypass::MissingReference`).
pub(crate) fn publish_record(
    store: &Store,
    record: &Eligible<MemberRecord>,
) -> Result<PublishedRecord, Bypass> {
    publish_pending(store, record)?;
    store.publish_member_record(&record.value)
}

/// Seal an eligible snapshot: every member record, the discovery evidence,
/// then one build of at most `MAX_ROW_BATCH_ROWS` members per batch. An
/// unsealed build left by a bypass is retired by maintenance
/// (`l4d-zf1b.10`).
pub(crate) fn seal_snapshot(
    store: &Store,
    snapshot: &Eligible<EligibleSnapshot>,
) -> Result<SnapshotHeader, Bypass> {
    let mut records = Vec::with_capacity(snapshot.value.members.len());
    for member in &snapshot.value.members {
        records.push(publish_record(store, member)?.record_id);
    }
    publish_pending(store, snapshot)?;
    let mut builder = store.begin_snapshot(&snapshot.context)?;
    for batch in records.chunks(MAX_ROW_BATCH_ROWS) {
        store.add_members(&mut builder, batch)?;
    }
    store.seal_snapshot(
        builder,
        &SnapshotSeal {
            discovery: Some(snapshot.value.discovery),
            coverage: snapshot.value.coverage,
        },
    )
}

fn publish_pending<T>(store: &Store, proof: &Eligible<T>) -> Result<(), Bypass> {
    for pending in &proof.evidence {
        let (target, captured) = match pending {
            PendingEvidence::Binding(lookup, captured) => {
                (EvidenceTarget::Binding(lookup), captured)
            }
            PendingEvidence::Discovery(scope, captured) => {
                (EvidenceTarget::Discovery(scope), captured)
            }
        };
        store.publish_evidence(
            target,
            &EvidenceWrite {
                context: &proof.context,
                observations: captured.observations(),
            },
        )?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
pub(in crate::workspace) mod tests {
    use super::*;
    use crate::workspace::capture::CapturedListing;
    use crate::workspace::facts::tests::{
        CONDITIONAL, Indexed, MAIN, NESTED, TYPES, index, index_with_buffers,
    };
    use crate::workspace::persist::tests::{PROVIDER_MAIN, SHAPES, open_store, private_dir};
    use pascal_cache::{
        BindingKind, BindingOutcome, CoverageStatus, DiscoveryInputs, Lookup, LookupInputs,
        LookupKind, Published, ScannedDirectory,
    };
    use pascal_project::ProjectPathEntry;
    use std::path::Path;

    pub(in crate::workspace) const EDITED_SHAPES: &str =
        "unit Shapes;\ninterface\ntype TPair = record A: Integer; end;\nimplementation\nend.\n";

    pub(in crate::workspace) fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    pub(in crate::workspace) fn budget() -> FactBudget {
        FactBudget::new(1 << 26)
    }

    pub(in crate::workspace) fn draft() -> MemberDraft {
        MemberDraft {
            fallback_unit_name: None,
            coverage: MemberCoverage {
                extraction: CoverageStatus::Complete,
                resolution: CoverageStatus::Complete,
            },
            bindings: Vec::new(),
        }
    }

    pub(in crate::workspace) const COMPLETE: SnapshotCoverage = SnapshotCoverage {
        discovery: CoverageStatus::Complete,
        membership: CoverageStatus::Complete,
    };

    /// Empty units `A.pas` and `B.pas` of one context, `buffers` open over
    /// them, `open` indexed.
    pub(in crate::workspace) fn units_a_and_b(buffers: &[(&str, &str)], open: &[&str]) -> Indexed {
        let unit = |name: &str| format!("unit {name};\ninterface\nimplementation\nend.\n");
        let (a, b) = (unit("A"), unit("B"));
        index_with_buffers(
            &[("A.pas", a.as_bytes()), ("B.pas", b.as_bytes())],
            buffers,
            open,
        )
    }

    /// A listing of `directory` as a capture recorded it, checked.
    pub(in crate::workspace) fn listing_capture(
        directory: &Path,
        reads: impl FnOnce(&mut CapturedReads),
        cancel: bool,
    ) -> Result<CapturedEvidence, EvidenceGap> {
        let observation = ResolutionObservation::Directory {
            path: directory.to_path_buf(),
            entry: ProjectPathEntry::configured(directory.to_path_buf()),
            stamp: None,
            complete: true,
            listed: true,
        };
        let mut captured = CapturedReads {
            listings: vec![CapturedListing {
                directory: directory.to_path_buf(),
                listing: Digest::from_bytes([9; 32]),
                complete: true,
                overlay: false,
            }],
            ..CapturedReads::default()
        };
        reads(&mut captured);
        capture_evidence(
            &[observation],
            &captured,
            &ProjectObservations::default(),
            &AtomicBool::new(cancel),
        )
    }

    pub(in crate::workspace) fn discovery(directory: &Path) -> (DiscoveryScope, CapturedEvidence) {
        let scope = DiscoveryScope::new(&DiscoveryInputs {
            directories: &[ScannedDirectory {
                path: directory,
                recursive: false,
            }],
            exclusions: &[],
        });
        (scope, listing_capture(directory, |_| {}, false).unwrap())
    }

    /// Every document of `key` in `workspace`, persisted and checked.
    pub(in crate::workspace) fn eligible_members(
        workspace: &Workspace,
        store: &Store,
        key: &ContextKey,
    ) -> Vec<Eligible<MemberRecord>> {
        let mut uris = workspace
            .document_contexts
            .iter()
            .filter(|(_, context)| *context == key)
            .map(|(uri, _)| uri.clone())
            .collect::<Vec<_>>();
        uris.sort();
        uris.iter()
            .map(|uri| {
                workspace
                    .persist_source(uri, store, &mut budget(), &no_cancel())
                    .unwrap();
                workspace
                    .eligible_member(uri, draft(), &mut budget(), &no_cancel())
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn a_saved_member_is_eligible_and_its_header_is_the_workspace_s_own() {
        let indexed = index(
            &[
                ("Main.pas", MAIN.as_bytes()),
                ("Types.inc", TYPES.as_bytes()),
                ("Nested.inc", NESTED.as_bytes()),
            ],
            &["Main.pas"],
        );
        let uri = indexed.uri("Main.pas");
        let directory = private_dir();
        let store = open_store(directory.path());
        let prepared = indexed
            .workspace
            .prepare_artifact(&uri, &mut budget(), &no_cancel())
            .unwrap();
        prepared.publish(&store).unwrap();
        let record = indexed
            .workspace
            .eligible_member(&uri, draft(), &mut budget(), &no_cancel())
            .unwrap();
        let header = &record.value().header;
        assert_eq!(header.artifact, Some(prepared.artifact_id()));
        assert!(
            header.member_evidence.is_some(),
            "an expanded artifact names its evidence"
        );
        assert_eq!(header.member_evidence, prepared.member_evidence_id());
        assert_eq!(header.source.canonical, indexed.root.join("Main.pas"));
        // The store accepts it only over the live artifact and evidence the
        // workspace published: the IDs agree with the store's.
        assert_eq!(
            publish_record(&store, &record).unwrap().published,
            Published::Inserted
        );
    }

    #[test]
    fn an_open_overlaid_or_deleted_root_is_never_eligible() {
        let files: &[(&str, &[u8])] = &[("Cond.pas", CONDITIONAL)];
        let edited = "unit Cond;\ninterface\nconst Edited = 1;\nimplementation\nend.\n";
        let open = index_with_buffers(files, &[("Cond.pas", edited)], &["Cond.pas"]);
        let uri = open.uri("Cond.pas");
        assert_eq!(
            open.workspace
                .eligible_member(&uri, draft(), &mut budget(), &no_cancel())
                .err(),
            Some(Ineligible::SessionPrivate(uri.clone()))
        );
        let mut deleted = index(files, &["Cond.pas"]);
        let uri = deleted.uri("Cond.pas");
        deleted.workspace.remember_deleted(&uri);
        assert_eq!(
            deleted
                .workspace
                .eligible_member(&uri, draft(), &mut budget(), &no_cancel())
                .err(),
            Some(Ineligible::SessionPrivate(uri))
        );
    }

    #[test]
    fn an_overlay_provider_behind_a_saved_root_is_never_eligible() {
        let indexed = index_with_buffers(
            &[("Main.pas", PROVIDER_MAIN), ("Shapes.pas", SHAPES)],
            &[("Shapes.pas", EDITED_SHAPES)],
            &["Main.pas"],
        );
        let result = indexed.workspace.eligible_member(
            &indexed.uri("Main.pas"),
            draft(),
            &mut budget(),
            &no_cancel(),
        );
        assert!(
            matches!(
                result,
                Err(Ineligible::Artifact(
                    PersistError::Evidence(EvidenceGap::Overlay) | PersistError::NotPersistable(_)
                ))
            ),
            "{result:?}"
        );
        let shapes = indexed.uri("Shapes.pas");
        assert_eq!(
            indexed
                .workspace
                .eligible_member(&shapes, draft(), &mut budget(), &no_cancel())
                .err(),
            Some(Ineligible::SessionPrivate(shapes))
        );
    }

    #[test]
    fn a_capture_with_editor_state_overlay_overflow_or_cancellation_is_not_evidence() {
        let directory = Path::new("/w/include");
        assert!(listing_capture(directory, |_| {}, false).is_ok());
        assert_eq!(
            listing_capture(directory, |reads| reads.editor_state = true, false).err(),
            Some(EvidenceGap::Overlay)
        );
        assert_eq!(
            listing_capture(directory, |reads| reads.listings[0].overlay = true, false).err(),
            Some(EvidenceGap::Overlay)
        );
        assert_eq!(
            listing_capture(directory, |reads| reads.overflowed = true, false).err(),
            Some(EvidenceGap::Overflowed)
        );
        assert_eq!(
            listing_capture(directory, |_| {}, true).err(),
            Some(EvidenceGap::Cancelled)
        );
    }

    #[test]
    fn a_binding_must_carry_the_evidence_its_capture_proves() {
        let indexed = index(
            &[("Main.pas", PROVIDER_MAIN), ("Shapes.pas", SHAPES)],
            &["Main.pas"],
        );
        let uri = indexed.uri("Main.pas");
        let workspace = &indexed.workspace;
        let context =
            CacheContextKey::new(&workspace.contexts[&workspace.document_contexts[&uri]].context);
        let lookup = LookupKey::new(&LookupInputs {
            kind: LookupKind::Unit,
            normalized_name: "shapes",
            path_expression: None,
            importer_directory: None,
        });
        let captured = listing_capture(&indexed.root, |_| {}, false).unwrap();
        let proven = evidence_id(
            &subject(EvidencePurpose::Binding, &context, lookup.key().digest()),
            None,
            captured.observations(),
        );
        let shapes = indexed.root.join("Shapes.pas");
        let binding = |evidence| Binding {
            kind: BindingKind::Import,
            occurrence: 0,
            outcome: BindingOutcome::Precise {
                target: SourceIdentity {
                    canonical: shapes.clone(),
                    logical: shapes.clone(),
                    spelling: None,
                    uri: None,
                },
            },
            evidence: Some(evidence),
        };
        let with = |binding: Binding, proof: Option<(LookupKey, CapturedEvidence)>| MemberDraft {
            bindings: vec![CheckedBinding { binding, proof }],
            ..draft()
        };
        let check = |draft| workspace.eligible_member(&uri, draft, &mut budget(), &no_cancel());
        assert_eq!(
            check(with(
                binding(Digest::from_bytes([1; 32])),
                Some((lookup.clone(), captured.clone()))
            ))
            .err(),
            Some(Ineligible::BindingEvidence { ordinal: 0 }),
            "evidence the capture does not prove"
        );
        assert_eq!(
            check(with(binding(proven), None)).err(),
            Some(Ineligible::BindingEvidence { ordinal: 0 }),
            "evidence without its capture"
        );
        let record = check(with(binding(proven), Some((lookup, captured)))).unwrap();
        let directory = private_dir();
        let store = open_store(directory.path());
        workspace
            .persist_source(&uri, &store, &mut budget(), &no_cancel())
            .unwrap();
        let published = publish_record(&store, &record).unwrap();
        assert_eq!(published.published, Published::Inserted);
        assert!(
            matches!(store.open_evidence(proven).unwrap(), Lookup::Hit(_)),
            "the wrapper published the binding evidence first"
        );
    }

    #[test]
    fn a_complete_saved_membership_seals() {
        let indexed = units_a_and_b(&[], &["A.pas", "B.pas"]);
        let workspace = &indexed.workspace;
        let key = workspace.document_contexts[&indexed.uri("A.pas")].clone();
        let directory = private_dir();
        let store = open_store(directory.path());
        let members = eligible_members(workspace, &store, &key);
        assert!(members.len() >= 2, "A and B are members");
        let snapshot = workspace
            .eligible_snapshot(
                &key,
                SnapshotDraft {
                    members: members.clone(),
                    discovery: discovery(&indexed.root),
                    coverage: COMPLETE,
                },
            )
            .unwrap();
        assert_eq!(snapshot.value().members().len(), members.len());
        let header = seal_snapshot(&store, &snapshot).unwrap();
        assert_eq!(header.member_count(), members.len() as u64);
        let Lookup::Hit(found) = store.find_snapshots(snapshot.context()).unwrap() else {
            panic!("the sealed snapshot is found under its context")
        };
        assert_eq!(found, vec![header]);
    }

    #[test]
    fn an_edited_member_makes_the_whole_membership_ineligible() {
        let indexed = units_a_and_b(
            &[(
                "A.pas",
                "unit A;\ninterface\nconst Edited = 1;\nimplementation\nend.\n",
            )],
            &["A.pas", "B.pas"],
        );
        let workspace = &indexed.workspace;
        let (a, b) = (indexed.uri("A.pas"), indexed.uri("B.pas"));
        let key = workspace.document_contexts[&b].clone();
        assert_eq!(workspace.document_contexts[&a], key, "one context");
        let directory = private_dir();
        let store = open_store(directory.path());
        workspace
            .persist_source(&b, &store, &mut budget(), &no_cancel())
            .unwrap();
        let saved = workspace
            .eligible_member(&b, draft(), &mut budget(), &no_cancel())
            .unwrap();
        // Leaving the edited member out cannot be expressed.
        let result = workspace.eligible_snapshot(
            &key,
            SnapshotDraft {
                members: vec![saved],
                discovery: discovery(&indexed.root),
                coverage: COMPLETE,
            },
        );
        assert_eq!(result.err(), Some(Ineligible::SessionPrivate(a)));
    }

    #[test]
    fn a_membership_must_be_supplied_exactly_once_and_from_its_own_context() {
        let indexed = units_a_and_b(&[], &["A.pas", "B.pas"]);
        let workspace = &indexed.workspace;
        let key = workspace.document_contexts[&indexed.uri("A.pas")].clone();
        let directory = private_dir();
        let store = open_store(directory.path());
        let members = eligible_members(workspace, &store, &key);
        let check = |members: Vec<Eligible<MemberRecord>>| {
            workspace
                .eligible_snapshot(
                    &key,
                    SnapshotDraft {
                        members,
                        discovery: discovery(&indexed.root),
                        coverage: COMPLETE,
                    },
                )
                .err()
        };
        let first = members[0].value().header.source.canonical.clone();
        assert!(matches!(
            check(members[1..].to_vec()),
            Some(Ineligible::MissingMember(_))
        ));
        let mut twice = members.clone();
        twice.push(members[0].clone());
        assert_eq!(check(twice), Some(Ineligible::DuplicateMember(first)));
        // A member of another workspace's tree is not this context's.
        let other = units_a_and_b(&[], &["A.pas"]);
        let stranger = other
            .workspace
            .eligible_member(&other.uri("A.pas"), draft(), &mut budget(), &no_cancel())
            .unwrap();
        let mut extra = members;
        extra.push(stranger);
        assert!(matches!(
            check(extra),
            Some(Ineligible::UnexpectedMember(_) | Ineligible::OtherContext(_))
        ));
    }
}
