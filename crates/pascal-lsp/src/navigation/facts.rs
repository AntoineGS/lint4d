//! Compact facts of one analysed document (`pascal_core::facts`), projected
//! from the existing parse rather than a second syntax walk. Ranges in a
//! `ParsedDocument` are in analysed coordinates, the include-expanded text
//! when includes were expanded; the include source map turns them into
//! source slots. Indexing calls this from `l4d-zf1b.8` on.
#![cfg_attr(not(test), allow(dead_code))]

use super::{
    Origin as SymbolOrigin, ParsedDocument, ROOT_SCOPE, Region as SymbolRegion, Span, Symbol,
    SymbolKind, Visibility as SymbolVisibility, canonical_path,
};
use crate::include_expansion::{ExpandedSource, SegmentSource};
use pascal_core::compile_time::CompileTimeOutput;
use pascal_core::facts::{
    Activity, ArtifactFacts, Certainty, CoverageReason, DeclarationFact, DeclarationKind,
    FactBudget, FactError, FactLocation, ImportFact, IncludeExpansion, IncludeFact, IncludeSlot,
    IncludeSlotOutcome, NotPersistable, Origin, PostingActivity, PostingFact, PostingOccurrence,
    Region, SegmentFact, SegmentTarget, TextRange, Visibility, decoded_text, identifier_runs,
    include_slots, session_persistability,
};
use pascal_project::ConditionalContext;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::size_of;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub(crate) struct ExtractedFacts {
    pub(crate) facts: ArtifactFacts,
    /// Whether the facts may be published; unpublishable facts remain usable
    /// in memory.
    pub(crate) persistable: Result<(), NotPersistable>,
    /// The environment the parse used, which a local artifact key names.
    pub(crate) conditional_context: ConditionalContext,
}

/// Symbols or lexical runs between cancellation checks.
const CANCEL_STRIDE: usize = 4096;

pub(crate) fn extract_facts(
    parsed: &ParsedDocument,
    expansion: Option<&ExpandedSource>,
    budget: &mut FactBudget,
    cancel: &AtomicBool,
) -> Result<ExtractedFacts, FactError> {
    check(cancel)?;
    let session: Option<&CompileTimeOutput> =
        parsed.compile_time_session().map(|analysis| &**analysis);
    let slots = match session {
        Some(session) => include_slots(session).map_err(|_| FactError::Inconsistent)?,
        None => Vec::new(),
    };
    let map = SlotMap::new(session, &slots, expansion)?;
    let texts = slot_texts(parsed, session, &slots)?;
    let mut facts = ArtifactFacts {
        slot_lengths: texts
            .iter()
            .map(|text| length(text.len()))
            .collect::<Result<_, _>>()?,
        expanded_length: length(parsed.source.len())?,
        ..ArtifactFacts::default()
    };
    declarations(parsed, &map, budget, cancel, &mut facts)?;
    imports(parsed, &map, budget, &mut facts)?;
    postings(parsed, &map, &texts, budget, cancel, &mut facts)?;
    includes(&slots, budget, &mut facts)?;
    segments(&map, budget, &mut facts)?;
    facts.coverage = coverage(parsed, session, &slots, &facts);
    Ok(ExtractedFacts {
        facts,
        persistable: session.map_or(Ok(()), |session| session_persistability(session, &slots)),
        conditional_context: parsed.conditional_context.clone(),
    })
}

fn check(cancel: &AtomicBool) -> Result<(), FactError> {
    if cancel.load(Ordering::Relaxed) {
        Err(FactError::Cancelled)
    } else {
        Ok(())
    }
}

fn length(bytes: usize) -> Result<u32, FactError> {
    u32::try_from(bytes).map_err(|_| FactError::RangeTooLarge)
}

/// Each slot's decoded text: the root's, then every include occurrence's.
fn slot_texts<'a>(
    parsed: &'a ParsedDocument,
    session: Option<&'a CompileTimeOutput>,
    slots: &[IncludeSlot<'a>],
) -> Result<Vec<&'a str>, FactError> {
    let mut texts = Vec::with_capacity(slots.len() + 1);
    texts.push(match session {
        Some(session) => {
            let root = session.occurrences.first().ok_or(FactError::Inconsistent)?;
            decoded_text(&root.source).ok_or(FactError::Inconsistent)?
        }
        None => parsed.source.as_ref(),
    });
    for slot in slots {
        texts.push(match &slot.outcome {
            IncludeSlotOutcome::Expanded { source, .. } => {
                decoded_text(source).ok_or(FactError::Inconsistent)?
            }
            IncludeSlotOutcome::Unresolved(_) => "",
        });
    }
    Ok(texts)
}

struct Segment {
    expanded: Range<usize>,
    /// The slot and source offset of a physical segment.
    target: Option<(u32, usize)>,
}

/// Analysed coordinates against source slots. Without includes the analysed
/// text is the root's own text and the map is the identity.
struct SlotMap {
    segments: Vec<Segment>,
    /// Each slot's physical segments by source start, with their analysed
    /// start.
    by_slot: Vec<Vec<(Range<usize>, usize)>>,
}

impl SlotMap {
    fn new(
        session: Option<&CompileTimeOutput>,
        slots: &[IncludeSlot<'_>],
        expansion: Option<&ExpandedSource>,
    ) -> Result<Self, FactError> {
        let identity = Self {
            segments: Vec::new(),
            by_slot: Vec::new(),
        };
        let Some(session) = session.filter(|_| !slots.is_empty()) else {
            return Ok(identity);
        };
        let expansion = expansion.ok_or(FactError::MissingSourceMap)?;
        let root = session
            .occurrences
            .first()
            .ok_or(FactError::Inconsistent)?
            .id;
        let mut slot_of = HashMap::from([(root, 0u32)]);
        for (index, slot) in slots.iter().enumerate() {
            if let IncludeSlotOutcome::Expanded { occurrence, .. } = &slot.outcome {
                slot_of.insert(*occurrence, length(index + 1)?);
            }
        }
        let mut segments = Vec::with_capacity(expansion.segment_count());
        let mut by_slot = vec![Vec::new(); slots.len() + 1];
        for (expanded, source) in expansion.segments() {
            let target = match source {
                SegmentSource::Physical {
                    occurrence: Some(occurrence),
                    source_start,
                } => {
                    let slot = *slot_of.get(&occurrence).ok_or(FactError::Inconsistent)?;
                    by_slot[slot as usize]
                        .push((source_start..source_start + expanded.len(), expanded.start));
                    Some((slot, source_start))
                }
                SegmentSource::Physical {
                    occurrence: None, ..
                } => return Err(FactError::Inconsistent),
                SegmentSource::Synthetic => None,
            };
            segments.push(Segment { expanded, target });
        }
        for physical in &mut by_slot {
            physical.sort_by_key(|(range, _)| range.start);
        }
        Ok(Self { segments, by_slot })
    }

    /// A fact's location: in one slot when one physical segment holds it.
    fn locate(&self, span: Range<usize>) -> Result<FactLocation, FactError> {
        if self.segments.is_empty() {
            return Ok(FactLocation::Source {
                slot: 0,
                range: TextRange::new(span)?,
            });
        }
        let index = self
            .segments
            .partition_point(|segment| segment.expanded.end <= span.start);
        if let Some(segment) = self.segments.get(index)
            && let Some((slot, source_start)) = segment.target
            && segment.expanded.start <= span.start
            && span.end <= segment.expanded.end
        {
            let start = source_start + (span.start - segment.expanded.start);
            return Ok(FactLocation::Source {
                slot,
                range: TextRange::new(start..start + span.len())?,
            });
        }
        Ok(FactLocation::Expanded {
            range: TextRange::new(span)?,
        })
    }

    /// Where a run of slot `slot`'s text was analysed, if it was.
    fn analysed(&self, slot: usize, run: &Range<usize>) -> Option<Range<usize>> {
        if self.segments.is_empty() {
            return (slot == 0).then(|| run.clone());
        }
        let physical = self.by_slot.get(slot)?;
        let index = physical.partition_point(|(range, _)| range.end <= run.start);
        let (range, start) = physical.get(index)?;
        (range.start <= run.start && run.end <= range.end)
            .then(|| start + (run.start - range.start)..start + (run.end - range.start))
    }
}

/// Unit-scope symbols and members of types: what another unit or a
/// qualified access can name. Routine locals, parameters, labels and generic
/// parameters stay out.
fn declared(symbol: &Symbol) -> Option<DeclarationKind> {
    if symbol.generic_parameter.is_some()
        || (symbol.scope != ROOT_SCOPE && symbol.owner_type.is_none())
    {
        return None;
    }
    // A path-inferred fallback unit name is not a declaration fact (spec §4.1).
    if symbol.kind == SymbolKind::Unit && symbol.key != written_unit_name(&symbol.name) {
        return None;
    }
    Some(match symbol.kind {
        SymbolKind::Unit => DeclarationKind::Unit,
        SymbolKind::Type => DeclarationKind::Type,
        SymbolKind::Routine => DeclarationKind::Routine,
        SymbolKind::Variable => DeclarationKind::Variable,
        SymbolKind::Constant => DeclarationKind::Constant,
        SymbolKind::Field => DeclarationKind::Field,
        SymbolKind::Property => DeclarationKind::Property,
        SymbolKind::EnumValue => DeclarationKind::EnumValue,
        SymbolKind::Parameter | SymbolKind::Label => return None,
    })
}

/// The unit key the analyser builds from a header's written dotted name.
fn written_unit_name(display: &str) -> String {
    let parts: Vec<String> = display
        .split('.')
        .map(|part| part.trim().to_owned())
        .collect();
    canonical_path(&parts)
}

fn visibility(visibility: SymbolVisibility) -> Visibility {
    match visibility {
        SymbolVisibility::Public => Visibility::Public,
        SymbolVisibility::Published => Visibility::Published,
        SymbolVisibility::Private => Visibility::Private,
        SymbolVisibility::Protected => Visibility::Protected,
        SymbolVisibility::StrictPrivate => Visibility::StrictPrivate,
        SymbolVisibility::StrictProtected => Visibility::StrictProtected,
    }
}

fn region(region: SymbolRegion) -> Region {
    match region {
        SymbolRegion::Interface => Region::Interface,
        SymbolRegion::Implementation => Region::Implementation,
        SymbolRegion::Other => Region::Other,
    }
}

fn origin(origin: SymbolOrigin) -> Origin {
    match origin {
        SymbolOrigin::Declaration => Origin::Declaration,
        SymbolOrigin::Definition => Origin::Definition,
    }
}

fn certainty(unknown: bool, recovery: &[Span], span: Span) -> Certainty {
    Certainty {
        activity: if unknown {
            Activity::Unknown
        } else {
            Activity::Active
        },
        // Closed intervals, so a zero-width MISSING node touching the fact
        // counts.
        recovered: recovery
            .iter()
            .any(|recovered| recovered.start <= span.end && span.start <= recovered.end),
    }
}

fn declarations(
    parsed: &ParsedDocument,
    map: &SlotMap,
    budget: &mut FactBudget,
    cancel: &AtomicBool,
    facts: &mut ArtifactFacts,
) -> Result<(), FactError> {
    let exported: HashSet<usize> = parsed.exported_symbol_indices.iter().copied().collect();
    let mut ids = HashMap::new();
    for (index, symbol) in parsed.symbols.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check(cancel)?;
        }
        let Some(kind) = declared(symbol) else {
            continue;
        };
        budget.charge(size_of::<DeclarationFact>() + symbol.key.len() + symbol.name.len())?;
        let id = length(facts.declarations.len())?;
        ids.insert(index, id);
        let unknown = parsed
            .conditional_unknown_symbols
            .get(index)
            .copied()
            .unwrap_or(true);
        facts.declarations.push(DeclarationFact {
            id,
            name: symbol.key.clone(),
            display: symbol.name.clone(),
            kind,
            visibility: visibility(symbol.visibility),
            region: region(symbol.region),
            origin: origin(symbol.origin),
            exported: exported.contains(&index),
            parent: None,
            location: map.locate(symbol.selection_span.start..symbol.selection_span.end)?,
            certainty: certainty(unknown, &parsed.parser_recovery_spans, symbol.span),
        });
    }
    // A parent must be an earlier declaration, so a member declared before
    // its owner gets no parent.
    for (index, symbol) in parsed.symbols.iter().enumerate() {
        let (Some(&id), Some(owner)) = (ids.get(&index), symbol.owner_type.as_ref()) else {
            continue;
        };
        facts.declarations[id as usize].parent = parsed
            .type_symbol_indices
            .get(owner)
            .and_then(|types| types.first())
            .and_then(|owner| ids.get(owner))
            .copied()
            .filter(|parent| *parent < id);
    }
    Ok(())
}

fn imports(
    parsed: &ParsedDocument,
    map: &SlotMap,
    budget: &mut FactBudget,
    facts: &mut ArtifactFacts,
) -> Result<(), FactError> {
    for import in &parsed.imports {
        let span = import.span.start..import.span.end;
        let display = parsed
            .source
            .get(span.clone())
            .ok_or(FactError::Inconsistent)?;
        budget.charge(size_of::<ImportFact>() + import.name.len() + display.len())?;
        let unknown = parsed
            .conditionals
            .unknown_spans
            .iter()
            .any(|unknown| unknown.start <= span.start && span.end <= unknown.end);
        facts.imports.push(ImportFact {
            id: length(facts.imports.len())?,
            name: import.name.clone(),
            display: display.to_owned(),
            section: import.section,
            location: map.locate(span.clone())?,
            certainty: certainty(
                unknown,
                &parsed.parser_recovery_spans,
                Span {
                    start: span.start,
                    end: span.end,
                },
            ),
        });
    }
    Ok(())
}

fn sorted(spans: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let mut spans: Vec<_> = spans.collect();
    spans.sort_by_key(|span| span.start);
    spans
}

/// The conditional context of an analysed run, most conservative first.
fn classify(
    run: &Range<usize>,
    directives: &[Range<usize>],
    inactive: &[Range<usize>],
    unknown: &[Range<usize>],
) -> PostingActivity {
    let holds = |spans: &[Range<usize>]| {
        spans[..spans.partition_point(|span| span.start <= run.start)]
            .iter()
            .any(|span| run.end <= span.end)
    };
    if holds(directives) {
        PostingActivity::Directive
    } else if holds(inactive) {
        PostingActivity::Inactive
    } else if unknown[..unknown.partition_point(|span| span.start < run.end)]
        .iter()
        .any(|span| run.start < span.end)
    {
        PostingActivity::Unknown
    } else {
        PostingActivity::Active
    }
}

fn postings(
    parsed: &ParsedDocument,
    map: &SlotMap,
    texts: &[&str],
    budget: &mut FactBudget,
    cancel: &AtomicBool,
    facts: &mut ArtifactFacts,
) -> Result<(), FactError> {
    let conditionals = &parsed.conditionals;
    let directives = sorted(
        conditionals
            .directives
            .iter()
            .map(|directive| directive.start..directive.end),
    );
    let inactive = sorted(conditionals.inactive_spans.iter().cloned());
    let unknown = sorted(conditionals.unknown_spans.iter().cloned());
    let mut names: BTreeMap<String, Vec<PostingOccurrence>> = BTreeMap::new();
    let mut name = String::new();
    let mut scanned = 0usize;
    for (slot, text) in texts.iter().enumerate() {
        let slot_id = length(slot)?;
        for run in identifier_runs(text.as_bytes()) {
            scanned += 1;
            if scanned.is_multiple_of(CANCEL_STRIDE) {
                check(cancel)?;
            }
            // A digit-led run is never an identifier, so no name query can
            // match it.
            if text.as_bytes()[run.start].is_ascii_digit() {
                continue;
            }
            let activity = match map.analysed(slot, &run) {
                Some(analysed) => classify(&analysed, &directives, &inactive, &unknown),
                // A masked include directive is not analysed text.
                None => PostingActivity::Directive,
            };
            let occurrence = PostingOccurrence {
                slot: slot_id,
                range: TextRange::new(run.clone())?,
                activity,
            };
            name.clear();
            name.push_str(&text[run]);
            name.make_ascii_lowercase();
            if let Some(occurrences) = names.get_mut(name.as_str()) {
                budget.charge(size_of::<PostingOccurrence>())?;
                occurrences.push(occurrence);
            } else {
                budget.charge(
                    size_of::<PostingFact>() + name.len() + size_of::<PostingOccurrence>(),
                )?;
                names.insert(name.clone(), vec![occurrence]);
            }
        }
    }
    facts.postings = names
        .into_iter()
        .map(|(name, occurrences)| PostingFact { name, occurrences })
        .collect();
    Ok(())
}

fn includes(
    slots: &[IncludeSlot<'_>],
    budget: &mut FactBudget,
    facts: &mut ArtifactFacts,
) -> Result<(), FactError> {
    for (index, slot) in slots.iter().enumerate() {
        budget.charge(size_of::<IncludeFact>())?;
        facts.includes.push(IncludeFact {
            slot: length(index + 1)?,
            parent: slot.parent,
            directive: TextRange::new(slot.directive.clone())?,
            expansion: match slot.outcome {
                IncludeSlotOutcome::Expanded { .. } => IncludeExpansion::Expanded,
                IncludeSlotOutcome::Unresolved(_) => IncludeExpansion::Unresolved,
            },
        });
    }
    Ok(())
}

fn segments(
    map: &SlotMap,
    budget: &mut FactBudget,
    facts: &mut ArtifactFacts,
) -> Result<(), FactError> {
    for segment in &map.segments {
        budget.charge(size_of::<SegmentFact>())?;
        facts.segments.push(SegmentFact {
            expanded: TextRange::new(segment.expanded.clone())?,
            target: match segment.target {
                Some((slot, start)) => SegmentTarget::Source {
                    slot,
                    start: length(start)?,
                },
                None => SegmentTarget::Synthetic,
            },
        });
    }
    Ok(())
}

fn coverage(
    parsed: &ParsedDocument,
    session: Option<&CompileTimeOutput>,
    slots: &[IncludeSlot<'_>],
    facts: &ArtifactFacts,
) -> Vec<CoverageReason> {
    let mut reasons = BTreeSet::new();
    if !parsed.conditionals.unknown_spans.is_empty() {
        reasons.insert(CoverageReason::ConditionalUnknown);
    }
    if !parsed.parser_recovery_spans.is_empty() {
        reasons.insert(CoverageReason::ParserRecovery);
    }
    if slots
        .iter()
        .any(|slot| matches!(slot.outcome, IncludeSlotOutcome::Unresolved(_)))
    {
        reasons.insert(CoverageReason::UnresolvedInclude);
    }
    if !parsed.conditionals.complete || session.is_some_and(|session| !session.reasons.is_empty()) {
        reasons.insert(CoverageReason::ExpansionIncomplete);
    }
    let unmapped = |location: &FactLocation| matches!(location, FactLocation::Expanded { .. });
    if facts.declarations.iter().any(|d| unmapped(&d.location))
        || facts.imports.iter().any(|i| unmapped(&i.location))
    {
        reasons.insert(CoverageReason::UnmappedLocation);
    }
    reasons.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::Document;
    use lsp_types::Url;
    use pascal_project::ConditionalFact;

    const CONDITIONAL: &str = "unit Cond;\ninterface\n{$IFDEF MAYBE}\nconst Maybe = 1;\n{$ENDIF}\n{$IFDEF NEVER}\nconst Never = 2;\n{$ENDIF}\nconst Always = 3;\nimplementation\nend.\n";

    fn parse(source: &str, context: &ConditionalContext) -> std::sync::Arc<ParsedDocument> {
        let path = if cfg!(windows) {
            "C:/w/Unit.pas"
        } else {
            "/w/Unit.pas"
        };
        Document::parse_with_cancel(
            Url::from_file_path(path).unwrap(),
            source.to_owned(),
            context,
            &AtomicBool::new(false),
            None,
        )
        .unwrap()
        .parsed
    }

    fn never_defined() -> ConditionalContext {
        let mut context = ConditionalContext::default();
        context.set_define("NEVER", ConditionalFact::False);
        context
    }

    fn extract(parsed: &ParsedDocument) -> ArtifactFacts {
        extract_facts(
            parsed,
            None,
            &mut FactBudget::new(1 << 24),
            &AtomicBool::new(false),
        )
        .unwrap()
        .facts
    }

    fn root_text(source: &str, location: FactLocation) -> &str {
        let FactLocation::Source { slot: 0, range } = location else {
            panic!("{location:?} is not in the root")
        };
        &source[range.to_range()]
    }

    fn activities(facts: &ArtifactFacts, name: &str) -> Vec<PostingActivity> {
        facts
            .postings
            .iter()
            .find(|posting| posting.name == name)
            .unwrap_or_else(|| panic!("no posting {name}"))
            .occurrences
            .iter()
            .map(|occurrence| occurrence.activity)
            .collect()
    }

    #[test]
    fn declarations_keep_conditional_certainty_and_root_locations() {
        let parsed = parse(CONDITIONAL, &never_defined());
        let extracted = extract_facts(
            &parsed,
            None,
            &mut FactBudget::new(1 << 24),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(extracted.conditional_context, never_defined());
        let facts = extracted.facts;
        let summary: Vec<_> = facts
            .declarations
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    declaration.kind,
                    declaration.certainty.activity,
                    root_text(CONDITIONAL, declaration.location),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("cond", DeclarationKind::Unit, Activity::Active, "Cond"),
                (
                    "maybe",
                    DeclarationKind::Constant,
                    Activity::Unknown,
                    "Maybe"
                ),
                (
                    "always",
                    DeclarationKind::Constant,
                    Activity::Active,
                    "Always"
                ),
            ]
        );
        assert!(facts.declarations.iter().all(|d| !d.certainty.recovered));
        assert_eq!(facts.coverage, vec![CoverageReason::ConditionalUnknown]);
        assert_eq!(facts.slot_lengths, vec![CONDITIONAL.len() as u32]);
        assert_eq!(facts.expanded_length, CONDITIONAL.len() as u32);
        assert!(facts.includes.is_empty() && facts.segments.is_empty());
    }

    #[test]
    fn a_path_derived_unit_name_is_not_a_declaration() {
        let parsed = parse(
            "unit ;\ninterface\nimplementation\nend.\n",
            &ConditionalContext::default(),
        );
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Unit && symbol.key == "unit"),
            "the fixture must take the file-name fallback"
        );
        let facts = extract(&parsed);
        assert!(
            facts
                .declarations
                .iter()
                .all(|d| d.kind != DeclarationKind::Unit && d.name != "unit"),
            "{:?}",
            facts.declarations
        );
        let dotted = extract(&parse(
            "unit Vendor . Sub;\ninterface\nimplementation\nend.\n",
            &ConditionalContext::default(),
        ));
        assert!(
            dotted
                .declarations
                .iter()
                .any(|d| d.kind == DeclarationKind::Unit && d.name == "vendor.sub"),
            "a written dotted name stays a declaration: {:?}",
            dotted.declarations
        );
    }

    #[test]
    fn members_name_an_earlier_owner_as_their_parent() {
        let source = "unit Shapes;\ninterface\ntype\n  TShape = class\n    FWidth: Integer;\n    procedure Draw;\n    property Width: Integer read FWidth;\n  end;\nimplementation\nprocedure TShape.Draw;\nbegin\nend;\nend.\n";
        let facts = extract(&parse(source, &ConditionalContext::default()));
        let shape = facts
            .declarations
            .iter()
            .find(|d| d.name == "tshape" && d.kind == DeclarationKind::Type)
            .expect("the class type is declared");
        for member in ["fwidth", "draw", "width"] {
            let declaration = facts
                .declarations
                .iter()
                .find(|d| d.name == member && d.origin == Origin::Declaration)
                .unwrap_or_else(|| panic!("no member {member}"));
            assert_eq!(declaration.parent, Some(shape.id), "{member}");
        }
        for declaration in &facts.declarations {
            if let Some(parent) = declaration.parent {
                assert!(
                    parent < declaration.id,
                    "{} names a later parent",
                    declaration.name
                );
            }
        }
    }

    #[test]
    fn postings_are_lexical_runs_tagged_with_their_conditional_context() {
        let facts = extract(&parse(CONDITIONAL, &never_defined()));
        assert_eq!(
            activities(&facts, "maybe"),
            vec![PostingActivity::Directive, PostingActivity::Unknown]
        );
        assert_eq!(
            activities(&facts, "never"),
            vec![PostingActivity::Directive, PostingActivity::Inactive]
        );
        assert_eq!(activities(&facts, "always"), vec![PostingActivity::Active]);
        assert_eq!(
            activities(&facts, "ifdef"),
            vec![PostingActivity::Directive; 2]
        );
        let runs = identifier_runs(CONDITIONAL.as_bytes())
            .filter(|run| !CONDITIONAL.as_bytes()[run.start].is_ascii_digit())
            .count();
        let postings: usize = facts.postings.iter().map(|p| p.occurrences.len()).sum();
        assert_eq!(
            postings, runs,
            "every identifier-led lexical run is a posting occurrence"
        );
        for posting in &facts.postings {
            for occurrence in &posting.occurrences {
                assert!(
                    CONDITIONAL[occurrence.range.to_range()].eq_ignore_ascii_case(&posting.name)
                );
            }
        }
        assert!(
            facts
                .postings
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
        );
    }

    #[test]
    fn digit_led_runs_are_not_postings() {
        let source = "unit N;\ninterface\nconst X = 123 + $1F;\nimplementation\nend.\n";
        let facts = extract(&parse(source, &ConditionalContext::default()));
        assert!(
            facts
                .postings
                .iter()
                .all(|p| !p.name.as_bytes()[0].is_ascii_digit())
        );
        assert_eq!(activities(&facts, "x"), vec![PostingActivity::Active]);
    }

    #[test]
    fn recovered_facts_are_never_proven() {
        let source =
            "unit Broken;\ninterface\nprocedure Bad(;\nconst After = 1;\nimplementation\nend.\n";
        let parsed = parse(source, &ConditionalContext::default());
        assert!(
            !parsed.parser_recovery_spans.is_empty(),
            "the fixture must need recovery"
        );
        let facts = extract(&parsed);
        assert!(facts.coverage.contains(&CoverageReason::ParserRecovery));
        let declared = parsed
            .symbols
            .iter()
            .filter(|symbol| declared(symbol).is_some());
        for (declaration, symbol) in facts.declarations.iter().zip(declared) {
            let overlaps = parsed
                .parser_recovery_spans
                .iter()
                .any(|r| r.start <= symbol.span.end && symbol.span.start <= r.end);
            assert_eq!(
                declaration.certainty.recovered, overlaps,
                "{}",
                declaration.name
            );
            assert!(!overlaps || !declaration.certainty.is_proven());
        }
    }

    #[test]
    fn extraction_stops_on_cancellation_and_admission() {
        let parsed = parse(CONDITIONAL, &ConditionalContext::default());
        assert_eq!(
            extract_facts(
                &parsed,
                None,
                &mut FactBudget::new(1 << 24),
                &AtomicBool::new(true)
            )
            .unwrap_err(),
            FactError::Cancelled
        );
        let mut budget = FactBudget::new(256);
        assert_eq!(
            extract_facts(&parsed, None, &mut budget, &AtomicBool::new(false)).unwrap_err(),
            FactError::Budget { limit: 256 }
        );
        assert!(budget.used() <= 256);
    }
}
