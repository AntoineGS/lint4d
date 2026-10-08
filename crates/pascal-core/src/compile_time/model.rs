use crate::{ResolutionObservation, SourceId};
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact<T> {
    Known(T),
    Unknown(UnknownReason),
    Incomplete(IncompleteReason),
}

impl<T> Fact<T> {
    /// Extract only a proven value; unknown and incomplete facts stay unproven.
    pub fn known(self) -> Option<T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown(_) | Self::Incomplete(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    MissingTarget,
    UnsupportedTarget,
    UnsupportedVersion,
    UnresolvedName,
    AmbiguousBinding,
    UnknownActivity,
    UnsupportedLayout,
    InvalidOperand,
    CyclicType,
    CyclicProvider,
    UnresolvedInclude,
    Overflow,
    HeaderNotAnalyzed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncompleteReason {
    Cancelled,
    Limit { name: &'static str, maximum: usize },
    RequiredSource { source: SourceId, reason: String },
    MalformedSource,
}

/// Inherent storage alignment, not a declaration's packed-field alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageLayout {
    pub size: u64,
    pub alignment: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OccurrenceId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sequence(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BindingSite {
    pub scope: ScopeId,
    pub sequence: Sequence,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceAnchor {
    pub source: SourceId,
    pub occurrence: OccurrenceId,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DefinitionKey {
    pub anchor: SourceAnchor,
    pub scope: ScopeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeIdentity {
    Intrinsic(String),
    Declared(DefinitionKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownLayout {
    pub storage: StorageLayout,
    pub identity: TypeIdentity,
    pub dependencies: Vec<ResolutionObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFragment<'a> {
    pub source: &'a SourceId,
    pub occurrence: OccurrenceId,
    pub physical_start: usize,
    pub text: &'a str,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub work: usize,
    pub byte_work: usize,
    pub retained_bytes: usize,
}

/// Round up using checked arithmetic; zero/non-power-of-two alignment is invalid.
pub fn checked_align_up(offset: u64, alignment: u32) -> Option<u64> {
    let a = u64::from(alignment);
    if a == 0 || !a.is_power_of_two() {
        return None;
    }
    offset.checked_add(a - 1).map(|n| n & !(a - 1))
}
