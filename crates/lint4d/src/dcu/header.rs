use crate::dcu::reader::DcuReader;
use crate::dcu::tags::DcuError;
use crate::dcu::{DcuPlatform, DcuVersion};

use DcuPlatform::{Win32, Win64};
use DcuVersion::*;

/// Version signatures: (version byte, version, signature byte, code in bits
/// 8-12, Win64 compiler exists).
static VERSIONS: &[(u8, DcuVersion, u8, u8, bool)] = &[
    (0x15, D2010, 0x45, 0x00, false),
    (0x16, DXE, 0x4B, 0x03, false),
    (0x17, DXE2, 0x4B, 0x03, true),
    (0x18, DXE3, 0x4B, 0x03, true),
    (0x19, DXE4, 0x4B, 0x03, true),
    (0x1A, DXE5, 0x4B, 0x03, true),
    (0x1B, DXE6, 0x4D, 0x03, true),
    (0x1C, DXE7, 0x4D, 0x03, true),
    (0x1D, DXE8, 0x4D, 0x03, true),
    (0x1E, D10S, 0x4D, 0x03, true),
    (0x1F, D101B, 0x4D, 0x03, true),
    (0x20, D102T, 0x4D, 0x03, true),
    (0x21, D103R, 0x4D, 0x03, true),
    (0x22, D104S, 0x4D, 0x03, true),
    (0x23, D11A, 0x4D, 0x03, true),
    (0x24, D12A, 0x4D, 0x03, true),
    (0x25, D13, 0x4D, 0x03, true),
];

/// Decode the magic as dcc 13's unit reader (`FUN_00806170`) does: it
/// accepts `(magic & 0xff801fff) == 0x2500004d | (k << 8)` with `k` 3, or 0
/// under one compiler option, so bits 16-22 are free and bits 13-15 are a
/// target code. Code 0 is Win32 and 1 is Win64; dcc gives codes 1 and 4
/// 8-byte pointers and sets another flag for 0 and 2. The other codes are
/// other targets this reader does not decode.
pub fn parse_magic(data: &[u8]) -> Result<(DcuVersion, DcuPlatform), DcuError> {
    let mut reader = DcuReader::new(data, DcuVersion::D13);
    let magic = reader.read_u32()?;
    let version_byte = (magic >> 24) as u8;
    let signature = magic as u8;
    let code = ((magic >> 8) & 0x1F) as u8;
    let target_code = ((magic >> 13) & 0x7) as u8;
    let Some(&(_, version, _, _, has_win64)) =
        VERSIONS
            .iter()
            .find(|&&(byte, version, expected, expected_code, _)| {
                byte == version_byte
                    && magic & 0x0080_0000 == 0
                    && signature == expected
                    && (code == expected_code || (version == D13 && code == 0))
            })
    else {
        return Err(DcuError::UnsupportedVersion { magic });
    };
    match target_code {
        0 => Ok((version, Win32)),
        1 if has_win64 => Ok((version, Win64)),
        _ => Err(DcuError::UnsupportedPlatform {
            magic,
            version,
            target_code,
        }),
    }
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
