//! Bounded decoding of one stored observation. Every length is checked
//! against what remains before anything is taken.

use super::{LISTING, Observation, PAYLOAD, PRESENCE};
use crate::{Digest, SourceContent};
use pascal_core::SourceEncoding;
use pascal_project::{ProjectPathEntry, ProjectPathProvenance};
use std::path::PathBuf;

/// The bytes are not one well-formed observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationError(pub &'static str);

type Decoded<T> = Result<T, ObservationError>;

fn malformed<T>(reason: &'static str) -> Decoded<T> {
    Err(ObservationError(reason))
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Decoded<&'a [u8]> {
        if count > self.0.len() {
            return malformed("truncated observation");
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Decoded<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Decoded<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }

    fn u64(&mut self) -> Decoded<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn length(&mut self) -> Decoded<usize> {
        usize::try_from(self.u64()?).or_else(|_| malformed("length overflows"))
    }

    fn field(&mut self, tag: u16) -> Decoded<()> {
        if self.u16()? == tag {
            Ok(())
        } else {
            malformed("unexpected field")
        }
    }

    fn bool(&mut self) -> Decoded<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => malformed("invalid bool"),
        }
    }

    fn digest(&mut self) -> Decoded<Digest> {
        Ok(Digest::from_bytes(
            self.take(32)?.try_into().expect("32 bytes"),
        ))
    }

    #[cfg(unix)]
    fn path(&mut self) -> Decoded<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        if self.u8()? != 1 {
            return malformed("foreign path encoding");
        }
        let length = self.length()?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
            self.take(length)?,
        )))
    }

    #[cfg(windows)]
    fn path(&mut self) -> Decoded<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        if self.u8()? != 2 {
            return malformed("foreign path encoding");
        }
        let units = self.length()?;
        let bytes = self.take(
            units
                .checked_mul(2)
                .ok_or(ObservationError("length overflows"))?,
        )?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&units)))
    }

    fn entry(&mut self) -> Decoded<ProjectPathEntry> {
        self.field(1)?;
        let path = self.path()?;
        self.field(2)?;
        let provenance = match self.u8()? {
            1 => ProjectPathProvenance::LegacyNative,
            2 => ProjectPathProvenance::Configured,
            3 => ProjectPathProvenance::Mapped { root: self.path()? },
            _ => return malformed("unknown provenance"),
        };
        Ok(ProjectPathEntry { path, provenance })
    }

    fn content(&mut self) -> Decoded<SourceContent> {
        self.field(1)?;
        let digest = self.digest()?;
        self.field(2)?;
        let length = self.u64()?;
        self.field(3)?;
        let encoding = match self.u8()? {
            1 => SourceEncoding::Utf8Bom,
            2 => SourceEncoding::Utf8,
            3 => SourceEncoding::Latin1,
            _ => return malformed("unknown source encoding"),
        };
        Ok(SourceContent::from_parts(digest, length, encoding))
    }
}

pub fn decode_observation(bytes: &[u8]) -> Result<Observation, ObservationError> {
    let mut reader = Reader(bytes);
    let observation = match reader.u8()? {
        PAYLOAD => {
            reader.field(1)?;
            let path = reader.path()?;
            reader.field(2)?;
            let entry = reader.entry()?;
            reader.field(3)?;
            let content = reader.content()?;
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
            let entry = if reader.bool()? {
                Some(reader.entry()?)
            } else {
                None
            };
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
            let entry = reader.entry()?;
            reader.field(3)?;
            let listing = reader.digest()?;
            Observation::Listing {
                path,
                entry,
                listing,
            }
        }
        _ => return malformed("unknown observation kind"),
    };
    if !reader.0.is_empty() {
        return malformed("trailing observation bytes");
    }
    Ok(observation)
}
