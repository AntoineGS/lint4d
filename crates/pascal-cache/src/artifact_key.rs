use crate::manifest::manifest_encoder;
use crate::{
    CanonicalEncode, CanonicalKey, Digest, Encoder, KeyKind, SourceContent, SourceIdentity,
};
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
    /// The search entries consulted are part of the identity: a different
    /// search could find the file.
    Unresolved {
        searched: Vec<ProjectPathEntry>,
        reason: String,
    },
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
            Self::Unresolved { searched, reason } => {
                encoder.variant(2);
                encoder.field(1);
                encoder.seq(searched, |encoder, entry| encoder.encode(entry));
                encoder.field(2);
                encoder.str(reason);
            }
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

/// A root's local key plus its ordered include occurrences and topology.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExpandedArtifactKey {
    key: CanonicalKey,
    root: Digest,
}

impl ExpandedArtifactKey {
    pub fn new(
        root: &LocalArtifactKey,
        occurrences: &[IncludeOccurrence],
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
        Ok(Self {
            key: CanonicalKey::finish(KeyKind::ExpandedArtifact, encoder),
            root: root.digest(),
        })
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
}
