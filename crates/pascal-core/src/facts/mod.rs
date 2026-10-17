//! Compact facts of one analysed source: the persistent analysis cache's
//! DTO contracts (shared persistent analysis cache design §5).
//!
//! Facts own no syntax tree and no path. A fact is located by a source slot
//! and a byte range in that slot's decoded UTF-8 text: slot 0 is the root,
//! slot `n` the `n`th include occurrence in expansion order. Postings are
//! lexical candidates, never proven references.

mod lexical;

pub use lexical::{IdentifierRuns, identifier_runs, is_identifier_byte};

use crate::ImportSection;
use std::fmt;
use std::ops::Range;

/// A byte range in 32-bit fact coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    pub fn new(range: Range<usize>) -> Result<Self, FactError> {
        let start = u32::try_from(range.start).map_err(|_| FactError::RangeTooLarge)?;
        let end = u32::try_from(range.end).map_err(|_| FactError::RangeTooLarge)?;
        if start > end {
            return Err(FactError::InvertedRange);
        }
        Ok(Self { start, end })
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn to_range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactLocation {
    /// In slot `slot`'s decoded text.
    Source { slot: u32, range: TextRange },
    /// In analysed (expanded) coordinates only: the range crosses sources or
    /// lies in synthetic text. Never an edit span.
    Expanded { range: TextRange },
}

/// Conditional activity of the code holding a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    Active,
    Inactive,
    Unknown,
}

/// What the analysis established about a fact. Only an active fact outside
/// parser recovery is proven, and nothing upgrades the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Certainty {
    pub activity: Activity,
    pub recovered: bool,
}

impl Certainty {
    pub fn is_proven(self) -> bool {
        self.activity == Activity::Active && !self.recovered
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclarationKind {
    Unit,
    Type,
    Routine,
    Variable,
    Constant,
    Field,
    Property,
    EnumValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    Public,
    Published,
    Private,
    Protected,
    StrictPrivate,
    StrictProtected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    Interface,
    Implementation,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    Declaration,
    Definition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationFact {
    /// Position among the artifact's declarations.
    pub id: u32,
    /// Normalized with the analyzer's name rules; the index key.
    pub name: String,
    pub display: String,
    pub kind: DeclarationKind,
    pub visibility: Visibility,
    pub region: Region,
    pub origin: Origin,
    /// Visible to importers.
    pub exported: bool,
    /// The declaration of the owning type, when the analysis found it.
    pub parent: Option<u32>,
    /// The declared name.
    pub location: FactLocation,
    pub certainty: Certainty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportFact {
    /// Position among the artifact's imports.
    pub id: u32,
    /// Normalized dotted unit name; the index key.
    pub name: String,
    /// The spelling as written.
    pub display: String,
    pub section: ImportSection,
    pub location: FactLocation,
    pub certainty: Certainty,
}

/// The conditional context of one lexical identifier run. The run may be in
/// a comment or a string: postings are candidates, not references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PostingActivity {
    Active,
    Inactive,
    Unknown,
    /// In a compiler directive, including a masked include directive.
    Directive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PostingOccurrence {
    pub slot: u32,
    pub range: TextRange,
    pub activity: PostingActivity,
}

/// Every lexical run of one normalized identifier: at least one, ordered by
/// slot and then offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingFact {
    /// The run's ASCII-lowercased spelling; the index key.
    pub name: String,
    pub occurrences: Vec<PostingOccurrence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IncludeExpansion {
    Expanded,
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IncludeFact {
    /// This occurrence's slot, at least 1.
    pub slot: u32,
    /// The slot holding the directive.
    pub parent: u32,
    /// The directive in its parent's decoded text.
    pub directive: TextRange,
    pub expansion: IncludeExpansion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentTarget {
    Source { slot: u32, start: u32 },
    Synthetic,
}

/// One run of analysed text and where it came from, in analysed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentFact {
    pub expanded: TextRange,
    pub target: SegmentTarget,
}

/// Why an extraction is incomplete: a stable property of the exact inputs,
/// never a claim of current verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoverageReason {
    ConditionalUnknown,
    ParserRecovery,
    UnresolvedInclude,
    ExpansionIncomplete,
    UnmappedLocation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactFacts {
    /// Decoded length of each slot's text; slot 0 is the root.
    pub slot_lengths: Vec<u32>,
    /// Length of the analysed text, which is the expanded text when includes
    /// were expanded.
    pub expanded_length: u32,
    pub declarations: Vec<DeclarationFact>,
    pub imports: Vec<ImportFact>,
    /// Sorted by name bytes, one entry per name.
    pub postings: Vec<PostingFact>,
    /// In slot order: `includes[i].slot == i + 1`.
    pub includes: Vec<IncludeFact>,
    /// The source map; empty without includes.
    pub segments: Vec<SegmentFact>,
    /// Sorted, without duplicates; empty when the extraction is complete.
    pub coverage: Vec<CoverageReason>,
}

/// Retained-byte admission for facts, charged before each fact is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactBudget {
    limit: usize,
    used: usize,
}

impl FactBudget {
    pub fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Reserve `bytes`, or refuse without reserving anything.
    pub fn charge(&mut self, bytes: usize) -> Result<(), FactError> {
        match self.used.checked_add(bytes) {
            Some(used) if used <= self.limit => {
                self.used = used;
                Ok(())
            }
            _ => Err(FactError::Budget { limit: self.limit }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactError {
    Budget {
        limit: usize,
    },
    RangeTooLarge,
    InvertedRange,
    Cancelled,
    /// The source is not analysed.
    NotIndexed,
    /// Includes were expanded but their source map is not available.
    MissingSourceMap,
    /// The analysis's own coordinates or include topology disagree.
    Inconsistent,
}

impl fmt::Display for FactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget { limit } => write!(formatter, "facts exceed the {limit}-byte admission"),
            Self::RangeTooLarge => formatter.write_str("a fact range exceeds 32-bit coordinates"),
            Self::InvertedRange => formatter.write_str("a fact range is inverted"),
            Self::Cancelled => formatter.write_str("request cancelled"),
            Self::NotIndexed => formatter.write_str("the source is not indexed"),
            Self::MissingSourceMap => formatter.write_str("expanded includes have no source map"),
            Self::Inconsistent => formatter.write_str("the analysis is internally inconsistent"),
        }
    }
}

impl std::error::Error for FactError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_refuse_inversion_and_overflow() {
        assert_eq!(TextRange::new(3..7), Ok(TextRange { start: 3, end: 7 }));
        assert_eq!(TextRange::new(3..7).unwrap().len(), 4);
        #[allow(clippy::reversed_empty_ranges)]
        let inverted = TextRange::new(7..3);
        assert_eq!(inverted, Err(FactError::InvertedRange));
        assert_eq!(
            TextRange::new(0..u32::MAX as usize + 1),
            Err(FactError::RangeTooLarge)
        );
    }

    #[test]
    fn only_active_unrecovered_facts_are_proven() {
        let proven = Certainty {
            activity: Activity::Active,
            recovered: false,
        };
        assert!(proven.is_proven());
        for weaker in [
            Certainty {
                activity: Activity::Unknown,
                ..proven
            },
            Certainty {
                activity: Activity::Inactive,
                ..proven
            },
            Certainty {
                recovered: true,
                ..proven
            },
        ] {
            assert!(!weaker.is_proven(), "{weaker:?}");
        }
    }

    #[test]
    fn the_budget_refuses_without_reserving() {
        let mut budget = FactBudget::new(10);
        assert_eq!(budget.charge(6), Ok(()));
        assert_eq!(budget.charge(5), Err(FactError::Budget { limit: 10 }));
        assert_eq!(budget.used(), 6);
        assert_eq!(
            budget.charge(usize::MAX),
            Err(FactError::Budget { limit: 10 })
        );
        assert_eq!(budget.charge(4), Ok(()));
        assert_eq!(budget.used(), 10);
    }
}
