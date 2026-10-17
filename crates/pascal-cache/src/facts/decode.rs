use super::encode::{
    activity_tag, coverage_tag, expansion_tag, kind_tag, origin_tag, posting_activity_tag,
    region_tag, section_tag, visibility_tag,
};
use super::{
    DECLARATION_ROW, IMPORT_ROW, INCLUDE_ROW, LENGTH_BYTES, OCCURRENCE_BYTES, POSTING_ROW,
    SEGMENT_ROW,
};
use crate::StoredRow;
use pascal_core::ImportSection;
use pascal_core::facts::{
    Activity, ArtifactFacts, Certainty, CoverageReason, DeclarationFact, DeclarationKind,
    FactBudget, FactLocation, ImportFact, IncludeExpansion, IncludeFact, Origin, PostingActivity,
    PostingFact, PostingOccurrence, Region, SegmentFact, SegmentTarget, TextRange, Visibility,
    is_identifier_byte,
};
use std::mem::size_of;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactDecodeError {
    /// The decoded facts did not fit the caller's admission.
    Budget,
    /// The rows break a DTO invariant; never read as empty facts.
    Malformed(&'static str),
}

type Decoded<T> = Result<T, FactDecodeError>;

fn malformed<T>(reason: &'static str) -> Decoded<T> {
    Err(FactDecodeError::Malformed(reason))
}

/// Bounded positional reads; a length is checked against what remains
/// before anything is taken.
struct ByteReader<'a> {
    bytes: &'a [u8],
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, count: usize) -> Decoded<&'a [u8]> {
        if count > self.bytes.len() {
            return malformed("truncated payload");
        }
        let (head, tail) = self.bytes.split_at(count);
        self.bytes = tail;
        Ok(head)
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }

    fn u8(&mut self) -> Decoded<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Decoded<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn length(&mut self) -> Decoded<usize> {
        let length = u64::from_le_bytes(self.take(LENGTH_BYTES)?.try_into().expect("eight bytes"));
        usize::try_from(length).or_else(|_| malformed("length overflows"))
    }

    fn bool(&mut self) -> Decoded<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => malformed("invalid bool"),
        }
    }

    fn str(&mut self) -> Decoded<&'a str> {
        let length = self.length()?;
        std::str::from_utf8(self.take(length)?).or_else(|_| malformed("text is not UTF-8"))
    }

    fn range(&mut self) -> Decoded<TextRange> {
        let (start, end) = (self.u32()?, self.u32()?);
        if start > end {
            return malformed("inverted range");
        }
        Ok(TextRange { start, end })
    }

    fn location(&mut self) -> Decoded<FactLocation> {
        match self.u8()? {
            1 => Ok(FactLocation::Source {
                slot: self.u32()?,
                range: self.range()?,
            }),
            2 => Ok(FactLocation::Expanded {
                range: self.range()?,
            }),
            _ => malformed("unknown location tag"),
        }
    }

    fn certainty(&mut self) -> Decoded<Certainty> {
        Ok(Certainty {
            activity: tagged(self.u8()?, &ACTIVITIES, activity_tag)?,
            recovered: self.bool()?,
        })
    }

    fn finish(self) -> Decoded<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            malformed("trailing payload bytes")
        }
    }
}

const KINDS: [DeclarationKind; 8] = [
    DeclarationKind::Unit,
    DeclarationKind::Type,
    DeclarationKind::Routine,
    DeclarationKind::Variable,
    DeclarationKind::Constant,
    DeclarationKind::Field,
    DeclarationKind::Property,
    DeclarationKind::EnumValue,
];
const VISIBILITIES: [Visibility; 6] = [
    Visibility::Public,
    Visibility::Published,
    Visibility::Private,
    Visibility::Protected,
    Visibility::StrictPrivate,
    Visibility::StrictProtected,
];
const REGIONS: [Region; 3] = [Region::Interface, Region::Implementation, Region::Other];
const ORIGINS: [Origin; 2] = [Origin::Declaration, Origin::Definition];
const SECTIONS: [ImportSection; 3] = [
    ImportSection::Module,
    ImportSection::Interface,
    ImportSection::Implementation,
];
const ACTIVITIES: [Activity; 3] = [Activity::Active, Activity::Inactive, Activity::Unknown];
const POSTING_ACTIVITIES: [PostingActivity; 4] = [
    PostingActivity::Active,
    PostingActivity::Inactive,
    PostingActivity::Unknown,
    PostingActivity::Directive,
];
const EXPANSIONS: [IncludeExpansion; 2] =
    [IncludeExpansion::Expanded, IncludeExpansion::Unresolved];
const REASONS: [CoverageReason; 5] = [
    CoverageReason::ConditionalUnknown,
    CoverageReason::ParserRecovery,
    CoverageReason::UnresolvedInclude,
    CoverageReason::ExpansionIncomplete,
    CoverageReason::UnmappedLocation,
];

/// The variant whose encoder tag is `tag`, so both directions share one
/// table.
fn tagged<T: Copy>(tag: u8, variants: &[T], encode: fn(T) -> u8) -> Decoded<T> {
    match variants
        .iter()
        .copied()
        .find(|variant| encode(*variant) == tag)
    {
        Some(variant) => Ok(variant),
        None => malformed("unknown tag"),
    }
}

fn index_name(key: Option<&[u8]>) -> Decoded<&str> {
    match key {
        Some(key) if !key.is_empty() => {
            std::str::from_utf8(key).or_else(|_| malformed("index key is not UTF-8"))
        }
        _ => malformed("missing index key"),
    }
}

/// A posting key is a normalized lexical run: lowercase identifier bytes,
/// not digit-led.
fn posting_name(key: Option<&[u8]>) -> Decoded<&str> {
    let name = index_name(key)?;
    let bytes = name.as_bytes();
    if bytes[0].is_ascii_digit()
        || !bytes
            .iter()
            .all(|byte| is_identifier_byte(*byte) && !byte.is_ascii_uppercase())
    {
        return malformed("posting name is not a normalized run");
    }
    Ok(name)
}

pub struct FactDecoder<'b> {
    budget: &'b mut FactBudget,
    facts: ArtifactFacts,
    posting_rows: usize,
    last_kind: u8,
}

impl<'b> FactDecoder<'b> {
    pub fn new(budget: &'b mut FactBudget) -> Self {
        Self {
            budget,
            facts: ArtifactFacts::default(),
            posting_rows: 0,
            last_kind: 0,
        }
    }

    fn charge(&mut self, bytes: usize) -> Decoded<()> {
        self.budget
            .charge(bytes)
            .map_err(|_| FactDecodeError::Budget)
    }

    /// Decode one batch, in ordinal order. Only row-local invariants are
    /// checked here; [`Self::finish`] checks the rest against the header.
    pub fn push(&mut self, rows: &[StoredRow]) -> Decoded<()> {
        for row in rows {
            if row.row_kind < self.last_kind {
                return malformed("row kinds out of order");
            }
            self.last_kind = row.row_kind;
            let key = row.index_key.as_deref();
            let mut reader = ByteReader::new(&row.payload);
            match row.row_kind {
                DECLARATION_ROW => self.declaration(index_name(key)?, &mut reader)?,
                IMPORT_ROW => self.import(index_name(key)?, &mut reader)?,
                POSTING_ROW => self.posting(posting_name(key)?, &mut reader)?,
                INCLUDE_ROW | SEGMENT_ROW if key.is_some() => {
                    return malformed("unexpected index key");
                }
                INCLUDE_ROW => self.include(&mut reader)?,
                SEGMENT_ROW => self.segment(&mut reader)?,
                _ => return malformed("unknown row kind"),
            }
            reader.finish()?;
        }
        Ok(())
    }

    fn declaration(&mut self, name: &str, reader: &mut ByteReader<'_>) -> Decoded<()> {
        if reader.u32()? as usize != self.facts.declarations.len() {
            return malformed("declaration ids are not dense");
        }
        let display = reader.str()?;
        let kind = tagged(reader.u8()?, &KINDS, kind_tag)?;
        let visibility = tagged(reader.u8()?, &VISIBILITIES, visibility_tag)?;
        let region = tagged(reader.u8()?, &REGIONS, region_tag)?;
        let origin = tagged(reader.u8()?, &ORIGINS, origin_tag)?;
        let exported = reader.bool()?;
        let parent = match reader.u8()? {
            0 => None,
            1 => Some(reader.u32()?),
            _ => return malformed("invalid option"),
        };
        let location = reader.location()?;
        let certainty = reader.certainty()?;
        self.charge(size_of::<DeclarationFact>() + name.len() + display.len())?;
        self.facts.declarations.push(DeclarationFact {
            id: self.facts.declarations.len() as u32,
            name: name.to_owned(),
            display: display.to_owned(),
            kind,
            visibility,
            region,
            origin,
            exported,
            parent,
            location,
            certainty,
        });
        Ok(())
    }

    fn import(&mut self, name: &str, reader: &mut ByteReader<'_>) -> Decoded<()> {
        if reader.u32()? as usize != self.facts.imports.len() {
            return malformed("import ids are not dense");
        }
        let display = reader.str()?;
        let section = tagged(reader.u8()?, &SECTIONS, section_tag)?;
        let location = reader.location()?;
        let certainty = reader.certainty()?;
        self.charge(size_of::<ImportFact>() + name.len() + display.len())?;
        self.facts.imports.push(ImportFact {
            id: self.facts.imports.len() as u32,
            name: name.to_owned(),
            display: display.to_owned(),
            section,
            location,
            certainty,
        });
        Ok(())
    }

    fn posting(&mut self, name: &str, reader: &mut ByteReader<'_>) -> Decoded<()> {
        let count = reader.length()?;
        if count == 0 || count > reader.remaining() / OCCURRENCE_BYTES {
            return malformed("invalid posting count");
        }
        self.charge(count * size_of::<PostingOccurrence>())?;
        let continues = match self.facts.postings.last() {
            Some(last) if last.name == name => true,
            Some(last) if last.name.as_str() > name => {
                return malformed("posting names out of order");
            }
            _ => false,
        };
        if !continues {
            self.charge(size_of::<PostingFact>() + name.len())?;
            self.facts.postings.push(PostingFact {
                name: name.to_owned(),
                occurrences: Vec::new(),
            });
        }
        let posting = self.facts.postings.last_mut().expect("pushed above");
        posting.occurrences.reserve_exact(count);
        for _ in 0..count {
            let occurrence = PostingOccurrence {
                slot: reader.u32()?,
                range: reader.range()?,
                activity: tagged(reader.u8()?, &POSTING_ACTIVITIES, posting_activity_tag)?,
            };
            if occurrence.range.len() as usize != name.len() {
                return malformed("posting range is not the name");
            }
            if let Some(previous) = posting.occurrences.last()
                && (previous.slot, previous.range.end) > (occurrence.slot, occurrence.range.start)
            {
                return malformed("posting occurrences out of order");
            }
            posting.occurrences.push(occurrence);
        }
        self.posting_rows += 1;
        Ok(())
    }

    fn include(&mut self, reader: &mut ByteReader<'_>) -> Decoded<()> {
        let include = IncludeFact {
            slot: reader.u32()?,
            parent: reader.u32()?,
            directive: reader.range()?,
            expansion: tagged(reader.u8()?, &EXPANSIONS, expansion_tag)?,
        };
        if include.slot as usize != self.facts.includes.len() + 1 || include.parent >= include.slot
        {
            return malformed("include slots are not in expansion order");
        }
        self.charge(size_of::<IncludeFact>())?;
        self.facts.includes.push(include);
        Ok(())
    }

    fn segment(&mut self, reader: &mut ByteReader<'_>) -> Decoded<()> {
        let expanded = reader.range()?;
        let target = match reader.u8()? {
            1 => SegmentTarget::Source {
                slot: reader.u32()?,
                start: reader.u32()?,
            },
            2 => SegmentTarget::Synthetic,
            _ => return malformed("unknown segment tag"),
        };
        let start = self
            .facts
            .segments
            .last()
            .map_or(0, |last| last.expanded.end);
        if expanded.start != start || expanded.is_empty() {
            return malformed("segments are not contiguous");
        }
        self.charge(size_of::<SegmentFact>())?;
        self.facts.segments.push(SegmentFact { expanded, target });
        Ok(())
    }

    /// Check the decoded rows against the verified supplemental header and
    /// every cross-row invariant.
    pub fn finish(mut self, supplemental: &[u8]) -> Decoded<ArtifactFacts> {
        let mut reader = ByteReader::new(supplemental);
        let mut counts = [0usize; 5];
        for count in &mut counts {
            *count = reader.u32()? as usize;
        }
        let slots = reader.length()?;
        if slots > reader.remaining() / 4 {
            return malformed("slot count exceeds the header");
        }
        self.charge(slots * size_of::<u32>())?;
        let mut slot_lengths = Vec::with_capacity(slots);
        for _ in 0..slots {
            slot_lengths.push(reader.u32()?);
        }
        let expanded_length = reader.u32()?;
        let reasons = reader.length()?;
        if reasons > reader.remaining() {
            return malformed("coverage count exceeds the header");
        }
        let mut coverage: Vec<CoverageReason> = Vec::with_capacity(reasons);
        for _ in 0..reasons {
            let reason = tagged(reader.u8()?, &REASONS, coverage_tag)?;
            if coverage.last().is_some_and(|last| *last >= reason) {
                return malformed("coverage is not sorted");
            }
            coverage.push(reason);
        }
        reader.finish()?;
        let facts = &mut self.facts;
        if counts
            != [
                facts.declarations.len(),
                facts.imports.len(),
                self.posting_rows,
                facts.includes.len(),
                facts.segments.len(),
            ]
        {
            return malformed("row counts disagree with the header");
        }
        facts.slot_lengths = slot_lengths;
        facts.expanded_length = expanded_length;
        facts.coverage = coverage;
        validate(facts)?;
        Ok(self.facts)
    }
}

fn validate(facts: &ArtifactFacts) -> Decoded<()> {
    if facts.slot_lengths.len() != facts.includes.len() + 1 {
        return malformed("slots disagree with includes");
    }
    let slot = |slot: u32| match facts.slot_lengths.get(slot as usize) {
        Some(length) => Ok(*length),
        None => malformed("unknown slot"),
    };
    let within = |range: TextRange, length: u32| {
        if range.end <= length {
            Ok(())
        } else {
            malformed("range beyond its text")
        }
    };
    let located = |location: &FactLocation| match *location {
        FactLocation::Source { slot: index, range } => within(range, slot(index)?),
        FactLocation::Expanded { range } => within(range, facts.expanded_length),
    };
    for declaration in &facts.declarations {
        located(&declaration.location)?;
        if let Some(parent) = declaration.parent
            && (parent == declaration.id || parent as usize >= facts.declarations.len())
        {
            return malformed("invalid declaration parent");
        }
    }
    for import in &facts.imports {
        located(&import.location)?;
    }
    for posting in &facts.postings {
        for occurrence in &posting.occurrences {
            within(occurrence.range, slot(occurrence.slot)?)?;
        }
    }
    for include in &facts.includes {
        within(include.directive, slot(include.parent)?)?;
    }
    match (facts.includes.is_empty(), facts.segments.last()) {
        (true, None) if facts.expanded_length == facts.slot_lengths[0] => {}
        (false, Some(last)) if last.expanded.end == facts.expanded_length => {}
        _ => return malformed("segments do not cover the analysed text"),
    }
    for segment in &facts.segments {
        if let SegmentTarget::Source { slot: index, start } = segment.target
            && u64::from(start) + u64::from(segment.expanded.len()) > u64::from(slot(index)?)
        {
            return malformed("segment beyond its slot");
        }
    }
    Ok(())
}
