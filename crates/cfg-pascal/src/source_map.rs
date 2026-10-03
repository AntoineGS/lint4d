//! Validated mappings between a prepared Pascal buffer and its source files.
//!
//! The mapping is deliberately a caller-supplied contract.  This crate does
//! not discover include files or decide which conditional branch is active;
//! it only validates the byte-level projection that a future preparer hands to
//! it.  In particular, a synthetic segment has no source origin and a masked
//! segment is still required to preserve the original line breaks.

use std::{collections::HashMap, fmt, ops::Range, sync::Arc};

use crate::ProjectSourceId;

/// An immutable source revision that can be used as an origin in a
/// [`SourceMap`].
///
/// `source_id` is the caller's identity for the exact byte snapshot.  It is
/// not a path guessed by this crate and it need not have any particular
/// spelling.  The bytes are stored in an [`Arc`] so maps and prepared units
/// can retain the same immutable revision without copying it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    source_id: ProjectSourceId,
    bytes: Arc<[u8]>,
}

impl SourceSnapshot {
    /// Create an immutable source snapshot.
    ///
    /// Empty IDs are accepted here to keep construction consistent with
    /// [`ProjectUnitInput::new`](crate::ProjectUnitInput::new); they are
    /// rejected when the snapshot is used to build a validated map or
    /// project.  Use [`Self::try_new`] when the source object itself should
    /// enforce the ID invariant.
    pub fn new(source_id: ProjectSourceId, bytes: impl AsRef<[u8]>) -> Self {
        Self {
            source_id,
            bytes: Arc::from(bytes.as_ref()),
        }
    }

    /// Create a source snapshot and reject an empty source identity.
    pub fn try_new(
        source_id: ProjectSourceId,
        bytes: impl AsRef<[u8]>,
    ) -> Result<Self, SourceSnapshotError> {
        if source_id.as_str().is_empty() {
            return Err(SourceSnapshotError::EmptySourceId);
        }
        Ok(Self::new(source_id, bytes))
    }

    /// Stable identity of this exact source revision.
    pub fn source_id(&self) -> &ProjectSourceId {
        &self.source_id
    }

    /// Borrow the immutable source bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Number of bytes in this source revision.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether this source revision is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Validation failures for an individual [`SourceSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceSnapshotError {
    /// The source identity was empty.
    EmptySourceId,
}

impl fmt::Display for SourceSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySourceId => formatter.write_str("source snapshot ID must not be empty"),
        }
    }
}

impl std::error::Error for SourceSnapshotError {}

/// A range in one original source snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    /// Identity of the source containing [`Self::byte_range`].
    pub source_id: ProjectSourceId,
    /// Half-open byte range in the source snapshot.
    pub byte_range: Range<usize>,
}

impl SourceSpan {
    /// Construct an original source span.
    pub fn new(source_id: ProjectSourceId, byte_range: Range<usize>) -> Self {
        Self {
            source_id,
            byte_range,
        }
    }

    /// Source identity for this span.
    pub fn source_id(&self) -> &ProjectSourceId {
        &self.source_id
    }

    /// Borrow the half-open source byte range.
    pub fn byte_range(&self) -> Range<usize> {
        self.byte_range.clone()
    }
}

/// Caller-assigned identity for one expansion occurrence.
///
/// Repeated inclusion of the same source must use a different ID for each
/// occurrence.  Nested occurrences can use a caller-defined hierarchy such
/// as `include-1` and `include-1/nested`; the map treats IDs as opaque.
/// Origin-bearing segments for one occurrence must refer to one source file;
/// discontiguous ranges within that source are allowed around nested
/// occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExpansionId(String);

impl ExpansionId {
    /// Create an opaque expansion identity.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Identity used for bytes belonging to the root prepared source rather
    /// than an included occurrence.
    pub fn root() -> Self {
        Self::new("root")
    }

    /// Borrow the identity text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ExpansionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ExpansionId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// The byte-level relationship between a prepared range and its origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceSegmentKind {
    /// Prepared bytes are exactly copied from the original source range.
    Copied,
    /// Prepared bytes mask original content with whitespace while preserving
    /// byte length and line breaks.
    Masked,
    /// Prepared bytes were introduced by the caller and have no source
    /// origin.
    Synthetic,
}

/// One ordered segment in a validated prepared-source map.
///
/// Segments cover a prepared buffer, not an original file.  An original range
/// may therefore be reused by several segments when an include is repeated;
/// the [`ExpansionId`] distinguishes those occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMapSegment {
    /// Half-open byte range in the prepared buffer.
    pub prepared_range: Range<usize>,
    /// Original origin for copied or masked bytes.  Synthetic segments must
    /// leave this as `None` rather than inventing an origin.
    pub original: Option<SourceSpan>,
    /// Caller-assigned root/include occurrence identity.
    pub expansion_id: ExpansionId,
    /// Relationship between prepared bytes and the original.
    pub kind: SourceSegmentKind,
}

impl SourceMapSegment {
    /// Construct a copied segment.
    pub fn copied(
        prepared_range: Range<usize>,
        source_id: ProjectSourceId,
        original_range: Range<usize>,
        expansion_id: ExpansionId,
    ) -> Self {
        Self {
            prepared_range,
            original: Some(SourceSpan::new(source_id, original_range)),
            expansion_id,
            kind: SourceSegmentKind::Copied,
        }
    }

    /// Construct a whitespace-masked segment.
    pub fn masked(
        prepared_range: Range<usize>,
        source_id: ProjectSourceId,
        original_range: Range<usize>,
        expansion_id: ExpansionId,
    ) -> Self {
        Self {
            prepared_range,
            original: Some(SourceSpan::new(source_id, original_range)),
            expansion_id,
            kind: SourceSegmentKind::Masked,
        }
    }

    /// Construct a caller-introduced segment with no false source origin.
    pub fn synthetic(prepared_range: Range<usize>, expansion_id: ExpansionId) -> Self {
        Self {
            prepared_range,
            original: None,
            expansion_id,
            kind: SourceSegmentKind::Synthetic,
        }
    }

    /// Borrow the prepared range.
    pub fn prepared_range(&self) -> Range<usize> {
        self.prepared_range.clone()
    }

    /// Borrow the optional original span.
    pub fn original(&self) -> Option<&SourceSpan> {
        self.original.as_ref()
    }

    /// Borrow the expansion occurrence identity.
    pub fn expansion_id(&self) -> &ExpansionId {
        &self.expansion_id
    }

    /// Return the segment kind.
    pub fn kind(&self) -> SourceSegmentKind {
        self.kind
    }
}

/// A mapped portion of a query range.
///
/// A query crossing an include boundary returns multiple values.  The
/// `prepared_range` is clipped to the query, while a copied or masked
/// `original` range is clipped by the same byte offset.  Synthetic portions
/// are represented explicitly with `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedSourceSpan {
    /// Portion of the requested range in the prepared buffer.
    pub prepared_range: Range<usize>,
    /// Corresponding original span, or `None` for synthetic bytes.
    pub original: Option<SourceSpan>,
    /// Expansion occurrence that produced this portion.
    pub expansion_id: ExpansionId,
    /// Segment relationship.
    pub kind: SourceSegmentKind,
}

impl MappedSourceSpan {
    /// Borrow the optional original span.
    pub fn original(&self) -> Option<&SourceSpan> {
        self.original.as_ref()
    }

    /// Borrow the expansion occurrence identity.
    pub fn expansion_id(&self) -> &ExpansionId {
        &self.expansion_id
    }

    /// Return the segment kind.
    pub fn kind(&self) -> SourceSegmentKind {
        self.kind
    }
}

/// Validation failures for a prepared-source map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMapError {
    /// An origin source ID was empty.
    EmptySourceId(ProjectSourceId),
    /// The same origin ID was supplied more than once with equal bytes.
    DuplicateSourceId(ProjectSourceId),
    /// The same origin ID was supplied with conflicting bytes.
    ConflictingSourceId(ProjectSourceId),
    /// A segment referred to an origin that was not supplied.
    UnknownSourceId(ProjectSourceId),
    /// An expansion identity was empty.
    EmptyExpansionId,
    /// A segment had a reversed range.
    InvalidRange {
        /// Range that was not a valid half-open range.
        range: Range<usize>,
        /// Whether the range was prepared or original.
        space: &'static str,
    },
    /// A segment had no bytes.  Empty maps use no segments instead.
    EmptySegment { range: Range<usize> },
    /// A prepared segment extended past the prepared buffer.
    PreparedRangeOutOfBounds {
        range: Range<usize>,
        prepared_len: usize,
    },
    /// An original span extended past its source snapshot.
    OriginalRangeOutOfBounds {
        source_id: ProjectSourceId,
        range: Range<usize>,
        source_len: usize,
    },
    /// Segments did not cover the prepared buffer contiguously.
    IncompleteCoverage {
        expected_start: usize,
        actual_start: usize,
        prepared_len: usize,
    },
    /// Two segments overlapped in prepared coordinates.
    OverlappingSegments {
        previous: Range<usize>,
        next: Range<usize>,
    },
    /// Two origin-bearing segments reused an original range within one
    /// source/expansion occurrence.
    OverlappingOriginalRanges {
        source_id: ProjectSourceId,
        expansion_id: ExpansionId,
        previous: Range<usize>,
        next: Range<usize>,
    },
    /// One expansion occurrence claimed origins in more than one source.
    ExpansionSourceMismatch {
        expansion_id: ExpansionId,
        expected: ProjectSourceId,
        found: ProjectSourceId,
    },
    /// A copied or masked segment did not carry an original span.
    MissingOrigin {
        prepared_range: Range<usize>,
        kind: SourceSegmentKind,
    },
    /// A synthetic segment incorrectly claimed an origin.
    SyntheticHasOrigin { prepared_range: Range<usize> },
    /// Prepared and original ranges had different byte lengths.
    LengthMismatch {
        prepared_range: Range<usize>,
        original_range: Range<usize>,
    },
    /// A copied segment's bytes differed from its declared origin.
    CopiedBytesMismatch {
        source_id: ProjectSourceId,
        prepared_range: Range<usize>,
        original_range: Range<usize>,
    },
    /// A masked segment contained a non-ASCII-whitespace byte.
    MaskedBytesNotWhitespace {
        prepared_range: Range<usize>,
        offset: usize,
        byte: u8,
    },
    /// A masked segment changed or removed a line-break byte.
    MaskedLineBreakMismatch {
        prepared_range: Range<usize>,
        original_range: Range<usize>,
        offset: usize,
    },
    /// A requested mapping range was outside the prepared buffer.
    MapRangeOutOfBounds {
        range: Range<usize>,
        prepared_len: usize,
    },
}

impl fmt::Display for SourceMapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySourceId(id) => write!(formatter, "source map source ID {:?} is empty", id),
            Self::DuplicateSourceId(id) => {
                write!(
                    formatter,
                    "source map source ID {:?} was supplied twice",
                    id
                )
            }
            Self::ConflictingSourceId(id) => write!(
                formatter,
                "source map source ID {:?} was supplied with conflicting bytes",
                id
            ),
            Self::UnknownSourceId(id) => {
                write!(formatter, "source map source ID {:?} was not supplied", id)
            }
            Self::EmptyExpansionId => {
                formatter.write_str("source map expansion ID must not be empty")
            }
            Self::InvalidRange { range, space } => {
                write!(formatter, "invalid {space} range {range:?}")
            }
            Self::EmptySegment { range } => {
                write!(formatter, "source map segment {range:?} must not be empty")
            }
            Self::PreparedRangeOutOfBounds {
                range,
                prepared_len,
            } => write!(
                formatter,
                "prepared range {range:?} exceeds prepared length {prepared_len}"
            ),
            Self::OriginalRangeOutOfBounds {
                source_id,
                range,
                source_len,
            } => write!(
                formatter,
                "original range {range:?} for source {:?} exceeds source length {}",
                source_id, source_len
            ),
            Self::IncompleteCoverage {
                expected_start,
                actual_start,
                prepared_len,
            } => write!(
                formatter,
                "source map coverage expected byte {}, found {}; prepared length {}",
                expected_start, actual_start, prepared_len
            ),
            Self::OverlappingSegments { previous, next } => write!(
                formatter,
                "source map ranges {:?} and {:?} overlap",
                previous, next
            ),
            Self::OverlappingOriginalRanges {
                source_id,
                expansion_id,
                previous,
                next,
            } => write!(
                formatter,
                "source {:?} ranges {:?} and {:?} overlap within expansion {:?}",
                source_id, previous, next, expansion_id
            ),
            Self::ExpansionSourceMismatch {
                expansion_id,
                expected,
                found,
            } => write!(
                formatter,
                "expansion {:?} maps to source {:?} and source {:?}",
                expansion_id, expected, found
            ),
            Self::MissingOrigin {
                prepared_range,
                kind,
            } => write!(
                formatter,
                "{kind:?} segment {prepared_range:?} is missing its original origin"
            ),
            Self::SyntheticHasOrigin { prepared_range } => write!(
                formatter,
                "synthetic segment {prepared_range:?} must not claim an original origin"
            ),
            Self::LengthMismatch {
                prepared_range,
                original_range,
            } => write!(
                formatter,
                "prepared range {:?} and original range {:?} have different lengths",
                prepared_range, original_range
            ),
            Self::CopiedBytesMismatch {
                source_id,
                prepared_range,
                original_range,
            } => write!(
                formatter,
                "copied prepared range {:?} differs from source {:?} range {:?}",
                prepared_range, source_id, original_range
            ),
            Self::MaskedBytesNotWhitespace {
                prepared_range,
                offset,
                byte,
            } => write!(
                formatter,
                "masked range {:?} contains byte 0x{:02x} at offset {}",
                prepared_range, byte, offset
            ),
            Self::MaskedLineBreakMismatch {
                prepared_range,
                original_range,
                offset,
            } => write!(
                formatter,
                "masked range {:?} does not preserve the line break at offset {} from {:?}",
                prepared_range, offset, original_range
            ),
            Self::MapRangeOutOfBounds {
                range,
                prepared_len,
            } => write!(
                formatter,
                "mapping range {range:?} exceeds prepared length {prepared_len}"
            ),
        }
    }
}

impl std::error::Error for SourceMapError {}

/// A validated, immutable, ordered map for one prepared buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMap {
    prepared_bytes: Arc<[u8]>,
    original_sources: Vec<SourceSnapshot>,
    segments: Vec<SourceMapSegment>,
    prepared_len: usize,
}

impl SourceMap {
    /// Validate a complete mapping for `prepared_bytes`.
    ///
    /// Validation checks that segments form ordered full coverage, all source
    /// identities are unique and present, copied/masked ranges have equal
    /// lengths, copied bytes match exactly, and masked bytes contain only
    /// whitespace while preserving line breaks.  Origin ranges are non-
    /// overlapping within each source/expansion pair, and one expansion
    /// occurrence cannot claim more than one source.  Synthetic bytes are
    /// valid only when their lack of origin is explicit in the segment.
    pub fn new(
        prepared_bytes: impl AsRef<[u8]>,
        original_sources: Vec<SourceSnapshot>,
        segments: Vec<SourceMapSegment>,
    ) -> Result<Self, SourceMapError> {
        let prepared_bytes: Arc<[u8]> = Arc::from(prepared_bytes.as_ref());
        let prepared_len = prepared_bytes.len();

        let mut sources = HashMap::with_capacity(original_sources.len());
        for (index, source) in original_sources.iter().enumerate() {
            if source.source_id.as_str().is_empty() {
                return Err(SourceMapError::EmptySourceId(source.source_id.clone()));
            }
            if let Some(previous) = sources.insert(source.source_id.clone(), index) {
                let previous_source = &original_sources[previous];
                if previous_source.bytes == source.bytes {
                    return Err(SourceMapError::DuplicateSourceId(source.source_id.clone()));
                }
                return Err(SourceMapError::ConflictingSourceId(
                    source.source_id.clone(),
                ));
            }
        }

        let mut expected_start = 0;
        let mut previous_range = None;
        let mut expansion_sources: HashMap<ExpansionId, ProjectSourceId> = HashMap::new();
        let mut original_ranges: HashMap<(ProjectSourceId, ExpansionId), Vec<Range<usize>>> =
            HashMap::new();
        for segment in &segments {
            if segment.expansion_id.as_str().is_empty() {
                return Err(SourceMapError::EmptyExpansionId);
            }
            let prepared_range = &segment.prepared_range;
            if prepared_range.start > prepared_range.end {
                return Err(SourceMapError::InvalidRange {
                    range: prepared_range.clone(),
                    space: "prepared",
                });
            }
            if prepared_range.end > prepared_len {
                return Err(SourceMapError::PreparedRangeOutOfBounds {
                    range: prepared_range.clone(),
                    prepared_len,
                });
            }
            if prepared_range.start < expected_start {
                return Err(SourceMapError::OverlappingSegments {
                    previous: previous_range.clone().unwrap_or(0..expected_start),
                    next: prepared_range.clone(),
                });
            }
            if prepared_range.start > expected_start {
                return Err(SourceMapError::IncompleteCoverage {
                    expected_start,
                    actual_start: prepared_range.start,
                    prepared_len,
                });
            }
            if prepared_range.start == prepared_range.end {
                return Err(SourceMapError::EmptySegment {
                    range: prepared_range.clone(),
                });
            }
            expected_start = prepared_range.end;
            previous_range = Some(prepared_range.clone());

            match (segment.kind, segment.original.as_ref()) {
                (SourceSegmentKind::Synthetic, Some(_)) => {
                    return Err(SourceMapError::SyntheticHasOrigin {
                        prepared_range: prepared_range.clone(),
                    });
                }
                (SourceSegmentKind::Synthetic, None) => {}
                (kind, None) => {
                    return Err(SourceMapError::MissingOrigin {
                        prepared_range: prepared_range.clone(),
                        kind,
                    });
                }
                (kind, Some(original)) => {
                    let Some(&source_index) = sources.get(&original.source_id) else {
                        return Err(SourceMapError::UnknownSourceId(original.source_id.clone()));
                    };
                    if let Some(expected_source_id) = expansion_sources.get(&segment.expansion_id) {
                        if expected_source_id != &original.source_id {
                            return Err(SourceMapError::ExpansionSourceMismatch {
                                expansion_id: segment.expansion_id.clone(),
                                expected: expected_source_id.clone(),
                                found: original.source_id.clone(),
                            });
                        }
                    } else {
                        expansion_sources
                            .insert(segment.expansion_id.clone(), original.source_id.clone());
                    }
                    let source = &original_sources[source_index];
                    let original_range = &original.byte_range;
                    if original_range.start > original_range.end {
                        return Err(SourceMapError::InvalidRange {
                            range: original_range.clone(),
                            space: "original",
                        });
                    }
                    if original_range.end > source.len() {
                        return Err(SourceMapError::OriginalRangeOutOfBounds {
                            source_id: original.source_id.clone(),
                            range: original_range.clone(),
                            source_len: source.len(),
                        });
                    }
                    if prepared_range.len() != original_range.len() {
                        return Err(SourceMapError::LengthMismatch {
                            prepared_range: prepared_range.clone(),
                            original_range: original_range.clone(),
                        });
                    }
                    original_ranges
                        .entry((original.source_id.clone(), segment.expansion_id.clone()))
                        .or_default()
                        .push(original_range.clone());

                    match kind {
                        SourceSegmentKind::Copied => {
                            if prepared_bytes.as_ref()[prepared_range.clone()]
                                != source.bytes[original_range.clone()]
                            {
                                return Err(SourceMapError::CopiedBytesMismatch {
                                    source_id: original.source_id.clone(),
                                    prepared_range: prepared_range.clone(),
                                    original_range: original_range.clone(),
                                });
                            }
                        }
                        SourceSegmentKind::Masked => {
                            validate_mask(
                                prepared_bytes.as_ref(),
                                source.bytes(),
                                prepared_range,
                                original_range,
                            )?;
                        }
                        SourceSegmentKind::Synthetic => unreachable!(
                            "synthetic segments with origins are rejected before validation"
                        ),
                    }
                }
            }
        }

        if expected_start != prepared_len {
            return Err(SourceMapError::IncompleteCoverage {
                expected_start,
                actual_start: expected_start,
                prepared_len,
            });
        }

        // Keep the public segment order intact.  Original ranges are grouped
        // and sorted only for this validation pass, so discontiguous segments
        // around nested occurrences and source-order reversals remain valid.
        for ((source_id, expansion_id), ranges) in &mut original_ranges {
            ranges.sort_unstable_by_key(|range| (range.start, range.end));
            for pair in ranges.windows(2) {
                if ranges_overlap(&pair[0], &pair[1]) {
                    return Err(SourceMapError::OverlappingOriginalRanges {
                        source_id: source_id.clone(),
                        expansion_id: expansion_id.clone(),
                        previous: pair[0].clone(),
                        next: pair[1].clone(),
                    });
                }
            }
        }

        Ok(Self {
            prepared_bytes,
            original_sources,
            segments,
            prepared_len,
        })
    }

    /// Construct an identity map for one source revision.
    pub fn identity(source: SourceSnapshot) -> Result<Self, SourceMapError> {
        let source_id = source.source_id.clone();
        let bytes = source.bytes.to_vec();
        let len = bytes.len();
        let segments = (len > 0).then(|| {
            vec![SourceMapSegment::copied(
                0..len,
                source_id,
                0..len,
                ExpansionId::root(),
            )]
        });
        Self::new(bytes, vec![source], segments.unwrap_or_default())
    }

    /// Construct an identity map directly from an ID and bytes.
    pub fn identity_for(
        source_id: ProjectSourceId,
        bytes: impl AsRef<[u8]>,
    ) -> Result<Self, SourceMapError> {
        Self::identity(SourceSnapshot::new(source_id, bytes))
    }

    /// Borrow all original source snapshots retained by this map.
    pub fn original_sources(&self) -> &[SourceSnapshot] {
        &self.original_sources
    }

    /// Borrow the exact prepared bytes against which this map was validated.
    pub fn prepared_bytes(&self) -> &[u8] {
        &self.prepared_bytes
    }

    /// Borrow the validated ordered segments.
    pub fn segments(&self) -> &[SourceMapSegment] {
        &self.segments
    }

    /// Length of the prepared buffer covered by this map.
    pub fn prepared_len(&self) -> usize {
        self.prepared_len
    }

    pub(crate) fn prepared_bytes_arc(&self) -> Arc<[u8]> {
        Arc::clone(&self.prepared_bytes)
    }

    /// Map a prepared byte range to every corresponding original span.
    ///
    /// Empty query ranges return an empty vector.  Non-empty ranges are
    /// clipped at segment boundaries, so crossing files or include
    /// occurrences always returns one result per contributing segment.
    pub fn map_range(
        &self,
        prepared_range: Range<usize>,
    ) -> Result<Vec<MappedSourceSpan>, SourceMapError> {
        if prepared_range.start > prepared_range.end {
            return Err(SourceMapError::InvalidRange {
                range: prepared_range,
                space: "mapping",
            });
        }
        if prepared_range.end > self.prepared_len {
            return Err(SourceMapError::MapRangeOutOfBounds {
                range: prepared_range,
                prepared_len: self.prepared_len,
            });
        }
        if prepared_range.is_empty() {
            return Ok(Vec::new());
        }

        let mut mapped = Vec::new();
        for segment in &self.segments {
            if segment.prepared_range.end <= prepared_range.start {
                continue;
            }
            if segment.prepared_range.start >= prepared_range.end {
                break;
            }

            let start = segment.prepared_range.start.max(prepared_range.start);
            let end = segment.prepared_range.end.min(prepared_range.end);
            let original = segment.original.as_ref().map(|original| {
                let offset = start - segment.prepared_range.start;
                SourceSpan::new(
                    original.source_id.clone(),
                    (original.byte_range.start + offset)
                        ..(original.byte_range.start + offset + (end - start)),
                )
            });
            mapped.push(MappedSourceSpan {
                prepared_range: start..end,
                original,
                expansion_id: segment.expansion_id.clone(),
                kind: segment.kind,
            });
        }
        Ok(mapped)
    }
}

/// Short alias for callers that prefer the segment name without the map
/// prefix.
pub type SourceSegment = SourceMapSegment;

/// Short alias for mapped query results.
pub type MappedSpan = MappedSourceSpan;

fn validate_mask(
    prepared: &[u8],
    original: &[u8],
    prepared_range: &Range<usize>,
    original_range: &Range<usize>,
) -> Result<(), SourceMapError> {
    for (offset, (&prepared_byte, &original_byte)) in prepared[prepared_range.clone()]
        .iter()
        .zip(original[original_range.clone()].iter())
        .enumerate()
    {
        if !prepared_byte.is_ascii_whitespace() {
            return Err(SourceMapError::MaskedBytesNotWhitespace {
                prepared_range: prepared_range.clone(),
                offset,
                byte: prepared_byte,
            });
        }

        let prepared_break = matches!(prepared_byte, b'\r' | b'\n');
        let original_break = matches!(original_byte, b'\r' | b'\n');
        if prepared_break != original_break || (prepared_break && prepared_byte != original_byte) {
            return Err(SourceMapError::MaskedLineBreakMismatch {
                prepared_range: prepared_range.clone(),
                original_range: original_range.clone(),
                offset,
            });
        }
    }
    Ok(())
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}
