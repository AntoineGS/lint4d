//! Bounded decoding of member record parts.

use super::{
    Binding, BindingKind, BindingOutcome, CoverageStatus, IncompleteReason,
    MAX_AMBIGUOUS_CANDIDATES, MemberCoverage, MemberHeader, SnapshotCoverage, binding_error,
};
use crate::SourceIdentity;
use crate::decode::{Decoded, Reader};
use pascal_project::SourceOrigin;

/// The bytes are not one well-formed record part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordDecodeError(pub &'static str);

fn reason(reader: &mut Reader<'_>) -> Decoded<IncompleteReason> {
    IncompleteReason::from_tag(reader.u8()?).ok_or("unknown reason")
}

fn status(reader: &mut Reader<'_>) -> Decoded<CoverageStatus> {
    match reader.u8()? {
        1 => Ok(CoverageStatus::Complete),
        2 => Ok(CoverageStatus::Incomplete(reason(reader)?)),
        _ => Err("unknown coverage status"),
    }
}

fn identity(reader: &mut Reader<'_>) -> Decoded<SourceIdentity> {
    reader.field(1)?;
    let canonical = reader.path()?;
    reader.field(2)?;
    let logical = reader.path()?;
    reader.field(3)?;
    let spelling = reader.optional(Reader::str)?;
    reader.field(4)?;
    let uri = reader.optional(Reader::str)?;
    Ok(SourceIdentity {
        canonical,
        logical,
        spelling,
        uri,
    })
}

fn header(bytes: &[u8]) -> Decoded<MemberHeader> {
    let mut reader = Reader::new(bytes);
    reader.field(1)?;
    let source = identity(&mut reader)?;
    reader.field(2)?;
    let artifact = reader.optional(Reader::digest)?;
    reader.field(3)?;
    let member_evidence = reader.optional(Reader::digest)?;
    reader.field(4)?;
    let origin = match reader.u8()? {
        1 => SourceOrigin::ProjectCompiled,
        2 => SourceOrigin::Library,
        _ => return Err("unknown origin"),
    };
    reader.field(5)?;
    let fallback_unit_name = reader.optional(Reader::str)?;
    reader.field(6)?;
    reader.field(1)?;
    let extraction = status(&mut reader)?;
    reader.field(2)?;
    let resolution = status(&mut reader)?;
    reader.finish()?;
    if member_evidence.is_some() && artifact.is_none() {
        return Err("member evidence without an artifact");
    }
    Ok(MemberHeader {
        source,
        artifact,
        member_evidence,
        origin,
        fallback_unit_name,
        coverage: MemberCoverage {
            extraction,
            resolution,
        },
    })
}

fn binding(bytes: &[u8]) -> Decoded<Binding> {
    let mut reader = Reader::new(bytes);
    reader.field(1)?;
    let kind = BindingKind::from_tag(reader.u8()?).ok_or("unknown binding kind")?;
    reader.field(2)?;
    let occurrence = reader.u64()?;
    reader.field(3)?;
    let outcome = match reader.u8()? {
        1 => BindingOutcome::Precise {
            target: identity(&mut reader)?,
        },
        2 => {
            let count = reader.length()?;
            if count > MAX_AMBIGUOUS_CANDIDATES {
                return Err("too many candidates");
            }
            let mut candidates = Vec::with_capacity(count);
            for _ in 0..count {
                candidates.push(identity(&mut reader)?);
            }
            BindingOutcome::Ambiguous {
                candidates,
                overflow: reader.bool()?,
            }
        }
        3 => BindingOutcome::Unavailable,
        4 => BindingOutcome::Incomplete(reason(&mut reader)?),
        _ => return Err("unknown binding outcome"),
    };
    reader.field(4)?;
    let evidence = reader.optional(Reader::digest)?;
    reader.finish()?;
    let binding = Binding {
        kind,
        occurrence,
        outcome,
        evidence,
    };
    match binding_error(&binding, 0) {
        Some(_) => Err("binding breaks an invariant"),
        None => Ok(binding),
    }
}

fn snapshot_coverage(bytes: &[u8]) -> Decoded<SnapshotCoverage> {
    let mut reader = Reader::new(bytes);
    reader.field(1)?;
    let discovery = status(&mut reader)?;
    reader.field(2)?;
    let membership = status(&mut reader)?;
    reader.finish()?;
    Ok(SnapshotCoverage {
        discovery,
        membership,
    })
}

pub fn decode_member_header(bytes: &[u8]) -> Result<MemberHeader, RecordDecodeError> {
    header(bytes).map_err(RecordDecodeError)
}

pub fn decode_binding(bytes: &[u8]) -> Result<Binding, RecordDecodeError> {
    binding(bytes).map_err(RecordDecodeError)
}

pub fn decode_snapshot_coverage(bytes: &[u8]) -> Result<SnapshotCoverage, RecordDecodeError> {
    snapshot_coverage(bytes).map_err(RecordDecodeError)
}
