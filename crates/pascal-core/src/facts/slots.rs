//! The include occurrences of a compile-time session in expansion order,
//! and whether its results may be persisted.

use crate::compile_time::{CompileTimeOutput, IncompleteReason, OccurrenceId};
use crate::conditional::Truth;
use crate::{LoadedSource, Resolution, ResolutionCandidate, SourceRevision};
use std::ops::Range;
use std::path::PathBuf;

/// Why an include occurrence has no expansion. Typed and free of resolver,
/// OS or locale text, so identical inputs key identically everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvedInclude {
    Unavailable,
    Ambiguous {
        candidates: Vec<PathBuf>,
    },
    Incomplete {
        candidates: Vec<PathBuf>,
    },
    /// Found, but the session did not expand it.
    NotExpanded,
}

#[derive(Debug, Clone)]
pub enum IncludeSlotOutcome<'a> {
    Expanded {
        occurrence: OccurrenceId,
        source: &'a LoadedSource,
    },
    Unresolved(UnresolvedInclude),
}

#[derive(Debug, Clone)]
pub struct IncludeSlot<'a> {
    /// The slot holding the directive; 0 is the root.
    pub parent: u32,
    /// The directive in its parent's decoded text.
    pub directive: Range<usize>,
    /// The directive as written.
    pub written: &'a str,
    pub outcome: IncludeSlotOutcome<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    MissingOccurrence,
    NoDecodedText,
    DirectiveOutOfRange,
    TooManySlots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPersistable {
    /// Cancelled or limited: retry, never store as a stable result.
    Transient,
    /// The root came from an unsaved editor buffer (spec §4.4).
    OverlayRoot,
    /// An include came from an unsaved editor buffer (spec §4.4).
    OverlayInclude { slot: u32 },
}

/// The text a session's coordinates refer to, as the LSP source map reads
/// it (`workspace::compile_time::decoded_source`).
pub fn decoded_text(source: &LoadedSource) -> Option<&str> {
    source
        .decoded_text
        .as_deref()
        .or_else(|| std::str::from_utf8(&source.bytes).ok())
}

/// The session's include occurrences in expansion order. This is the order in
/// which the LSP source map appends them (`workspace::compile_time::mapped`):
/// an owner's includes in session order, each expanded child's own
/// occurrences before the owner's next include. An inactive directive that
/// was not expanded is no occurrence.
pub fn include_slots(output: &CompileTimeOutput) -> Result<Vec<IncludeSlot<'_>>, SlotError> {
    let root = output
        .occurrences
        .first()
        .ok_or(SlotError::MissingOccurrence)?;
    let mut slots = Vec::new();
    visit(output, root.id, 0, &mut slots)?;
    Ok(slots)
}

fn visit<'a>(
    output: &'a CompileTimeOutput,
    owner: OccurrenceId,
    owner_slot: u32,
    slots: &mut Vec<IncludeSlot<'a>>,
) -> Result<(), SlotError> {
    let occurrence = output
        .occurrences
        .iter()
        .find(|occurrence| occurrence.id == owner)
        .ok_or(SlotError::MissingOccurrence)?;
    let text = decoded_text(&occurrence.source).ok_or(SlotError::NoDecodedText)?;
    for include in output
        .includes
        .iter()
        .filter(|include| include.owner == owner)
    {
        let range = include.directive.range.clone();
        let written = text
            .get(range.clone())
            .ok_or(SlotError::DirectiveOutOfRange)?;
        let inactive = occurrence.analysis.directives.iter().any(|directive| {
            directive.start == range.start
                && directive.end == range.end
                && directive.activity == Truth::False
        });
        let outcome = match include.child {
            Some(child) => IncludeSlotOutcome::Expanded {
                occurrence: child,
                source: &output
                    .occurrences
                    .iter()
                    .find(|occurrence| occurrence.id == child)
                    .ok_or(SlotError::MissingOccurrence)?
                    .source,
            },
            None if inactive => continue,
            None => IncludeSlotOutcome::Unresolved(unresolved(&include.target)),
        };
        let slot = u32::try_from(slots.len() + 1).map_err(|_| SlotError::TooManySlots)?;
        let child = match &outcome {
            IncludeSlotOutcome::Expanded { occurrence, .. } => Some(*occurrence),
            IncludeSlotOutcome::Unresolved(_) => None,
        };
        slots.push(IncludeSlot {
            parent: owner_slot,
            directive: range,
            written,
            outcome,
        });
        if let Some(child) = child {
            visit(output, child, slot, slots)?;
        }
    }
    Ok(())
}

fn unresolved(target: &Resolution<LoadedSource>) -> UnresolvedInclude {
    let paths = |candidates: &[ResolutionCandidate]| -> Vec<PathBuf> {
        candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect()
    };
    match target {
        Resolution::Found(_) => UnresolvedInclude::NotExpanded,
        Resolution::Unavailable { .. } => UnresolvedInclude::Unavailable,
        Resolution::Ambiguous { candidates } => UnresolvedInclude::Ambiguous {
            candidates: paths(candidates),
        },
        Resolution::Incomplete { candidates, .. } => UnresolvedInclude::Incomplete {
            candidates: paths(candidates),
        },
    }
}

/// Whether facts derived from this session may be published.
///
/// Provider queries (`CompileTimeOutput::provider_queries`) do not refuse
/// persistence; they make the key dependent (`pascal_cache::SessionDependence`).
pub fn session_persistability(
    output: &CompileTimeOutput,
    slots: &[IncludeSlot<'_>],
) -> Result<(), NotPersistable> {
    if output.reasons.iter().any(|reason| {
        matches!(
            reason,
            IncompleteReason::Cancelled | IncompleteReason::Limit { .. }
        )
    }) || slots.iter().any(|slot| {
        matches!(
            slot.outcome,
            IncludeSlotOutcome::Unresolved(UnresolvedInclude::Incomplete { .. })
        )
    }) {
        return Err(NotPersistable::Transient);
    }
    if output
        .occurrences
        .first()
        .is_some_and(|root| matches!(root.source.revision, SourceRevision::Overlay { .. }))
    {
        return Err(NotPersistable::OverlayRoot);
    }
    for (index, slot) in slots.iter().enumerate() {
        if let IncludeSlotOutcome::Expanded { source, .. } = &slot.outcome
            && matches!(source.revision, SourceRevision::Overlay { .. })
        {
            return Err(NotPersistable::OverlayInclude {
                slot: index as u32 + 1,
            });
        }
    }
    Ok(())
}
