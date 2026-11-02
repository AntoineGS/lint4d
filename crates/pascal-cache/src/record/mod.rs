//! Member records (spec §5, §6.1 and §6.3, amendments 2026-10-27): one
//! member of a context and its resolved imports and includes, identified by
//! the digest of its canonical manifest, so identical members converge on
//! one record across snapshots and processes.

mod decode;

pub use decode::{
    RecordDecodeError, decode_binding, decode_member_header, decode_snapshot_coverage,
};

use crate::manifest::manifest_encoder;
use crate::path::path_bound;
use crate::{CanonicalEncode, Digest, Encoder, KeyKind, MANIFEST_DIGEST_CONTEXT, SourceIdentity};
use pascal_project::SourceOrigin;

/// Candidates an ambiguous binding keeps; `overflow` marks that more existed.
pub const MAX_AMBIGUOUS_CANDIDATES: usize = 16;

/// Why a result is incomplete under its exact inputs. Cancellation,
/// transient I/O and exhausted budgets have no code: they are retried,
/// never stored (spec §5, §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IncompleteReason {
    /// Syntax or a directive the analyzer does not support.
    Unsupported,
    /// A condition that evaluated to unknown decided the result.
    UnknownConditional,
    /// A compiled provider; ordinary analysis re-establishes it (spec §6.1).
    CompiledProvider,
    /// An input over a source size limit.
    OverSizeLimit,
    /// The read policy refused an input.
    Unauthorized,
}

impl IncompleteReason {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Unsupported => 1,
            Self::UnknownConditional => 2,
            Self::CompiledProvider => 3,
            Self::OverSizeLimit => 4,
            Self::Unauthorized => 5,
        }
    }

    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Unsupported),
            2 => Some(Self::UnknownConditional),
            3 => Some(Self::CompiledProvider),
            4 => Some(Self::OverSizeLimit),
            5 => Some(Self::Unauthorized),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverageStatus {
    Complete,
    /// Incomplete for at least this reason; others may exist.
    Incomplete(IncompleteReason),
}

/// A member's historical coverage; never a current-verification flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemberCoverage {
    pub extraction: CoverageStatus,
    pub resolution: CoverageStatus,
}

/// A snapshot's historical coverage (spec §6.3: `sealed` is not complete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotCoverage {
    pub discovery: CoverageStatus,
    pub membership: CoverageStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingKind {
    Import,
    Include,
}

impl BindingKind {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Import => 1,
            Self::Include => 2,
        }
    }

    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Import),
            2 => Some(Self::Include),
            _ => None,
        }
    }
}

/// What the resolver concluded. A target is a source identity, resolved to
/// a member within a snapshot by its canonical path (spec §5 amendment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingOutcome {
    Precise {
        target: SourceIdentity,
    },
    Ambiguous {
        candidates: Vec<SourceIdentity>,
        /// More candidates existed than `MAX_AMBIGUOUS_CANDIDATES`.
        overflow: bool,
    },
    Unavailable,
    Incomplete(IncompleteReason),
}

impl BindingOutcome {
    pub fn tag(&self) -> u8 {
        match self {
            Self::Precise { .. } => 1,
            Self::Ambiguous { .. } => 2,
            Self::Unavailable => 3,
            Self::Incomplete(_) => 4,
        }
    }

    pub fn target(&self) -> Option<&SourceIdentity> {
        match self {
            Self::Precise { target } => Some(target),
            _ => None,
        }
    }
}

/// One import or include occurrence of a member and its outcome. The
/// evidence proves the lookup (binding evidence, purpose 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub kind: BindingKind,
    /// The occurrence ID in the member's artifact.
    pub occurrence: u64,
    pub outcome: BindingOutcome,
    /// Required unless the outcome is `Incomplete`, which may still carry
    /// the evidence of the lookup that ended incomplete.
    pub evidence: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHeader {
    pub source: SourceIdentity,
    /// `None` for a recognized but unindexed member.
    pub artifact: Option<Digest>,
    /// The artifact evidence proving an expanded artifact in this context;
    /// absent for local artifacts and unindexed members.
    pub member_evidence: Option<Digest>,
    pub origin: SourceOrigin,
    pub fallback_unit_name: Option<String>,
    pub coverage: MemberCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRecord {
    pub header: MemberHeader,
    /// Strictly ordered by `(kind, occurrence)`.
    pub bindings: Vec<Binding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberError {
    EvidenceWithoutArtifact,
    BindingOrder {
        ordinal: usize,
    },
    MissingEvidence {
        ordinal: usize,
    },
    Candidates {
        ordinal: usize,
    },
    /// A referenced evidence set has another purpose (or concludes another
    /// artifact) than the reference needs.
    EvidencePurpose,
}

impl CanonicalEncode for IncompleteReason {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(self.tag());
    }
}

impl CanonicalEncode for CoverageStatus {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Complete => encoder.variant(1),
            Self::Incomplete(reason) => {
                encoder.variant(2);
                encoder.encode(reason);
            }
        }
    }
}

impl CanonicalEncode for MemberCoverage {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.field(1);
        encoder.encode(&self.extraction);
        encoder.field(2);
        encoder.encode(&self.resolution);
    }
}

impl CanonicalEncode for SnapshotCoverage {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.field(1);
        encoder.encode(&self.discovery);
        encoder.field(2);
        encoder.encode(&self.membership);
    }
}

impl CanonicalEncode for BindingOutcome {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(self.tag());
        match self {
            Self::Precise { target } => encoder.encode(target),
            Self::Ambiguous {
                candidates,
                overflow,
            } => {
                encoder.seq(candidates.iter(), |encoder, candidate| {
                    encoder.encode(candidate)
                });
                encoder.bool(*overflow);
            }
            Self::Unavailable => {}
            Self::Incomplete(reason) => encoder.encode(reason),
        }
    }
}

impl CanonicalEncode for Binding {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.field(1);
        encoder.variant(self.kind.tag());
        encoder.field(2);
        encoder.u64(self.occurrence);
        encoder.field(3);
        encoder.encode(&self.outcome);
        encoder.field(4);
        encoder.option(self.evidence.as_ref(), |encoder, evidence| {
            encoder.digest(evidence)
        });
    }
}

impl CanonicalEncode for MemberHeader {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.field(1);
        encoder.encode(&self.source);
        encoder.field(2);
        encoder.option(self.artifact.as_ref(), |encoder, artifact| {
            encoder.digest(artifact)
        });
        encoder.field(3);
        encoder.option(self.member_evidence.as_ref(), |encoder, evidence| {
            encoder.digest(evidence)
        });
        encoder.field(4);
        encoder.encode(&self.origin);
        encoder.field(5);
        encoder.option(self.fallback_unit_name.as_deref(), |encoder, name| {
            encoder.str(name)
        });
        encoder.field(6);
        encoder.encode(&self.coverage);
    }
}

fn encoded(value: &impl CanonicalEncode) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.encode(value);
    encoder.into_bytes()
}

impl SnapshotCoverage {
    pub fn encoded(&self) -> Vec<u8> {
        encoded(self)
    }
}

impl Binding {
    /// The stored payload, and this binding's part of the record manifest.
    pub fn encoded(&self) -> Vec<u8> {
        encoded(self)
    }

    /// The stored order: by kind tag value (`Import` = 1 before
    /// `Include` = 2), then occurrence. Part of the record ID.
    pub(crate) fn order_key(&self) -> (u8, u64) {
        (self.kind.tag(), self.occurrence)
    }
}

impl MemberHeader {
    pub fn encoded(&self) -> Vec<u8> {
        encoded(self)
    }
}

/// The invariants of one binding, apart from its position in the record.
pub(crate) fn binding_error(binding: &Binding, ordinal: usize) -> Option<MemberError> {
    if let BindingOutcome::Ambiguous {
        candidates,
        overflow,
    } = &binding.outcome
        && (candidates.len() < 2
            || candidates.len() > MAX_AMBIGUOUS_CANDIDATES
            || (*overflow && candidates.len() != MAX_AMBIGUOUS_CANDIDATES))
    {
        return Some(MemberError::Candidates { ordinal });
    }
    let needs_evidence = !matches!(binding.outcome, BindingOutcome::Incomplete(_));
    if needs_evidence && binding.evidence.is_none() {
        return Some(MemberError::MissingEvidence { ordinal });
    }
    None
}

impl MemberRecord {
    pub fn validate(&self) -> Result<(), MemberError> {
        if self.header.member_evidence.is_some() && self.header.artifact.is_none() {
            return Err(MemberError::EvidenceWithoutArtifact);
        }
        let mut previous = None;
        for (ordinal, binding) in self.bindings.iter().enumerate() {
            if let Some(error) = binding_error(binding, ordinal) {
                return Err(error);
            }
            let key = binding.order_key();
            if previous.is_some_and(|previous| previous >= key) {
                return Err(MemberError::BindingOrder { ordinal });
            }
            previous = Some(key);
        }
        Ok(())
    }

    /// The record ID: the digest of its canonical manifest (header, binding
    /// count, then each binding's encoding).
    pub fn id(&self) -> Result<Digest, MemberError> {
        self.validate()?;
        let mut hasher = RecordHasher::new(&self.header.encoded(), self.bindings.len());
        for binding in &self.bindings {
            hasher.update(&binding.encoded());
        }
        Ok(hasher.finish())
    }

    /// An upper bound of what `Store::publish_member_record` encodes and
    /// charges (the header, every binding payload and every precise target
    /// key), from input sizes alone, so the scratch is reserved before any
    /// encoding (spec §10).
    pub(crate) fn encoded_bytes_bound(&self) -> usize {
        // Tags, digests, options, coverage and the ambiguous count: under
        // 128 bytes for a header and for a binding.
        const FIXED: usize = 128;
        let header = FIXED
            .saturating_add(self.header.source.encoded_bytes_bound())
            .saturating_add(
                self.header
                    .fallback_unit_name
                    .as_ref()
                    .map_or(0, |name| 8 + name.len()),
            );
        self.bindings.iter().fold(header, |total, binding| {
            let identities = match &binding.outcome {
                BindingOutcome::Precise { target } => target
                    .encoded_bytes_bound()
                    .saturating_add(path_bound(&target.canonical)),
                BindingOutcome::Ambiguous { candidates, .. } => candidates
                    .iter()
                    .map(SourceIdentity::encoded_bytes_bound)
                    .fold(0, usize::saturating_add),
                BindingOutcome::Unavailable | BindingOutcome::Incomplete(_) => 0,
            };
            total.saturating_add(FIXED).saturating_add(identities)
        })
    }
}

/// The record ID, streamed so the bindings never exist in one buffer.
#[derive(Clone)]
pub(crate) struct RecordHasher(blake3::Hasher);

impl RecordHasher {
    pub(crate) fn new(header: &[u8], count: usize) -> Self {
        let mut encoder = manifest_encoder(KeyKind::MemberRecord);
        encoder.field(1);
        encoder.bytes(header);
        encoder.field(2);
        encoder.length(count);
        let mut hasher = blake3::Hasher::new_derive_key(MANIFEST_DIGEST_CONTEXT);
        hasher.update(encoder.as_bytes());
        Self(hasher)
    }

    /// Add the next binding's encoding (`Binding::encoded`).
    pub(crate) fn update(&mut self, encoded: &[u8]) {
        self.0.update(encoded);
    }

    pub(crate) fn finish(&self) -> Digest {
        Digest::from_bytes(*self.0.finalize().as_bytes())
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;
    use crate::evidence::path_key;

    fn identity(path: &str, spelling: Option<&str>, uri: Option<&str>) -> SourceIdentity {
        SourceIdentity {
            canonical: path.into(),
            logical: format!("{path}.logical").into(),
            spelling: spelling.map(str::to_owned),
            uri: uri.map(str::to_owned),
        }
    }

    /// What `Store::publish_member_record` charges: the encoded header, each
    /// binding payload and each precise target's path key.
    fn encoded_bytes(record: &MemberRecord) -> usize {
        record.header.encoded().len()
            + record
                .bindings
                .iter()
                .map(|binding| {
                    binding.encoded().len()
                        + binding
                            .outcome
                            .target()
                            .map_or(0, |target| path_key(&target.canonical).len())
                })
                .sum::<usize>()
    }

    #[test]
    fn the_record_bound_never_undercounts() {
        let evidence = Some(Digest::from_bytes([3; 32]));
        let header = |source, artifact: Option<Digest>, fallback: Option<&str>| MemberHeader {
            source,
            artifact,
            member_evidence: artifact.map(|_| Digest::from_bytes([4; 32])),
            origin: SourceOrigin::Library,
            fallback_unit_name: fallback.map(str::to_owned),
            coverage: MemberCoverage {
                extraction: CoverageStatus::Incomplete(IncompleteReason::Unsupported),
                resolution: CoverageStatus::Complete,
            },
        };
        let candidates = (0..MAX_AMBIGUOUS_CANDIDATES)
            .map(|index| {
                identity(
                    &format!("/w/C{index}.pas"),
                    Some("C"),
                    Some("file:///w/C.pas"),
                )
            })
            .collect::<Vec<_>>();
        let bindings = vec![
            Binding {
                kind: BindingKind::Import,
                occurrence: 0,
                outcome: BindingOutcome::Precise {
                    target: identity("/w/A.pas", Some("A"), None),
                },
                evidence,
            },
            Binding {
                kind: BindingKind::Import,
                occurrence: 1,
                outcome: BindingOutcome::Ambiguous {
                    candidates,
                    overflow: true,
                },
                evidence,
            },
            Binding {
                kind: BindingKind::Include,
                occurrence: 2,
                outcome: BindingOutcome::Unavailable,
                evidence,
            },
            Binding {
                kind: BindingKind::Include,
                occurrence: 3,
                outcome: BindingOutcome::Incomplete(IncompleteReason::OverSizeLimit),
                evidence: None,
            },
        ];
        for record in [
            MemberRecord {
                header: header(identity("/w/M.pas", None, None), None, None),
                bindings: Vec::new(),
            },
            MemberRecord {
                header: header(
                    identity("/w/M.pas", Some("M"), Some("file:///w/M.pas")),
                    Some(Digest::from_bytes([5; 32])),
                    Some("M"),
                ),
                bindings,
            },
        ] {
            let bound = record.encoded_bytes_bound();
            let encoded = encoded_bytes(&record);
            assert!(bound >= encoded, "{bound} < {encoded}");
        }
    }
}
