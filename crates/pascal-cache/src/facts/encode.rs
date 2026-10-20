use super::{
    DECLARATION_ROW, IMPORT_ROW, INCLUDE_ROW, LENGTH_BYTES, OCCURRENCE_BYTES, POSTING_ROW,
    SEGMENT_ROW,
};
use crate::{
    ArtifactRow, Encoder, MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS, MAX_INDEX_KEY_BYTES,
    MAX_ROW_BATCH_BYTES, MAX_SUPPLEMENTAL_BYTES, TooLarge,
};
use pascal_core::ImportSection;
use pascal_core::facts::{
    Activity, ArtifactFacts, Certainty, CoverageReason, DeclarationFact, DeclarationKind,
    FactLocation, ImportFact, IncludeExpansion, Origin, PostingActivity, Region, SegmentFact,
    SegmentTarget, TextRange, Visibility,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRow {
    pub row_kind: u8,
    pub index_key: Option<Vec<u8>>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFacts {
    pub rows: Vec<EncodedRow>,
    pub supplemental: Vec<u8>,
}

impl EncodedFacts {
    pub fn rows(&self) -> Vec<ArtifactRow<'_>> {
        self.rows
            .iter()
            .map(|row| ArtifactRow {
                row_kind: row.row_kind,
                index_key: row.index_key.as_deref(),
                payload: &row.payload,
            })
            .collect()
    }

    /// Bytes of payloads and index keys, as the store counts them.
    pub fn content_bytes(&self) -> usize {
        self.rows
            .iter()
            .map(|row| row.payload.len() + row.index_key.as_ref().map_or(0, Vec::len))
            .sum()
    }
}

pub(super) fn kind_tag(kind: DeclarationKind) -> u8 {
    match kind {
        DeclarationKind::Unit => 1,
        DeclarationKind::Type => 2,
        DeclarationKind::Routine => 3,
        DeclarationKind::Variable => 4,
        DeclarationKind::Constant => 5,
        DeclarationKind::Field => 6,
        DeclarationKind::Property => 7,
        DeclarationKind::EnumValue => 8,
    }
}

pub(super) fn visibility_tag(visibility: Visibility) -> u8 {
    match visibility {
        Visibility::Public => 1,
        Visibility::Published => 2,
        Visibility::Private => 3,
        Visibility::Protected => 4,
        Visibility::StrictPrivate => 5,
        Visibility::StrictProtected => 6,
    }
}

pub(super) fn region_tag(region: Region) -> u8 {
    match region {
        Region::Interface => 1,
        Region::Implementation => 2,
        Region::Other => 3,
    }
}

pub(super) fn origin_tag(origin: Origin) -> u8 {
    match origin {
        Origin::Declaration => 1,
        Origin::Definition => 2,
    }
}

pub(super) fn section_tag(section: ImportSection) -> u8 {
    match section {
        ImportSection::Module => 1,
        ImportSection::Interface => 2,
        ImportSection::Implementation => 3,
    }
}

pub(super) fn activity_tag(activity: Activity) -> u8 {
    match activity {
        Activity::Active => 1,
        Activity::Inactive => 2,
        Activity::Unknown => 3,
    }
}

pub(super) fn posting_activity_tag(activity: PostingActivity) -> u8 {
    match activity {
        PostingActivity::Active => 1,
        PostingActivity::Inactive => 2,
        PostingActivity::Unknown => 3,
        PostingActivity::Directive => 4,
    }
}

pub(super) fn expansion_tag(expansion: IncludeExpansion) -> u8 {
    match expansion {
        IncludeExpansion::Expanded => 1,
        IncludeExpansion::Unresolved => 2,
    }
}

pub(super) fn coverage_tag(reason: CoverageReason) -> u8 {
    match reason {
        CoverageReason::ConditionalUnknown => 1,
        CoverageReason::ParserRecovery => 2,
        CoverageReason::UnresolvedInclude => 3,
        CoverageReason::ExpansionIncomplete => 4,
        CoverageReason::UnmappedLocation => 5,
    }
}

fn location_bytes(location: &FactLocation) -> usize {
    match location {
        FactLocation::Source { .. } => 13,
        FactLocation::Expanded { .. } => 9,
    }
}

fn declaration_bytes(declaration: &DeclarationFact) -> usize {
    4 + LENGTH_BYTES
        + declaration.display.len()
        + 5
        + if declaration.parent.is_some() { 5 } else { 1 }
        + location_bytes(&declaration.location)
        + 2
}

fn import_bytes(import: &ImportFact) -> usize {
    4 + LENGTH_BYTES + import.display.len() + 1 + location_bytes(&import.location) + 2
}

const INCLUDE_BYTES: usize = 17;

fn segment_bytes(segment: &SegmentFact) -> usize {
    9 + match segment.target {
        SegmentTarget::Source { .. } => 8,
        SegmentTarget::Synthetic => 0,
    }
}

fn supplemental_bytes(facts: &ArtifactFacts) -> usize {
    20 + LENGTH_BYTES + 4 * facts.slot_lengths.len() + 4 + LENGTH_BYTES + facts.coverage.len()
}

/// How many occurrences of `name` one posting row holds within a batch.
fn occurrences_per_row(name: &str) -> usize {
    (MAX_ROW_BATCH_BYTES - name.len() - LENGTH_BYTES) / OCCURRENCE_BYTES
}

#[derive(Default)]
struct Plan {
    rows: usize,
    content: usize,
}

impl Plan {
    fn key(&self, bytes: usize) -> Result<(), TooLarge> {
        if bytes > MAX_INDEX_KEY_BYTES {
            return Err(TooLarge::IndexKey {
                ordinal: self.rows,
                bytes,
            });
        }
        Ok(())
    }

    fn row(&mut self, key: Option<usize>, payload: usize) -> Result<(), TooLarge> {
        if let Some(bytes) = key {
            self.key(bytes)?;
        }
        let bytes = payload + key.unwrap_or(0);
        if bytes > MAX_ROW_BATCH_BYTES {
            return Err(TooLarge::Row {
                ordinal: self.rows,
                bytes,
            });
        }
        self.rows += 1;
        if self.rows > MAX_ARTIFACT_ROWS {
            return Err(TooLarge::Rows { count: self.rows });
        }
        self.content += bytes;
        if self.content > MAX_ARTIFACT_CONTENT_BYTES {
            return Err(TooLarge::Content {
                bytes: self.content,
            });
        }
        Ok(())
    }
}

/// The content bytes [`encode_facts`] produces, checked against every store
/// limit without allocating.
pub fn encoded_content_bytes(facts: &ArtifactFacts) -> Result<usize, TooLarge> {
    let supplemental = supplemental_bytes(facts);
    if supplemental > MAX_SUPPLEMENTAL_BYTES {
        return Err(TooLarge::Supplemental {
            bytes: supplemental,
        });
    }
    let mut plan = Plan::default();
    for declaration in &facts.declarations {
        plan.row(Some(declaration.name.len()), declaration_bytes(declaration))?;
    }
    for import in &facts.imports {
        plan.row(Some(import.name.len()), import_bytes(import))?;
    }
    for posting in &facts.postings {
        plan.key(posting.name.len())?;
        for chunk in posting
            .occurrences
            .chunks(occurrences_per_row(&posting.name))
        {
            plan.row(
                Some(posting.name.len()),
                LENGTH_BYTES + OCCURRENCE_BYTES * chunk.len(),
            )?;
        }
    }
    for _ in &facts.includes {
        plan.row(None, INCLUDE_BYTES)?;
    }
    for segment in &facts.segments {
        plan.row(None, segment_bytes(segment))?;
    }
    Ok(plan.content)
}

fn range(encoder: &mut Encoder, range: TextRange) {
    encoder.u32(range.start);
    encoder.u32(range.end);
}

fn location(encoder: &mut Encoder, location: &FactLocation) {
    match *location {
        FactLocation::Source { slot, range: at } => {
            encoder.variant(1);
            encoder.u32(slot);
            range(encoder, at);
        }
        FactLocation::Expanded { range: at } => {
            encoder.variant(2);
            range(encoder, at);
        }
    }
}

fn certainty(encoder: &mut Encoder, certainty: Certainty) {
    encoder.u8(activity_tag(certainty.activity));
    encoder.bool(certainty.recovered);
}

fn row(row_kind: u8, key: Option<&str>, encode: impl FnOnce(&mut Encoder)) -> EncodedRow {
    let mut encoder = Encoder::new();
    encode(&mut encoder);
    EncodedRow {
        row_kind,
        index_key: key.map(|key| key.as_bytes().to_vec()),
        payload: encoder.into_bytes(),
    }
}

/// Encode `facts` as rows and a supplemental header, refusing anything over
/// a store limit before allocating.
pub fn encode_facts(facts: &ArtifactFacts) -> Result<EncodedFacts, TooLarge> {
    encoded_content_bytes(facts)?;
    let mut rows = Vec::new();
    for declaration in &facts.declarations {
        rows.push(row(
            DECLARATION_ROW,
            Some(declaration.name.as_str()),
            |encoder| {
                encoder.u32(declaration.id);
                encoder.str(&declaration.display);
                encoder.u8(kind_tag(declaration.kind));
                encoder.u8(visibility_tag(declaration.visibility));
                encoder.u8(region_tag(declaration.region));
                encoder.u8(origin_tag(declaration.origin));
                encoder.bool(declaration.exported);
                encoder.option(declaration.parent.as_ref(), |encoder, parent| {
                    encoder.u32(*parent)
                });
                location(encoder, &declaration.location);
                certainty(encoder, declaration.certainty);
            },
        ));
    }
    for import in &facts.imports {
        rows.push(row(IMPORT_ROW, Some(import.name.as_str()), |encoder| {
            encoder.u32(import.id);
            encoder.str(&import.display);
            encoder.u8(section_tag(import.section));
            location(encoder, &import.location);
            certainty(encoder, import.certainty);
        }));
    }
    for posting in &facts.postings {
        for chunk in posting
            .occurrences
            .chunks(occurrences_per_row(&posting.name))
        {
            rows.push(row(POSTING_ROW, Some(posting.name.as_str()), |encoder| {
                encoder.length(chunk.len());
                for occurrence in chunk {
                    encoder.u32(occurrence.slot);
                    range(encoder, occurrence.range);
                    encoder.u8(posting_activity_tag(occurrence.activity));
                }
            }));
        }
    }
    for include in &facts.includes {
        rows.push(row(INCLUDE_ROW, None, |encoder| {
            encoder.u32(include.slot);
            encoder.u32(include.parent);
            range(encoder, include.directive);
            encoder.u8(expansion_tag(include.expansion));
        }));
    }
    for segment in &facts.segments {
        rows.push(row(SEGMENT_ROW, None, |encoder| {
            range(encoder, segment.expanded);
            match segment.target {
                SegmentTarget::Source { slot, start } => {
                    encoder.variant(1);
                    encoder.u32(slot);
                    encoder.u32(start);
                }
                SegmentTarget::Synthetic => encoder.variant(2),
            }
        }));
    }
    let posting_rows = rows
        .iter()
        .filter(|row| row.row_kind == POSTING_ROW)
        .count();
    let mut header = Encoder::new();
    for count in [
        facts.declarations.len(),
        facts.imports.len(),
        posting_rows,
        facts.includes.len(),
        facts.segments.len(),
    ] {
        header.u32(count as u32);
    }
    header.seq(&facts.slot_lengths, |encoder, length| encoder.u32(*length));
    header.u32(facts.expanded_length);
    header.seq(&facts.coverage, |encoder, reason| {
        encoder.u8(coverage_tag(*reason))
    });
    Ok(EncodedFacts {
        rows,
        supplemental: header.into_bytes(),
    })
}
