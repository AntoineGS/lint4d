//! Bounded decoding of one stored observation.

use super::{LISTING, Observation, PAYLOAD, PRESENCE};
use crate::SourceContent;
use crate::decode::{Decoded, Reader};
use pascal_core::SourceEncoding;
use pascal_project::{ProjectPathEntry, ProjectPathProvenance};

/// The bytes are not one well-formed observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationError(pub &'static str);

fn entry(reader: &mut Reader<'_>) -> Decoded<ProjectPathEntry> {
    reader.field(1)?;
    let path = reader.path()?;
    reader.field(2)?;
    let provenance = match reader.u8()? {
        1 => ProjectPathProvenance::LegacyNative,
        2 => ProjectPathProvenance::Configured,
        3 => ProjectPathProvenance::Mapped {
            root: reader.path()?,
        },
        _ => return Err("unknown provenance"),
    };
    Ok(ProjectPathEntry { path, provenance })
}

fn content(reader: &mut Reader<'_>) -> Decoded<SourceContent> {
    reader.field(1)?;
    let digest = reader.digest()?;
    reader.field(2)?;
    let length = reader.u64()?;
    reader.field(3)?;
    let encoding = match reader.u8()? {
        1 => SourceEncoding::Utf8Bom,
        2 => SourceEncoding::Utf8,
        3 => SourceEncoding::Latin1,
        _ => return Err("unknown source encoding"),
    };
    Ok(SourceContent::from_parts(digest, length, encoding))
}

fn observation(bytes: &[u8]) -> Decoded<Observation> {
    let mut reader = Reader::new(bytes);
    let observation = match reader.u8()? {
        PAYLOAD => {
            reader.field(1)?;
            let path = reader.path()?;
            reader.field(2)?;
            let entry = entry(&mut reader)?;
            reader.field(3)?;
            let content = content(&mut reader)?;
            Observation::Payload {
                path,
                entry,
                content,
            }
        }
        PRESENCE => {
            reader.field(1)?;
            let path = reader.path()?;
            reader.field(2)?;
            let entry = reader.optional(entry)?;
            reader.field(3)?;
            let present = reader.bool()?;
            Observation::Presence {
                path,
                entry,
                present,
            }
        }
        LISTING => {
            reader.field(1)?;
            let path = reader.path()?;
            reader.field(2)?;
            let entry = entry(&mut reader)?;
            reader.field(3)?;
            let listing = reader.digest()?;
            Observation::Listing {
                path,
                entry,
                listing,
            }
        }
        _ => return Err("unknown observation kind"),
    };
    reader.finish()?;
    Ok(observation)
}

pub fn decode_observation(bytes: &[u8]) -> Result<Observation, ObservationError> {
    observation(bytes).map_err(ObservationError)
}
