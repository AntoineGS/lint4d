use crate::manifest::manifest_encoder;
use crate::{
    CanonicalEncode, CanonicalKey, Digest, Encoder, KeyKind, SourceContent, SourceIdentity,
};
use pascal_core::SourceRevision;
use pascal_core::facts::{IncludeSlot, IncludeSlotOutcome, UnresolvedInclude};
use pascal_project::{ConditionalContext, ProjectContext, ProjectPathEntry, SourceOrigin};
use std::fmt;
use std::ops::Range;
use std::path::Path;

/// Facts of one source under one effective conditional environment. Carries
/// no project or path identity, so projects with identical inputs share it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalArtifactKey(CanonicalKey);

impl LocalArtifactKey {
    pub fn new(
        content: &SourceContent,
        conditional: &ConditionalContext,
        origin: SourceOrigin,
    ) -> Self {
        let mut encoder = manifest_encoder(KeyKind::LocalArtifact);
        encoder.field(1);
        encoder.encode(content);
        encoder.field(2);
        encoder.encode(conditional);
        encoder.field(3);
        encoder.encode(&origin);
        Self(CanonicalKey::finish(KeyKind::LocalArtifact, encoder))
    }

    /// Key `path` with the origin and effective environment `context`
    /// assigns to it.
    pub fn for_source(context: &ProjectContext, path: &Path, content: &SourceContent) -> Self {
        Self::new(
            content,
            &context.conditional_context_for(path),
            context.source_origin(path),
        )
    }

    pub fn key(&self) -> &CanonicalKey {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        self.0.digest()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeOutcome {
    Resolved {
        source: SourceIdentity,
        entry: Option<ProjectPathEntry>,
        content: SourceContent,
    },
    /// The typed reason, never resolver text, so identical inputs key
    /// identically everywhere. The search configuration is in the context
    /// key: a search that finds the file yields `Resolved`, and one that
    /// also fails cannot change the expansion.
    Unresolved { reason: UnresolvedInclude },
}

/// One include directive in expansion order. A parent always precedes its
/// children, so `parent` indexes an earlier occurrence; `None` is the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeOccurrence {
    pub parent: Option<usize>,
    /// Byte range of the directive in its owner's decoded text.
    pub directive_range: Range<u64>,
    /// The include argument as written.
    pub directive: String,
    /// Conditional state in force at the directive, inherited by the include.
    pub inherited: ConditionalContext,
    pub outcome: IncludeOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    ParentNotEarlier { occurrence: usize, parent: usize },
    InvertedRange { occurrence: usize },
    OverlayInclude { occurrence: usize },
}

impl fmt::Display for KeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentNotEarlier { occurrence, parent } => write!(
                formatter,
                "include occurrence {occurrence} names parent {parent}, which does not precede it"
            ),
            Self::InvertedRange { occurrence } => {
                write!(
                    formatter,
                    "include occurrence {occurrence} has an inverted directive range"
                )
            }
            Self::OverlayInclude { occurrence } => write!(
                formatter,
                "include occurrence {occurrence} came from an unsaved editor buffer"
            ),
        }
    }
}

impl std::error::Error for KeyError {}

impl CanonicalEncode for IncludeOutcome {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Resolved {
                source,
                entry,
                content,
            } => {
                encoder.variant(1);
                encoder.field(1);
                encoder.encode(source);
                encoder.field(2);
                encoder.option(entry.as_ref(), |encoder, entry| encoder.encode(entry));
                encoder.field(3);
                encoder.encode(content);
            }
            Self::Unresolved { reason } => {
                encoder.variant(2);
                encoder.field(1);
                encoder.encode(reason);
            }
        }
    }
}

impl CanonicalEncode for UnresolvedInclude {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Unavailable => encoder.variant(1),
            Self::Ambiguous { candidates } => {
                encoder.variant(2);
                encoder.field(1);
                encoder.seq(candidates, |encoder, path| encoder.encode(path.as_path()));
            }
            Self::Incomplete { candidates } => {
                encoder.variant(3);
                encoder.field(1);
                encoder.seq(candidates, |encoder, path| encoder.encode(path.as_path()));
            }
            Self::NotExpanded => encoder.variant(4),
        }
    }
}

impl CanonicalEncode for IncludeOccurrence {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            parent,
            directive_range,
            directive,
            inherited,
            outcome,
        } = self;
        encoder.field(1);
        encoder.option(parent.as_ref(), |encoder, parent| {
            encoder.u64(*parent as u64)
        });
        encoder.field(2);
        encoder.u64(directive_range.start);
        encoder.field(3);
        encoder.u64(directive_range.end);
        encoder.field(4);
        encoder.str(directive);
        encoder.field(5);
        encoder.encode(inherited);
        encoder.field(6);
        encoder.encode(outcome);
    }
}

/// What makes an expansion's facts depend on more than its root and
/// includes: compile-time evaluation that consulted unit providers. Those
/// facts hold only under the context that resolved the providers and for
/// the exact observations the session made, so both join the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionDependence {
    /// The `ContextKey` digest.
    pub context: Digest,
    /// `observations_digest` of the session's evidence.
    pub observations: Digest,
}

/// A root's local key plus its ordered include occurrences and topology.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExpandedArtifactKey {
    key: CanonicalKey,
    root: Digest,
    dependence: Option<SessionDependence>,
}

impl ExpandedArtifactKey {
    pub fn new(
        root: &LocalArtifactKey,
        occurrences: &[IncludeOccurrence],
    ) -> Result<Self, KeyError> {
        Self::build(root, occurrences, None)
    }

    /// Key an expansion whose facts also depend on `dependence`.
    pub fn dependent(
        root: &LocalArtifactKey,
        occurrences: &[IncludeOccurrence],
        dependence: SessionDependence,
    ) -> Result<Self, KeyError> {
        Self::build(root, occurrences, Some(dependence))
    }

    fn build(
        root: &LocalArtifactKey,
        occurrences: &[IncludeOccurrence],
        dependence: Option<SessionDependence>,
    ) -> Result<Self, KeyError> {
        for (index, occurrence) in occurrences.iter().enumerate() {
            if let Some(parent) = occurrence.parent
                && parent >= index
            {
                return Err(KeyError::ParentNotEarlier {
                    occurrence: index,
                    parent,
                });
            }
            if occurrence.directive_range.start > occurrence.directive_range.end {
                return Err(KeyError::InvertedRange { occurrence: index });
            }
        }
        let mut encoder = manifest_encoder(KeyKind::ExpandedArtifact);
        encoder.field(1);
        encoder.digest(&root.digest());
        encoder.field(2);
        encoder.seq(occurrences, |encoder, occurrence| {
            encoder.encode(occurrence)
        });
        // Present only for a dependent key, so every other key keeps its
        // phase-4 encoding.
        if let Some(dependence) = &dependence {
            encoder.field(3);
            encoder.digest(&dependence.context);
            encoder.digest(&dependence.observations);
        }
        Ok(Self {
            key: CanonicalKey::finish(KeyKind::ExpandedArtifact, encoder),
            root: root.digest(),
            dependence,
        })
    }

    /// Key the expansion a compile-time session performed. Every occurrence
    /// inherits `context`, the root's effective environment: the state at a
    /// directive is a function of the root, that environment and the earlier
    /// occurrences, all of which the key holds. The session observed one path
    /// per included source; it is both its canonical and logical identity.
    pub fn from_slots(
        root: &LocalArtifactKey,
        context: &ConditionalContext,
        slots: &[IncludeSlot<'_>],
    ) -> Result<Self, KeyError> {
        Self::new(root, &slot_occurrences(context, slots)?)
    }

    /// Key the expansion a compile-time session performed. Every occurrence
    /// inherits `context`, the root's effective environment: the state at a
    /// directive is a function of the root, that environment and the earlier
    /// occurrences, all of which the key holds. The session observed one path
    /// per included source; it is both its canonical and logical identity.
    /// This is the key for a session that consulted providers.
    pub fn dependent_from_slots(
        root: &LocalArtifactKey,
        context: &ConditionalContext,
        slots: &[IncludeSlot<'_>],
        dependence: SessionDependence,
    ) -> Result<Self, KeyError> {
        Self::dependent(root, &slot_occurrences(context, slots)?, dependence)
    }

    pub fn key(&self) -> &CanonicalKey {
        &self.key
    }

    pub fn digest(&self) -> Digest {
        self.key.digest()
    }

    /// The digest of the root local key embedded in the manifest.
    pub fn root_digest(&self) -> Digest {
        self.root
    }

    /// What this key holds beyond its root and includes, if anything.
    pub fn dependence(&self) -> Option<SessionDependence> {
        self.dependence
    }
}

fn slot_occurrences(
    context: &ConditionalContext,
    slots: &[IncludeSlot<'_>],
) -> Result<Vec<IncludeOccurrence>, KeyError> {
    slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let outcome = match &slot.outcome {
                IncludeSlotOutcome::Expanded { source, .. } => {
                    let SourceRevision::Disk { path_entry, .. } = &source.revision else {
                        return Err(KeyError::OverlayInclude { occurrence: index });
                    };
                    IncludeOutcome::Resolved {
                        source: SourceIdentity {
                            canonical: source.path.clone(),
                            logical: source.path.clone(),
                            spelling: None,
                            uri: None,
                        },
                        entry: Some(path_entry.clone()),
                        content: SourceContent::from_bytes(&source.bytes),
                    }
                }
                IncludeSlotOutcome::Unresolved(reason) => IncludeOutcome::Unresolved {
                    reason: reason.clone(),
                },
            };
            Ok(IncludeOccurrence {
                parent: slot.parent.checked_sub(1).map(|parent| parent as usize),
                directive_range: slot.directive.start as u64..slot.directive.end as u64,
                directive: slot.written.to_owned(),
                inherited: context.clone(),
                outcome,
            })
        })
        .collect()
}
