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
    Binding, Bypass, ContextKey as CacheContextKey, CoverageStatus, Digest, DiscoveryScope,
    EvidencePurpose, EvidenceSubject, EvidenceTarget, EvidenceWrite, IncompleteReason, LookupKey,
    MAX_ROW_BATCH_ROWS, MemberCoverage, MemberError, MemberHeader, MemberRecord, Observation,
    PublishedRecord, SnapshotCoverage, SnapshotHeader, SnapshotSeal, SourceIdentity, Store,
    encoded_content_bytes, evidence_id,
};
use pascal_core::facts::FactBudget;
use pascal_core::resolver::ResolutionObservation;
use pascal_project::{ProjectContext, ProjectObservations, ProjectPathProvenance};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
    ///
    /// A root over `maxFileBytes`, or facts over a store limit, is refused
    /// for good under the same inputs (spec §5), so the member is kept
    /// without an artifact or member evidence and its extraction is
    /// `Incomplete(OverSizeLimit)`. Every other refusal (I/O, cancellation,
    /// budget, editor state) stays ineligible, to be retried.
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
        let state = self
            .document_contexts
            .get(uri)
            .and_then(|key| self.contexts.get(key))
            .ok_or(Ineligible::NoContext)?;
        let path = uri
            .to_file_path()
            .map_err(|()| Ineligible::Artifact(PersistError::NotIndexed))?;
        let prepared = if self.root_over_size_limit(&path, &state.context) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Ineligible::Artifact(PersistError::Cancelled));
            }
            None
        } else {
            let prepared = self
                .prepare_artifact(uri, budget, cancel)
                .map_err(Ineligible::Artifact)?;
            encoded_content_bytes(&prepared.facts)
                .is_ok()
                .then_some(prepared)
        };
        let mut coverage = draft.coverage;
        let (artifact, member_evidence, origin) = match &prepared {
            Some(prepared) => (
                Some(prepared.artifact_id()),
                prepared.member_evidence_id(),
                prepared.origin,
            ),
            None => {
                coverage.extraction = CoverageStatus::Incomplete(IncompleteReason::OverSizeLimit);
                (None, None, state.context.source_origin(&path))
            }
        };
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
        let canonical = super::absolute_path(path);
        let record = MemberRecord {
            header: MemberHeader {
                source: SourceIdentity {
                    canonical: canonical.clone(),
                    logical: canonical,
                    spelling: None,
                    uri: Some(uri.to_string()),
                },
                artifact,
                member_evidence,
                origin,
                fallback_unit_name: draft.fallback_unit_name,
                coverage,
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

    /// Whether the saved root at `path` exceeds `maxFileBytes`, by its
    /// metadata alone, so nothing oversized is read. A root `context` does
    /// not authorize, or whose metadata cannot be read, is not:
    /// `prepare_artifact` refuses it.
    fn root_over_size_limit(&self, path: &Path, context: &ProjectContext) -> bool {
        let Some(entry) = context.path_entry_for(path) else {
            return false;
        };
        let policy = context.read_policy();
        let metadata = if matches!(entry.provenance, ProjectPathProvenance::LegacyNative) {
            if !policy.allows_legacy_payload_entry(&entry) {
                return false;
            }
            std::fs::metadata(&entry.path)
        } else {
            if !policy.allows_entry(&entry) {
                return false;
            }
            std::fs::symlink_metadata(&entry.path)
        };
        metadata.is_ok_and(|metadata| {
            metadata.is_file() && metadata.len() > self.options.limits.max_file_bytes as u64
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
        BindingKind, BindingOutcome, CoverageStatus, DiscoveryInputs, IncompleteReason, Lookup,
        LookupInputs, LookupKind, Published, ScannedDirectory,
    };
    use pascal_core::facts::FactError;
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
        let stranger_path = stranger.value().header.source.canonical.clone();
        let mut extra = members;
        extra.push(stranger);
        assert_eq!(check(extra), Some(Ineligible::OtherContext(stranger_path)));
    }

    #[test]
    fn a_member_of_this_context_outside_the_derived_membership_is_unexpected() {
        let mut indexed = units_a_and_b(&[], &["A.pas", "B.pas"]);
        let (a, b) = (indexed.uri("A.pas"), indexed.uri("B.pas"));
        let key = indexed.workspace.document_contexts[&a].clone();
        let directory = private_dir();
        let store = open_store(directory.path());
        let members = eligible_members(&indexed.workspace, &store, &key);
        // B is checked under the context, then the workspace stops holding
        // it: its member is the context's own but no longer a document.
        indexed.workspace.document_contexts.remove(&b);
        let result = indexed.workspace.eligible_snapshot(
            &key,
            SnapshotDraft {
                members,
                discovery: discovery(&indexed.root),
                coverage: COMPLETE,
            },
        );
        assert_eq!(
            result.err(),
            Some(Ineligible::UnexpectedMember(indexed.root.join("B.pas")))
        );
    }

    fn incomplete_binding(occurrence: u64, evidence: Option<Digest>) -> Binding {
        Binding {
            kind: BindingKind::Import,
            occurrence,
            outcome: BindingOutcome::Incomplete(IncompleteReason::Unsupported),
            evidence,
        }
    }

    #[test]
    fn a_capture_for_a_binding_without_evidence_is_refused_at_its_ordinal() {
        let indexed = units_a_and_b(&[], &["A.pas"]);
        let lookup = LookupKey::new(&LookupInputs {
            kind: LookupKind::Unit,
            normalized_name: "b",
            path_expression: None,
            importer_directory: None,
        });
        let captured = listing_capture(&indexed.root, |_| {}, false).unwrap();
        let draft = MemberDraft {
            bindings: vec![
                CheckedBinding {
                    binding: incomplete_binding(0, None),
                    proof: None,
                },
                CheckedBinding {
                    binding: incomplete_binding(1, None),
                    proof: Some((lookup, captured)),
                },
            ],
            ..draft()
        };
        assert_eq!(
            indexed
                .workspace
                .eligible_member(&indexed.uri("A.pas"), draft, &mut budget(), &no_cancel())
                .err(),
            Some(Ineligible::BindingEvidence { ordinal: 1 })
        );
    }

    #[test]
    fn a_record_the_store_format_rejects_is_ineligible() {
        let indexed = units_a_and_b(&[], &["A.pas"]);
        let target = |name: &str| SourceIdentity {
            canonical: indexed.root.join(name),
            logical: indexed.root.join(name),
            spelling: None,
            uri: None,
        };
        let lookup = LookupKey::new(&LookupInputs {
            kind: LookupKind::Unit,
            normalized_name: "b",
            path_expression: None,
            importer_directory: None,
        });
        let captured = listing_capture(&indexed.root, |_| {}, false).unwrap();
        let uri = indexed.uri("A.pas");
        let workspace = &indexed.workspace;
        let context =
            CacheContextKey::new(&workspace.contexts[&workspace.document_contexts[&uri]].context);
        let proven = evidence_id(
            &subject(EvidencePurpose::Binding, &context, lookup.key().digest()),
            None,
            captured.observations(),
        );
        let draft = MemberDraft {
            bindings: vec![CheckedBinding {
                binding: Binding {
                    kind: BindingKind::Import,
                    occurrence: 0,
                    outcome: BindingOutcome::Ambiguous {
                        candidates: vec![target("B.pas")],
                        overflow: false,
                    },
                    evidence: Some(proven),
                },
                proof: Some((lookup, captured)),
            }],
            ..draft()
        };
        assert_eq!(
            workspace
                .eligible_member(&uri, draft, &mut budget(), &no_cancel())
                .err(),
            Some(Ineligible::Record(MemberError::Candidates { ordinal: 0 }))
        );
    }

    const OVER_SIZE: CoverageStatus = CoverageStatus::Incomplete(IncompleteReason::OverSizeLimit);

    /// `units_a_and_b`, then `B.pas` grows on disk past a lowered
    /// `maxFileBytes`.
    fn oversize_b(buffers: &[(&str, &str)]) -> Indexed {
        let mut indexed = units_a_and_b(buffers, &["A.pas", "B.pas"]);
        indexed.workspace.options.limits.max_file_bytes = 1024;
        let grown = format!(
            "unit B;\ninterface\n{{{}}}\nimplementation\nend.\n",
            "x".repeat(2048)
        );
        std::fs::write(indexed.root.join("B.pas"), grown).unwrap();
        indexed
    }

    #[test]
    fn an_oversize_root_is_an_artifact_less_member_and_its_membership_seals() {
        let indexed = oversize_b(&[]);
        let workspace = &indexed.workspace;
        let b = indexed.uri("B.pas");
        let key = workspace.document_contexts[&b].clone();
        let directory = private_dir();
        let store = open_store(directory.path());
        let artifact_less = workspace
            .eligible_member(&b, draft(), &mut budget(), &no_cancel())
            .unwrap();
        let header = &artifact_less.value().header;
        assert_eq!(header.artifact, None);
        assert_eq!(header.member_evidence, None);
        assert_eq!(header.coverage.extraction, OVER_SIZE);
        assert_eq!(
            header.coverage.resolution,
            CoverageStatus::Complete,
            "the draft's resolution coverage is kept"
        );
        let path = indexed.root.join("B.pas");
        assert_eq!(header.source.canonical, path);
        assert_eq!(
            header.origin,
            workspace.contexts[&key].context.source_origin(&path)
        );
        assert_eq!(
            publish_record(&store, &artifact_less).unwrap().published,
            Published::Inserted
        );
        let mut members = workspace
            .document_contexts
            .iter()
            .filter(|(uri, context)| **context == key && **uri != b)
            .map(|(uri, _)| {
                workspace
                    .persist_source(uri, &store, &mut budget(), &no_cancel())
                    .unwrap();
                workspace
                    .eligible_member(uri, draft(), &mut budget(), &no_cancel())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(!members.is_empty(), "A is a member");
        members.push(artifact_less);
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
        let header = seal_snapshot(&store, &snapshot).unwrap();
        assert_eq!(header.member_count(), members.len() as u64);
    }

    #[test]
    fn facts_over_a_store_limit_make_an_artifact_less_member() {
        let name = "L".repeat(pascal_cache::MAX_INDEX_KEY_BYTES + 1);
        let source = format!("unit Long;\ninterface\nconst {name} = 1;\nimplementation\nend.\n");
        let indexed = index(&[("Long.pas", source.as_bytes())], &["Long.pas"]);
        let uri = indexed.uri("Long.pas");
        let facts = indexed.facts("Long.pas").extracted;
        assert_eq!(facts.persistable, Ok(()));
        assert!(
            pascal_cache::encoded_content_bytes(&facts.facts).is_err(),
            "the store cannot hold these facts"
        );
        let member = indexed
            .workspace
            .eligible_member(&uri, draft(), &mut budget(), &no_cancel())
            .unwrap();
        let header = &member.value().header;
        assert_eq!(header.artifact, None);
        assert_eq!(header.member_evidence, None);
        assert_eq!(header.coverage.extraction, OVER_SIZE);
        let directory = private_dir();
        let store = open_store(directory.path());
        assert_eq!(
            publish_record(&store, &member).unwrap().published,
            Published::Inserted
        );
        // Admission still comes first: a refused budget is retried.
        assert!(matches!(
            indexed.workspace.eligible_member(
                &uri,
                draft(),
                &mut FactBudget::new(16),
                &no_cancel()
            ),
            Err(Ineligible::Artifact(PersistError::Fact(
                FactError::Budget { .. }
            )))
        ));
    }

    #[test]
    fn transient_artifact_refusals_stay_ineligible() {
        let indexed = units_a_and_b(&[], &["A.pas", "B.pas"]);
        let (a, b) = (indexed.uri("A.pas"), indexed.uri("B.pas"));
        let workspace = &indexed.workspace;
        let cancelled =
            workspace.eligible_member(&a, draft(), &mut budget(), &AtomicBool::new(true));
        assert!(
            matches!(
                cancelled,
                Err(Ineligible::Artifact(
                    PersistError::Cancelled | PersistError::Fact(FactError::Cancelled)
                ))
            ),
            "{cancelled:?}"
        );
        let tight = workspace.eligible_member(&a, draft(), &mut FactBudget::new(16), &no_cancel());
        assert!(
            matches!(
                tight,
                Err(Ineligible::Artifact(PersistError::Fact(
                    FactError::Budget { .. }
                )))
            ),
            "{tight:?}"
        );
        std::fs::remove_file(indexed.root.join("B.pas")).unwrap();
        let unreadable = workspace.eligible_member(&b, draft(), &mut budget(), &no_cancel());
        assert!(
            matches!(
                unreadable,
                Err(Ineligible::Artifact(PersistError::Unreadable(_)))
            ),
            "{unreadable:?}"
        );
        let oversize = oversize_b(&[]);
        assert_eq!(
            oversize
                .workspace
                .eligible_member(
                    &oversize.uri("B.pas"),
                    draft(),
                    &mut budget(),
                    &AtomicBool::new(true)
                )
                .err(),
            Some(Ineligible::Artifact(PersistError::Cancelled))
        );
    }

    #[test]
    fn an_open_oversize_root_stays_session_private() {
        let indexed = oversize_b(&[(
            "B.pas",
            "unit B;\ninterface\nconst Edited = 1;\nimplementation\nend.\n",
        )]);
        let b = indexed.uri("B.pas");
        assert_eq!(
            indexed
                .workspace
                .eligible_member(&b, draft(), &mut budget(), &no_cancel())
                .err(),
            Some(Ineligible::SessionPrivate(b))
        );
    }
}
