use crate::dcu::reader::DcuReader;
use crate::dcu::tags::DcuError;
use crate::dcu::{DcuPlatform, DcuVersion};

use DcuPlatform::{Win32, Win64};
use DcuVersion::*;

/// Magic-number lookup table: (raw LE u32, version, platform).
/// 17 Win32 entries + 14 Win64 entries = 31 total (D2010 and XE have no Win64).
static MAGIC_TABLE: &[(u32, DcuVersion, DcuPlatform)] = &[
    // D2010
    (0x1500_0045, D2010, Win32),
    // XE
    (0x1600_034B, DXE, Win32),
    // XE2
    (0x1700_034B, DXE2, Win32),
    (0x1700_234B, DXE2, Win64),
    // XE3
    (0x1800_034B, DXE3, Win32),
    (0x1800_234B, DXE3, Win64),
    // XE4
    (0x1900_034B, DXE4, Win32),
    (0x1900_234B, DXE4, Win64),
    // XE5
    (0x1A00_034B, DXE5, Win32),
    (0x1A00_234B, DXE5, Win64),
    // XE6
    (0x1B00_034D, DXE6, Win32),
    (0x1B00_234D, DXE6, Win64),
    // XE7
    (0x1C00_034D, DXE7, Win32),
    (0x1C00_234D, DXE7, Win64),
    // XE8
    (0x1D00_034D, DXE8, Win32),
    (0x1D00_234D, DXE8, Win64),
    // 10 Seattle
    (0x1E00_034D, D10S, Win32),
    (0x1E00_234D, D10S, Win64),
    // 10.1 Berlin
    (0x1F00_034D, D101B, Win32),
    (0x1F00_234D, D101B, Win64),
    // 10.2 Tokyo
    (0x2000_034D, D102T, Win32),
    (0x2000_234D, D102T, Win64),
    // 10.3 Rio
    (0x2100_034D, D103R, Win32),
    (0x2100_234D, D103R, Win64),
    // 10.4 Sydney
    (0x2200_034D, D104S, Win32),
    (0x2200_234D, D104S, Win64),
    // 11 Alexandria
    (0x2300_034D, D11A, Win32),
    (0x2300_234D, D11A, Win64),
    // 12 Athens
    (0x2400_034D, D12A, Win32),
    (0x2400_234D, D12A, Win64),
    // 13
    (0x2500_034D, D13, Win32),
    (0x2500_234D, D13, Win64),
];

pub fn parse_magic(data: &[u8]) -> Result<(DcuVersion, DcuPlatform), DcuError> {
    let mut reader = DcuReader::new(data, DcuVersion::D13);
    let magic = reader.read_u32()?;
    for &(m, ver, plat) in MAGIC_TABLE {
        if m == magic {
            return Ok((ver, plat));
        }
    }
    Err(DcuError::UnsupportedVersion { magic })
}

/// Parsed DCU unit header containing identity and version information.
pub struct UnitHeader {
    pub version: DcuVersion,
    pub platform: DcuPlatform,
    /// The fully-qualified unit name (e.g. `Lint4dFixture.Classes`).
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
    let mut reader = DcuReader::new(data, DcuVersion::D13);

    // Skip magic (4 bytes already parsed by parse_magic).
    reader.skip(4)?;

    // Read stamp (per-unit compilation counter).
    let stamp = reader.read_u32()?;

    // Skip compiler_stamp (4 bytes) + project_stamp (4 bytes) + flags_lo (1 byte) + flags_hi (1 byte).
    reader.skip(10)?;

    // Read the namespace name (e.g. "Lint4dFixture" for unit "Lint4dFixture.Classes").
    // We don't use this value directly; we skip past it.
    let _namespace = reader.read_name()?;

    // Skip 10 bytes of intermediate fields between namespace and source filename.
    reader.skip(10)?;

    // Read the source filename (e.g. "Lint4dFixture.Classes.pas").
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
