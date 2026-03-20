use crate::dcu::reader::DcuReader;
use crate::dcu::tags::DcuError;
use crate::dcu::{DcuPlatform, DcuVersion};

const MAGIC_D13_WIN32: u32 = 0x2500_034D;
const MAGIC_D13_WIN64: u32 = 0x2500_234D;

pub fn parse_magic(data: &[u8]) -> Result<(DcuVersion, DcuPlatform), DcuError> {
    let mut reader = DcuReader::new(data);
    let magic = reader.read_u32()?;
    match magic {
        MAGIC_D13_WIN32 => Ok((DcuVersion::D13, DcuPlatform::Win32)),
        MAGIC_D13_WIN64 => Ok((DcuVersion::D13, DcuPlatform::Win64)),
        _ => Err(DcuError::UnsupportedVersion { magic }),
    }
}

/// Parsed DCU unit header containing identity and version information.
pub struct UnitHeader {
    pub version: DcuVersion,
    pub platform: DcuPlatform,
    /// The fully-qualified unit name (e.g. `CDAPI.Adapter.Data.Types`).
    pub name: String,
    /// Raw flags value from the header.
    pub flags: u32,
    /// Unit file stamp (changes when the unit is recompiled).
    pub stamp: u32,
    /// Byte offset where the unit body begins.
    pub body_offset: usize,
}

/// Parse the unit header from a raw DCU byte slice.
///
/// Layout (D13):
/// ```text
/// [0..4]   magic          (u32 LE)
/// [4..8]   stamp          (u32 LE)  — per-unit compilation counter
/// [8..12]  compiler_stamp (u32 LE)  — same for all units in one build
/// [12..16] project_stamp  (u32 LE)  — same for all units in one build
/// [16]     flags_lo       (raw byte)
/// [17]     flags_hi       (raw byte)
/// [18..]   namespace      (read_name: 1-byte length then ANSI bytes)
/// [+10]    <intermediate> (10 bytes skipped)
/// [+1+n]   source_file    (read_name: 1-byte length then ANSI bytes, includes ".pas")
/// ```
///
/// The unit name is derived from the source filename by stripping the trailing `.pas` suffix.
pub fn parse_unit_header(data: &[u8]) -> Result<UnitHeader, DcuError> {
    let (version, platform) = parse_magic(data)?;
    let mut reader = DcuReader::new(data);

    // Skip magic (4 bytes already parsed by parse_magic).
    reader.skip(4)?;

    // Read stamp (per-unit compilation counter).
    let stamp = reader.read_u32()?;

    // Skip compiler_stamp (4 bytes) + project_stamp (4 bytes) + flags_lo (1 byte) + flags_hi (1 byte).
    reader.skip(10)?;

    // Read the namespace name (e.g. "CDAPI.Adapter.Data" for unit "CDAPI.Adapter.Data.Types").
    // We don't use this value directly; we skip past it.
    let _namespace = reader.read_name()?;

    // Skip 10 bytes of intermediate fields between namespace and source filename.
    reader.skip(10)?;

    // Read the source filename (e.g. "CDAPI.Adapter.Data.Types.pas").
    let source_file = reader.read_name()?;

    // Derive the unit name by stripping the ".pas" extension.
    let name = if source_file.ends_with(".pas") {
        source_file[..source_file.len() - 4].to_owned()
    } else {
        source_file
    };

    Ok(UnitHeader {
        version,
        platform,
        name,
        flags: 0,
        stamp,
        body_offset: reader.position(),
    })
}
