//! Publishing a source's facts with the evidence that proves them, and
//! reusing persisted facts only after current inputs validate (spec §7).

use super::evidence::EvidenceGap;
use super::facts::SourceFacts;
use super::indexing::{ReusePhase, timed};
use super::validate::{
    CurrentInputs, EvidenceCheck, Stale, Unproven, Validator, validate_evidence,
};
use super::{ContextKey, PathStamp, Workspace};
use crate::persistent_cache::counters::MissCause;
use crate::project_cache::{Reservation, ReservationKind};
use lsp_types::Url;
use pascal_cache::{
    ArtifactKeyRef, Bypass, ContextKey as CacheContextKey, Digest, EvidencePurpose,
    EvidenceSubject, EvidenceWrite, ExpandedArtifactKey, KeyError, LocalArtifactKey, Lookup,
    Malformed, Miss, Observation, Published, PublishedEvidence, SessionDependence, SourceContent,
    Store, evidence_id, observations_digest,
};
use pascal_core::facts::{ArtifactFacts, FactBudget, FactError, NotPersistable, include_slots};
use pascal_core::resolver::ResolverLimits;
use pascal_project::{ProjectContext, ProjectPathProvenance, ProjectWorkBudget, SourceOrigin};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Persisted {
    Local(Published),
    Expanded(PublishedEvidence),
}

/// What `persist_source` publishes, prepared without the store: the facts,
/// their key and, for an expanded artifact, the evidence that proves it.
/// `workspace::eligible` derives a member record's header from it.
pub(crate) struct PreparedArtifact {
    pub(crate) facts: ArtifactFacts,
    pub(crate) origin: SourceOrigin,
    local: LocalArtifactKey,
    expanded: Option<ExpandedArtifact>,
}

struct ExpandedArtifact {
    key: ExpandedArtifactKey,
    context: CacheContextKey,
    observations: Vec<Observation>,
}

impl PreparedArtifact {
    pub(crate) fn key(&self) -> ArtifactKeyRef<'_> {
        match &self.expanded {
            None => ArtifactKeyRef::local(&self.local),
            Some(expanded) => ArtifactKeyRef::expanded(&expanded.key, &self.local)
                .expect("an expanded key built from this root names it"),
        }
    }

    pub(crate) fn artifact_id(&self) -> Digest {
        self.key().digest()
    }

    /// What preparing this artifact holds beyond its facts' own charge:
    /// the facts' encoded content and the observations' encoded sizes.
    pub(crate) fn transient_bytes(&self) -> usize {
        let facts = pascal_cache::encoded_content_bytes(&self.facts).unwrap_or(usize::MAX);
        let observations = self.expanded.as_ref().map_or(0, |expanded| {
            expanded
                .observations
                .iter()
                .fold(0usize, |sum, observation| {
                    sum.saturating_add(observation.encoded().len())
                })
        });
        facts.saturating_add(observations)
    }

    /// The paths the evidence names besides the root: what a change to
    /// makes this artifact stale. A local artifact names none.
    pub(crate) fn input_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.expanded.as_ref().map_or_else(Vec::new, |expanded| {
            expanded
                .observations
                .iter()
                .map(|observation| super::absolute_path(observation.path().to_path_buf()))
                .collect()
        });
        paths.sort();
        paths.dedup();
        paths
    }

    /// The ID `Store::publish_facts_with_evidence` gives this artifact's
    /// evidence; `None` for a local artifact, which is its own proof.
    pub(crate) fn member_evidence_id(&self) -> Option<Digest> {
        let expanded = self.expanded.as_ref()?;
        Some(evidence_id(
            &EvidenceSubject {
                purpose: EvidencePurpose::Artifact,
                context: expanded.context.digest(),
                subject: self.local.digest(),
            },
            Some(self.artifact_id()),
            &expanded.observations,
        ))
    }

    /// Publish under the local key, or the expanded key with its evidence
    /// in one transaction (spec §6.2).
    pub(crate) fn publish(&self, store: &Store) -> Result<Persisted, PersistError> {
        let published = match &self.expanded {
            None => store
                .publish_facts(self.key(), &self.facts)
                .map(Persisted::Local),
            Some(expanded) => store
                .publish_facts_with_evidence(
                    self.key(),
                    &self.facts,
                    &EvidenceWrite {
                        context: &expanded.context,
                        observations: &expanded.observations,
                    },
                )
                .map(Persisted::Expanded),
        };
        published.map_err(PersistError::Store)
    }
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
    /// Context discovery failed (`reuse_persisted_facts`).
    #[cfg(test)]
    Context(String),
    NotIndexed,
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reuse {
    Hit {
        facts: AdmittedFacts,
        via: ReuseVia,
        /// Payloads and listings this call read rather than witnessed,
        /// summed over every evidence candidate it validated, including
        /// candidates rejected before the hit. A cost measure, not a
        /// property of the facts returned.
        reads: usize,
        /// The loaded artifact's ID: the local key's digest, or the
        /// evidence header's `artifact_id()`.
        artifact: Digest,
        /// The validated header's `evidence_id()`; `None` via `Local`.
        evidence: Option<Digest>,
        /// The stamp taken before each include payload that held, in
        /// check order; the root is not listed.
        include_stamps: Vec<(PathBuf, PathStamp)>,
        /// `content_hash_bytes` of the root bytes this call read.
        root_hash: u64,
    },
    Miss(ReuseMiss),
}

/// Decoded persisted facts and their charge to `maxCacheBytes` (spec §10
/// amendment 2026-11-02). The charge lives exactly as long as this value,
/// however long a worker snapshot keeps it; eviction never releases it.
pub(crate) struct AdmittedFacts {
    pub(crate) facts: Arc<ArtifactFacts>,
    pub(crate) charge: Reservation,
}

impl std::fmt::Debug for AdmittedFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdmittedFacts")
            .field("facts", &self.facts)
            .field("charged", &self.charge.bytes())
            .finish()
    }
}

/// The charge is accounting, not content.
impl PartialEq for AdmittedFacts {
    fn eq(&self, other: &Self) -> bool {
        self.facts == other.facts
    }
}

impl Eq for AdmittedFacts {}

/// Keep `charge` at what decoding `facts` used.
pub(super) fn admitted(
    facts: ArtifactFacts,
    budget: &FactBudget,
    mut charge: Reservation,
) -> AdmittedFacts {
    charge.shrink_to(budget.used());
    AdmittedFacts {
        facts: Arc::new(facts),
        charge,
    }
}

/// A decoder refusal for size is a memory miss; any other bypass stays an
/// error.
pub(super) fn decoded(
    result: Result<Lookup<ArtifactFacts>, Bypass>,
) -> Result<Result<Lookup<ArtifactFacts>, ReuseMiss>, PersistError> {
    match result {
        Ok(lookup) => Ok(Ok(lookup)),
        Err(Bypass::FactBudget) => Ok(Err(ReuseMiss::Unproven(Unproven::Memory))),
        Err(bypass) => Err(PersistError::Store(bypass)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReuseVia {
    /// The key from the current root bytes is its own proof.
    Local,
    /// Evidence validated against current inputs.
    Evidence,
    /// The record a held baseline snapshot lists for the member, its
    /// evidence (or local key) validated against current inputs (D31).
    /// `record` is that record, found live and authorized, and the
    /// snapshot, when this process's lease held it (13f).
    Snapshot {
        record: Option<(pascal_cache::SnapshotId, Digest)>,
    },
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

impl ReuseMiss {
    /// The diagnostic cause (spec §11.2 amendment 2026-11-03, D27).
    pub(crate) fn cause(&self) -> MissCause {
        use MissCause::*;
        match self {
            Self::Open | Self::Store(Miss::Absent) | Self::Stale(Stale::Context) => Input,
            Self::Store(Miss::KeyMismatch(_)) => Version,
            Self::Stale(Stale::Unauthorized(_)) => Authorization,
            Self::Stale(_) | Self::Store(Miss::Retired | Miss::Vanished) => Freshness,
            Self::Unproven(_) | Self::Store(Miss::Oversized) => Budget,
            Self::Store(Miss::Malformed(_) | Miss::ChecksumMismatch) => Integrity,
        }
    }
}

impl PersistError {
    /// A store bypass is counted as a bypass, everything else as a miss.
    pub(crate) fn cause(&self) -> Result<MissCause, &Bypass> {
        use MissCause::*;
        match self {
            Self::Store(bypass) => Err(bypass),
            Self::Unauthorized => Ok(Authorization),
            Self::RootChanged => Ok(Freshness),
            Self::Cancelled => Ok(Budget),
            Self::NotPersistable(_)
            | Self::Fact(_)
            | Self::Evidence(_)
            | Self::Key(_)
            | Self::Unreadable(_)
            | Self::NotIndexed => Ok(Input),
            #[cfg(test)]
            Self::Context(_) => Ok(Input),
        }
    }
}

#[cfg(test)]
thread_local! {
    static REUSE_IN_CONTEXT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `reuse_in_context` calls on this thread (the evidence-candidate search).
#[cfg(test)]
pub(crate) fn reuse_in_context_calls_for_test() -> usize {
    REUSE_IN_CONTEXT_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_reuse_in_context_calls_for_test() {
    REUSE_IN_CONTEXT_CALLS.with(|calls| calls.set(0));
}

/// The store's key of one installed context, encoded once (A5: the reuse
/// pass looks up every member of a context).
#[derive(Default)]
pub(crate) struct ContextKeyMemo(Option<(Arc<ProjectContext>, CacheContextKey)>);

impl ContextKeyMemo {
    fn key_of(&mut self, context: &Arc<ProjectContext>) -> &CacheContextKey {
        if !self
            .0
            .as_ref()
            .is_some_and(|(held, _)| Arc::ptr_eq(held, context))
        {
            self.0 = Some((Arc::clone(context), CacheContextKey::new(context)));
        }
        &self.0.as_ref().expect("just set").1
    }
}

/// The store's key of the context a view last asked for, shared by its
/// threads (13f): a context is immutable behind its `Arc`, so a batch
/// encodes its key once, not once per member it proves. The context is
/// held weakly, so the memo keeps no context alive; the held allocation
/// keeps its address from being reused by another context.
#[derive(Default)]
pub(crate) struct SharedContextKey {
    memo: std::sync::Mutex<Option<(std::sync::Weak<ProjectContext>, CacheContextKey)>>,
    #[cfg(test)]
    encodings: std::sync::atomic::AtomicUsize,
}

impl SharedContextKey {
    pub(crate) fn key_of(&self, context: &Arc<ProjectContext>) -> CacheContextKey {
        let mut memo = self
            .memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((held, key)) = memo.as_ref()
            && std::ptr::eq(held.as_ptr(), Arc::as_ptr(context))
        {
            return key.clone();
        }
        #[cfg(test)]
        self.encodings.fetch_add(1, Ordering::Relaxed);
        let key = CacheContextKey::new(context);
        *memo = Some((Arc::downgrade(context), key.clone()));
        key
    }

    /// How many keys this memo encoded.
    #[cfg(test)]
    pub(crate) fn encodings(&self) -> usize {
        self.encodings.load(Ordering::Relaxed)
    }
}

impl Workspace {
    /// The store's key of `context`, encoded once per installed context.
    pub(crate) fn cache_context_key(&self, context: &Arc<ProjectContext>) -> CacheContextKey {
        self.cache_context_key.key_of(context)
    }

    #[cfg(test)]
    pub(crate) fn cache_context_key_encodings(&self) -> usize {
        self.cache_context_key.encodings()
    }

    /// The saved root bytes of `uri`, read under `context`'s current policy.
    pub(super) fn saved_root_bytes(
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
    #[cfg(test)]
    pub(crate) fn persist_source(
        &self,
        uri: &Url,
        store: &Store,
        budget: &mut FactBudget,
        cancel: &AtomicBool,
    ) -> Result<Persisted, PersistError> {
        self.prepare_artifact(uri, budget, cancel)?.publish(store)
    }

    /// Everything `persist_source` checks and builds before it publishes.
    #[cfg(test)]
    pub(crate) fn prepare_artifact(
        &self,
        uri: &Url,
        budget: &mut FactBudget,
        cancel: &AtomicBool,
    ) -> Result<PreparedArtifact, PersistError> {
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
        self.prepare_from_facts(uri, source, cancel)
    }

    /// `prepare_artifact` over facts already extracted, so one extraction
    /// pass serves a member's publication and its own accounting.
    pub(crate) fn prepare_from_facts(
        &self,
        uri: &Url,
        source: SourceFacts,
        cancel: &AtomicBool,
    ) -> Result<PreparedArtifact, PersistError> {
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
            return Ok(PreparedArtifact {
                facts: source.extracted.facts,
                origin: source.origin,
                local,
                expanded: None,
            });
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
        let context = self.cache_context_key(&state.context);
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
        if cancel.load(Ordering::Relaxed) {
            return Err(PersistError::Cancelled);
        }
        Ok(PreparedArtifact {
            facts: source.extracted.facts,
            origin: source.origin,
            local,
            expanded: Some(ExpandedArtifact {
                key: expanded,
                context,
                observations,
            }),
        })
    }

    /// Whether `uri` is open, overlaid or deleted in this session's editor:
    /// its view is session-private (spec §4.4).
    pub(super) fn root_is_session_private(&self, uri: &Url) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        let deleted = super::resolver::deletion_overrides_by_path(&self.deleted_overrides);
        self.is_open(uri)
            || self
                .overlay_inputs()
                .contains_key(&super::canonical_file_uri(uri))
            || super::resolver::deletion_applies(&deleted, &super::absolute_path(path))
    }

    /// `uri`'s persisted facts, only if current inputs prove them (spec §7):
    /// the current context and root bytes select the keys, and evidence
    /// must validate under the current read policy and editor state.
    /// Nothing is indexed or parsed.
    ///
    /// `decode_limit` bytes are reserved from `maxCacheBytes` before the
    /// first decode and held until this returns; a hit keeps only what
    /// decoding used, for as long as its facts live (spec §10 amendment
    /// 2026-11-02). A refused reservation, or facts over the limit, is
    /// `Unproven::Memory`; cancellation, including during context
    /// discovery, is `Unproven::Cancelled` (l4d-ne50). Both are retryable.
    #[cfg(test)]
    pub(crate) fn reuse_persisted_facts(
        &mut self,
        uri: &Url,
        store: &Store,
        work: &dyn ProjectWorkBudget,
        decode_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Reuse, PersistError> {
        uri.to_file_path().map_err(|()| PersistError::NotIndexed)?;
        if self.root_is_session_private(uri) {
            return Ok(Reuse::Miss(ReuseMiss::Open));
        }
        let key = match self.context_for_uri_with_cancel(uri, Some(cancel)) {
            Ok(key) => key,
            Err(_) if cancel.load(Ordering::Relaxed) => {
                return Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Cancelled)));
            }
            Err(message) => return Err(PersistError::Context(message)),
        };
        self.reuse_in_context(uri, &key, store, work, decode_limit, cancel)
    }

    /// `reuse_persisted_facts` under `key`, the context that lists `uri`
    /// (an index context), without discovering `uri`'s own owner. `key`
    /// must already be installed. A hit also reports the artifact and
    /// evidence IDs, the stamps taken before each include's bytes were
    /// validated, and the root's content hash.
    #[cfg(test)]
    pub(crate) fn reuse_in_context(
        &self,
        uri: &Url,
        key: &ContextKey,
        store: &Store,
        work: &dyn ProjectWorkBudget,
        decode_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Reuse, PersistError> {
        self.reuse_in_context_with(
            uri,
            key,
            &mut ContextKeyMemo::default(),
            store,
            work,
            decode_limit,
            cancel,
        )
    }

    /// `reuse_in_context`, encoding the store's context key once for all
    /// the calls that share `memo` and the same installed context.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reuse_in_context_with(
        &self,
        uri: &Url,
        key: &ContextKey,
        memo: &mut ContextKeyMemo,
        store: &Store,
        work: &dyn ProjectWorkBudget,
        decode_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Reuse, PersistError> {
        #[cfg(test)]
        REUSE_IN_CONTEXT_CALLS.with(|calls| calls.set(calls.get() + 1));
        let path = uri.to_file_path().map_err(|()| PersistError::NotIndexed)?;
        if self.root_is_session_private(uri) {
            return Ok(Reuse::Miss(ReuseMiss::Open));
        }
        let context = Arc::clone(
            &self
                .contexts
                .get(key)
                .ok_or(PersistError::NotIndexed)?
                .context,
        );
        let (root_hash, local) = timed(ReusePhase::ReadHash, || {
            let bytes = self.saved_root_bytes(uri, &context)?;
            let root_hash = pascal_project::content_hash_bytes(&bytes);
            let local = LocalArtifactKey::new(
                &SourceContent::from_bytes(&bytes),
                &super::conditional_context_for_uri(&context, uri),
                context.source_origin(&path),
            );
            Ok::<_, PersistError>((root_hash, local))
        })?;
        let Some(charge) = self
            .project_cache
            .reserve(ReservationKind::Facts, decode_limit)
        else {
            return Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Memory)));
        };
        let mut budget = FactBudget::new(decode_limit);
        match decoded(timed(ReusePhase::Decode, || {
            store.load_facts(ArtifactKeyRef::local(&local), &mut budget)
        }))? {
            Ok(Lookup::Hit(facts)) => {
                return Ok(Reuse::Hit {
                    facts: admitted(facts, &budget, charge),
                    via: ReuseVia::Local,
                    reads: 0,
                    artifact: local.digest(),
                    evidence: None,
                    include_stamps: Vec::new(),
                    root_hash,
                });
            }
            Ok(Lookup::Miss(_)) => {}
            Err(miss) => return Ok(Reuse::Miss(miss)),
        }
        let context_key = memo.key_of(&context);
        let headers = match timed(ReusePhase::Lookup, || {
            store.find_evidence(context_key, &local)
        })
        .map_err(PersistError::Store)?
        {
            Lookup::Hit(headers) => headers,
            Lookup::Miss(miss) => return Ok(Reuse::Miss(ReuseMiss::Store(miss))),
        };
        // Only a member with candidates pays for the editor state (A5).
        let overlays = self.overlay_inputs();
        let deleted = super::resolver::deletion_overrides_by_path(&self.deleted_overrides);
        let mut validator = Validator::new(
            CurrentInputs {
                context: context_key,
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
        for header in &headers {
            let Some(artifact) = header.artifact_id() else {
                last = ReuseMiss::Store(Miss::Malformed(Malformed::Evidence));
                continue;
            };
            let held_before = validator.held_payloads().len();
            match timed(ReusePhase::Validation, || {
                validate_evidence(store, header, &mut validator)
            })
            .map_err(PersistError::Store)?
            {
                EvidenceCheck::Holds => {}
                EvidenceCheck::Stale(stale) => {
                    last = ReuseMiss::Stale(stale);
                    continue;
                }
                EvidenceCheck::Unproven(unproven) => {
                    return Ok(Reuse::Miss(ReuseMiss::Unproven(unproven)));
                }
                EvidenceCheck::Store(miss) => {
                    last = ReuseMiss::Store(miss);
                    continue;
                }
            }
            let include_stamps = validator.held_payloads()[held_before..]
                .iter()
                .filter(|(held, _)| !super::resolver::path_equivalent(held, &path))
                .cloned()
                .collect();
            return Ok(
                match decoded(timed(ReusePhase::Decode, || {
                    store.load_facts_by_id(artifact, &mut budget)
                }))? {
                    Ok(Lookup::Hit(facts)) => Reuse::Hit {
                        facts: admitted(facts, &budget, charge),
                        via: ReuseVia::Evidence,
                        reads: validator.reads(),
                        artifact,
                        evidence: Some(header.evidence_id()),
                        include_stamps,
                        root_hash,
                    },
                    Ok(Lookup::Miss(miss)) => Reuse::Miss(ReuseMiss::Store(miss)),
                    Err(miss) => Reuse::Miss(miss),
                },
            );
        }
        Ok(Reuse::Miss(last))
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
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

    pub(crate) fn private_dir() -> tempfile::TempDir {
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

    pub(in crate::workspace) const DECODE_LIMIT: usize = 1 << 26;

    pub(in crate::workspace) fn reuse(
        workspace: &mut Workspace,
        uri: &Url,
        store: &Store,
    ) -> Result<Reuse, PersistError> {
        let cancel = Arc::new(AtomicBool::new(false));
        let work = ReconciliationBudget::new(Arc::clone(&cancel));
        workspace.reuse_persisted_facts(uri, store, &work, DECODE_LIMIT, &cancel)
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
    fn a_context_key_memo_encodes_once_per_installed_context() {
        let one = Arc::new(ProjectContext::default());
        let other = Arc::new(ProjectContext::default());
        let mut memo = ContextKeyMemo::default();
        assert_eq!(*memo.key_of(&one), CacheContextKey::new(&one));
        assert!(
            memo.0
                .as_ref()
                .is_some_and(|(held, _)| Arc::ptr_eq(held, &one))
        );
        assert_eq!(*memo.key_of(&one), CacheContextKey::new(&one));
        assert_eq!(*memo.key_of(&other), CacheContextKey::new(&other));
        assert!(
            memo.0
                .as_ref()
                .is_some_and(|(held, _)| Arc::ptr_eq(held, &other)),
            "another installed context is encoded again"
        );
    }

    #[test]
    fn a_shared_context_key_encodes_once_per_installed_context() {
        let shared = SharedContextKey::default();
        let one = Arc::new(ProjectContext::default());
        assert_eq!(shared.key_of(&one), CacheContextKey::new(&one));
        assert_eq!(shared.key_of(&one), CacheContextKey::new(&one));
        assert_eq!(shared.encodings(), 1);
        let other = Arc::new(ProjectContext::default());
        assert_eq!(shared.key_of(&other), CacheContextKey::new(&other));
        assert_eq!(shared.encodings(), 2, "another installed context");
        // The memo holds the context weakly: once dropped, a context
        // installed later is encoded again, never given the old key.
        drop(other);
        let later = Arc::new(ProjectContext::default());
        assert_eq!(shared.key_of(&later), CacheContextKey::new(&later));
        assert_eq!(shared.encodings(), 3);
    }

    #[test]
    fn reuse_misses_map_to_their_diagnostic_causes() {
        use crate::persistent_cache::counters::MissCause::*;
        use pascal_cache::{Malformed, Mismatch, Miss};
        let path = std::path::PathBuf::from("/p/A.pas");
        for (miss, cause) in [
            (ReuseMiss::Open, Input),
            (ReuseMiss::Store(Miss::Absent), Input),
            (ReuseMiss::Stale(Stale::Context), Input),
            (
                ReuseMiss::Store(Miss::KeyMismatch(Mismatch::DigestDiffers)),
                Version,
            ),
            (ReuseMiss::Store(Miss::Retired), Freshness),
            (ReuseMiss::Store(Miss::Vanished), Freshness),
            (ReuseMiss::Stale(Stale::Content(path.clone())), Freshness),
            (ReuseMiss::Stale(Stale::Overlay(path.clone())), Freshness),
            (
                ReuseMiss::Stale(Stale::Unauthorized(path.clone())),
                Authorization,
            ),
            (ReuseMiss::Unproven(Unproven::Memory), Budget),
            (ReuseMiss::Unproven(Unproven::Cancelled), Budget),
            (ReuseMiss::Store(Miss::Oversized), Budget),
            (ReuseMiss::Store(Miss::Malformed(Malformed::Row)), Integrity),
            (ReuseMiss::Store(Miss::ChecksumMismatch), Integrity),
        ] {
            assert_eq!(miss.cause(), cause, "{miss:?}");
        }
        assert_eq!(PersistError::Unauthorized.cause(), Ok(Authorization));
        assert_eq!(PersistError::RootChanged.cause(), Ok(Freshness));
        assert_eq!(PersistError::Cancelled.cause(), Ok(Budget));
        assert_eq!(
            PersistError::Store(Bypass::Busy).cause(),
            Err(&Bypass::Busy)
        );
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
                ReuseVia::Snapshot { .. } => unreachable!("no snapshot is held"),
            }
            let expected = indexed.facts(name).extracted.facts;
            drop(store);
            let store = open_store(directory.path());
            let mut restarted = restart(&indexed);
            match reuse(&mut restarted, &indexed.uri(name), &store).unwrap() {
                Reuse::Hit { facts, via, .. } => {
                    assert_eq!(via, expected_via, "{name}");
                    assert_eq!(*facts.facts, expected, "{name}");
                }
                miss => panic!("{name}: {miss:?}"),
            }
        }
    }

    /// D31: a library unit whose discovered owner is another context is reused
    /// under the index context that lists it, never under its own owner.
    #[test]
    fn reuse_in_context_reports_ids_stamps_and_root_hash() {
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
        persist(&indexed, "Main.pas", &store);
        let uri = indexed.uri("Main.pas");
        let key = indexed.workspace.document_contexts[&uri].clone();
        let mut restarted = restart(&indexed);
        restarted.context_for_uri_with_cancel(&uri, None).unwrap();
        assert!(restarted.contexts.contains_key(&key));
        let cancel = Arc::new(AtomicBool::new(false));
        let work = ReconciliationBudget::new(Arc::clone(&cancel));
        let reuse = restarted
            .reuse_in_context(&uri, &key, &store, &work, DECODE_LIMIT, &cancel)
            .unwrap();
        let Reuse::Hit {
            evidence,
            include_stamps,
            via,
            artifact,
            root_hash,
            ..
        } = reuse
        else {
            panic!("{reuse:?}")
        };
        assert_eq!(via, ReuseVia::Evidence);
        let prepared = indexed
            .workspace
            .prepare_artifact(&uri, &mut FactBudget::new(1 << 26), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(evidence, prepared.member_evidence_id());
        assert!(evidence.is_some());
        assert_eq!(artifact, prepared.artifact_id());
        let observations = &prepared.expanded.as_ref().unwrap().observations;
        assert!(!observations.is_empty());
        assert_eq!(
            prepared.transient_bytes(),
            pascal_cache::encoded_content_bytes(&prepared.facts).unwrap()
                + observations
                    .iter()
                    .map(|observation| observation.encoded().len())
                    .sum::<usize>()
        );
        let bytes = std::fs::read(indexed.root.join("Main.pas")).unwrap();
        assert_eq!(root_hash, pascal_project::content_hash_bytes(&bytes));
        let mut stamped: Vec<_> = include_stamps
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        stamped.sort();
        assert_eq!(
            stamped,
            vec![
                indexed.root.join("Nested.inc"),
                indexed.root.join("Types.inc")
            ]
        );
        assert!(
            restarted.index.parsed_document(&uri).is_none(),
            "nothing was parsed"
        );
    }

    /// D31: `Main.pas` is analysed and persisted under the project context
    /// `App.dproj`; a restarted workspace without that project discovers
    /// another owner for it. `reuse_in_context` reuses it under the project
    /// key it is given, and `reuse_persisted_facts` (the discovered owner)
    /// cannot.
    #[test]
    fn reuse_in_context_uses_the_given_context_not_the_discovered_owner() {
        use crate::project_cache::IndexAnchor;
        use crate::workspace::WorkspaceOptions;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for (name, bytes) in [
            ("lib/Main.pas", MAIN.as_bytes()),
            ("lib/Types.inc", TYPES.as_bytes()),
            ("lib/Nested.inc", NESTED.as_bytes()),
            (
                "app/App.dproj",
                b"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>"
                    as &[u8],
            ),
            ("app/App.dpr", b"program App; begin end.\n"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        pascal_project::backdate_tree_for_test(&root);
        let uri = Url::from_file_path(root.join("lib/Main.pas")).unwrap();
        let session = || OverrideSession::new(None);
        let configured = Workspace::with_override_session(
            vec![root.clone()],
            WorkspaceOptions {
                project_file: Some("app/App.dproj".into()),
                ..WorkspaceOptions::default()
            },
            session(),
        );
        let mut analysed = Workspace::from_analysis_input(&configured.analysis_input());
        analysed
            .warm_with_cancel(&uri, &AtomicBool::new(false))
            .unwrap();
        let project_key = analysed.document_contexts[&uri].clone();
        let directory = private_dir();
        let store = open_store(directory.path());
        analysed
            .persist_source(
                &uri,
                &store,
                &mut FactBudget::new(1 << 26),
                &AtomicBool::new(false),
            )
            .unwrap();

        let plain = Workspace::with_override_session(
            vec![root.clone()],
            WorkspaceOptions::default(),
            session(),
        );
        let mut restarted = Workspace::from_analysis_input(&plain.analysis_input());
        let installed = restarted
            .index_context_for(
                &IndexAnchor::Project(root.join("app/App.dproj")),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(installed, project_key, "the same project context");
        let cancel = Arc::new(AtomicBool::new(false));
        let work = ReconciliationBudget::new(Arc::clone(&cancel));
        let reuse = restarted
            .reuse_in_context(&uri, &project_key, &store, &work, DECODE_LIMIT, &cancel)
            .unwrap();
        assert!(
            matches!(
                reuse,
                Reuse::Hit {
                    via: ReuseVia::Evidence,
                    ..
                }
            ),
            "{reuse:?}"
        );
        assert!(restarted.index.parsed_document(&uri).is_none());
        let discovered = restarted.context_for_uri_with_cancel(&uri, None).unwrap();
        assert_ne!(discovered, project_key, "another owner is discovered");
        let owner_reuse = restarted
            .reuse_persisted_facts(&uri, &store, &work, DECODE_LIMIT, &cancel)
            .unwrap();
        assert!(matches!(owner_reuse, Reuse::Miss(_)), "{owner_reuse:?}");
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

    type Fixture = (&'static [(&'static str, &'static [u8])], &'static str);

    /// A source that persists under its local key, and one that only
    /// persists under evidence (a provider-dependent source).
    const FIXTURES: [Fixture; 2] = [
        (&[("Cond.pas", CONDITIONAL)], "Cond.pas"),
        (
            &[("Main.pas", PROVIDER_MAIN), ("Shapes.pas", SHAPES)],
            "Main.pas",
        ),
    ];

    /// What decoding `name`'s stored artifact uses, loaded straight from the
    /// store under its local key, else through its evidence.
    fn decode_use(indexed: &Indexed, name: &str, store: &Store) -> usize {
        let source = indexed.facts(name);
        let bytes = std::fs::read(indexed.root.join(name)).unwrap();
        let local = LocalArtifactKey::new(
            &SourceContent::from_bytes(&bytes),
            &source.extracted.conditional_context,
            source.origin,
        );
        let mut budget = FactBudget::new(DECODE_LIMIT);
        if let Lookup::Hit(_) = store
            .load_facts(ArtifactKeyRef::local(&local), &mut budget)
            .unwrap()
        {
            return budget.used();
        }
        let context = &indexed.workspace.contexts
            [&indexed.workspace.document_contexts[&indexed.uri(name)]]
            .context;
        let Lookup::Hit(headers) = store
            .find_evidence(&CacheContextKey::new(context), &local)
            .unwrap()
        else {
            panic!("{name}: neither a local artifact nor evidence")
        };
        let artifact = headers[0].artifact_id().unwrap();
        let Lookup::Hit(_) = store.load_facts_by_id(artifact, &mut budget).unwrap() else {
            panic!("{name}: the evidence names a stored artifact")
        };
        budget.used()
    }

    #[test]
    fn a_hit_is_charged_to_the_cache_until_its_facts_drop() {
        for (files, name) in FIXTURES {
            let indexed = index(files, &[name]);
            let directory = private_dir();
            let store = open_store(directory.path());
            persist(&indexed, name, &store);
            let mut restarted = restart(&indexed);
            let cache = restarted.project_cache.clone();
            let Reuse::Hit { facts, .. } =
                reuse(&mut restarted, &indexed.uri(name), &store).unwrap()
            else {
                panic!("{name}: the persisted facts are reused")
            };
            let charged = facts.charge.bytes();
            assert!(charged > 0, "{name}");
            assert_eq!(
                charged,
                decode_use(&indexed, name, &store),
                "{name}: shrunk to what decoding used"
            );
            assert_eq!(cache.stats().reserved_facts_bytes, charged, "{name}");
            cache.set_max_bytes(1);
            assert_eq!(
                cache.stats().reserved_facts_bytes,
                charged,
                "{name}: eviction never releases facts a caller keeps"
            );
            drop(facts);
            assert_eq!(cache.stats().reserved_bytes(), 0, "{name}");
        }
    }

    #[test]
    fn a_refused_reservation_is_a_retryable_memory_miss() {
        let indexed = index(&[("Cond.pas", CONDITIONAL)], &["Cond.pas"]);
        let directory = private_dir();
        let store = open_store(directory.path());
        persist(&indexed, "Cond.pas", &store);
        let mut restarted = restart(&indexed);
        restarted.project_cache.set_max_bytes(DECODE_LIMIT - 1);
        assert_eq!(
            reuse(&mut restarted, &indexed.uri("Cond.pas"), &store).unwrap(),
            Reuse::Miss(ReuseMiss::Unproven(Unproven::Memory))
        );
        restarted
            .project_cache
            .set_max_bytes(crate::project_cache::DEFAULT_MAX_CACHE_BYTES);
        assert!(matches!(
            reuse(&mut restarted, &indexed.uri("Cond.pas"), &store).unwrap(),
            Reuse::Hit { .. }
        ));
    }

    #[test]
    fn facts_over_the_decode_limit_are_a_memory_miss() {
        for (files, name) in FIXTURES {
            let indexed = index(files, &[name]);
            let directory = private_dir();
            let store = open_store(directory.path());
            persist(&indexed, name, &store);
            let mut restarted = restart(&indexed);
            let cancel = Arc::new(AtomicBool::new(false));
            let work = ReconciliationBudget::new(Arc::clone(&cancel));
            assert_eq!(
                restarted.reuse_persisted_facts(&indexed.uri(name), &store, &work, 16, &cancel),
                Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Memory))),
                "{name}"
            );
            assert_eq!(
                restarted.project_cache.stats().reserved_bytes(),
                0,
                "{name}"
            );
        }
    }

    #[test]
    fn a_real_discovery_failure_is_still_an_error() {
        let indexed = index(&[("Cond.pas", CONDITIONAL)], &["Cond.pas"]);
        let directory = private_dir();
        let store = open_store(directory.path());
        std::fs::write(indexed.root.join("Notes.txt"), b"notes").unwrap();
        let mut restarted = restart(&indexed);
        let result = reuse(&mut restarted, &indexed.uri("Notes.txt"), &store);
        assert!(
            matches!(&result, Err(PersistError::Context(message)) if message.contains("unsupported Pascal document path")),
            "{result:?}"
        );
    }
}

/// `l4d-zf1b.5` acceptance criteria, each against a real store and a
/// restarted workspace.
#[cfg(all(test, unix))]
pub(in crate::workspace) mod acceptance {
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
    pub(in crate::workspace) fn rewrite_keeping_the_stamp(path: &Path, bytes: &[u8]) {
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
            DECODE_LIMIT,
            &cancel,
        );
        assert_eq!(
            result,
            Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Cancelled)))
        );
    }

    /// l4d-ne50: cancellation during context discovery is a retryable miss,
    /// never a hard failure a caller would report.
    #[test]
    fn cancellation_during_context_discovery_is_a_cancelled_miss() {
        let (indexed, directory) = persisted(&providers());
        let store = open_store(directory.path());
        let mut restarted = restart(&indexed);
        let cancel = Arc::new(AtomicBool::new(true));
        let work = crate::workspace::ReconciliationBudget::new(Arc::clone(&cancel));
        let result = restarted.reuse_persisted_facts(
            &indexed.uri("Main.pas"),
            &store,
            &work,
            DECODE_LIMIT,
            &cancel,
        );
        assert_eq!(
            result,
            Ok(Reuse::Miss(ReuseMiss::Unproven(Unproven::Cancelled)))
        );
    }
}
