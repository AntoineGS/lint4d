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
    /// Feature word of the 0xFE header record.
    pub flags: u32,
    /// File size recorded in the header (equal to the input length).
    pub stamp: u32,
    /// Byte offset where the unit body begins.
    pub body_offset: usize,
}

/// Parse the unit header from a raw DCU byte slice.
///
/// Layout (D2010+), as read by dcc's `FUN_00806170` (`dcc_UNITRDWR.c`):
/// ```text
/// [0..4]   magic          (u32 LE)
/// [4..8]   file_size      (u32 LE, must equal the input length)
/// [8..16]  two dwords     (stored by the compiler, unused here)
/// [16]     name           (read_name, empty in every observed unit)
/// [+]      unit index     (uindex, 1)
/// [+]      unit name      (read_name: the namespace prefix, e.g. `Lint4dFixture`)
/// [+]      records        0xFE feature word (uindex)
///                         0x96 unit header (three uindex)
/// [+]      drSrc tag      (0x70)
/// [+]      source_file    (read_name, includes ".pas")
/// ```
///
/// The unit name is derived from the source filename by stripping the `.pas` suffix.
pub fn parse_unit_header(data: &[u8]) -> Result<UnitHeader, DcuError> {
    let (version, platform) = parse_magic(data)?;
    let mut reader = DcuReader::new(data, version);

    parse_unit_header_with_reader(version, platform, &mut reader, true)
}

/// [`parse_unit_header`] without the size check, for the tolerant lint
/// parser that keeps the declarations of a truncated unit.
pub(crate) fn parse_unit_header_tolerant(data: &[u8]) -> Result<UnitHeader, DcuError> {
    let (version, platform) = parse_magic(data)?;
    let mut reader = DcuReader::new(data, version);

    parse_unit_header_with_reader(version, platform, &mut reader, false)
}

pub(crate) fn parse_unit_header_for_provider(
    data: &[u8],
    max_records: usize,
    max_decoded_bytes: usize,
) -> Result<(UnitHeader, DcuReader<'_>), DcuError> {
    let (version, platform) = parse_magic(data)?;
    let mut reader = DcuReader::new_for_provider(data, version, max_records, max_decoded_bytes);
    let header = parse_unit_header_with_reader(version, platform, &mut reader, true)?;
    Ok((header, reader))
}

fn parse_unit_header_with_reader(
    version: DcuVersion,
    platform: DcuPlatform,
    reader: &mut DcuReader<'_>,
    check_size: bool,
) -> Result<UnitHeader, DcuError> {
    // Skip magic (4 bytes already parsed by parse_magic).
    reader.skip(4)?;

    // dcc rejects a unit whose size field differs from the bytes it was given.
    let stamp = reader.read_u32()?;
    if check_size && stamp as usize != reader.len() {
        return Err(DcuError::SizeMismatch {
            declared: stamp,
            actual: reader.len(),
        });
    }

    reader.skip(8)?;
    reader.charge_decoded_record()?;
    let _name = reader.read_name()?;
    let _unit_index = reader.read_uindex()?;
    reader.charge_decoded_record()?;
    let _namespace = reader.read_name()?;

    // Tagged records up to drSrc (0x70), which holds the source filename.
    let mut flags = 0;
    loop {
        let offset = reader.position();
        match reader.read_byte()? {
            0xFE => flags = reader.read_uindex()?,
            0x96 => {
                for _ in 0..3 {
                    reader.read_uindex()?;
                }
            }
            0x70 => break,
            tag => return Err(DcuError::UnknownTag { tag, offset }),
        }
    }

    // Read the source filename (e.g. "Lint4dFixture.Classes.pas").
    reader.charge_decoded_record()?;
    let source_file = reader.read_name()?;

    // Derive the unit name: strip directory prefix (D2010 stores full relative
    // paths like "..\src\Lint4dFixture.Classes.pas") and the ".pas" extension.
    let filename = source_file
        .rsplit_once('\\')
        .map_or(source_file.as_str(), |(_, f)| f);
    let name = filename.strip_suffix(".pas").unwrap_or(filename).to_owned();

    Ok(UnitHeader {
        version,
        platform,
        name,
        flags,
        stamp,
        body_offset: reader.position(),
    })
}
