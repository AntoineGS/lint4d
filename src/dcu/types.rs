use crate::dcu::header::parse_unit_header;
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;
use crate::dcu::DcuUnit;

/// Parse a complete DCU file, extracting the unit name, version, platform,
/// and the list of imported unit names (from interface and implementation uses clauses).
pub fn parse_dcu(data: &[u8]) -> Result<DcuUnit, DcuError> {
    let header = parse_unit_header(data)?;
    let mut reader = DcuReader::new(data);
    reader.set_position(header.body_offset);

    // After parse_unit_header, we are positioned right after the source filename
    // that was read as part of the header. In DCU32 terms, parse_unit_header consumed
    // ReadMagic + ReadUnitHeader + the first drSrc tag byte + ReadName for the source file.
    // What remains for the first source entry: file_time (u32) + file_index (ReadUIndex).
    // Then ReadTag for the next record.

    // Finish reading the first source file entry.
    let _file_time = reader.read_u32()?; // source file timestamp
    let _file_index = reader.read_uindex()?; // source file index

    // Read the next tag to continue with source files or uses clauses.
    let mut tag = reader.read_byte()?;

    // Skip remaining source file entries (drSrc, drObj, drRes, drAsm, drUnitInlineSrc).
    while is_source_file_tag(tag) {
        let _name = reader.read_name()?;
        let _ft = reader.read_u32()?;
        let _idx = reader.read_uindex()?;
        tag = reader.read_byte()?;
    }

    // Parse uses clauses: interface (drUnit), implementation (drUnit1), DLL (drDLL).
    let mut imported_units = Vec::new();

    read_uses(&mut reader, &mut tag, DR_UNIT, &mut imported_units)?;
    read_uses(&mut reader, &mut tag, DR_UNIT1, &mut imported_units)?;
    // DLL imports: skip them (don't add to imported_units).
    skip_uses(&mut reader, &mut tag, DR_DLL)?;

    Ok(DcuUnit {
        name: header.name,
        version: header.version,
        platform: header.platform,
        imported_units,
        types: Vec::new(),
    })
}

/// Returns true if the tag byte represents a source file record.
fn is_source_file_tag(tag: u8) -> bool {
    matches!(tag, DR_SRC | DR_OBJ | DR_RES | DR_ASM | DR_UNIT_INLINE_SRC)
}

/// Read a uses clause section. Each unit entry starts with the given `tag_rq` tag.
/// Extracts unit names into `imported_units`.
///
/// The `tag` parameter is updated to the tag read after the last uses entry
/// (i.e., the first tag that doesn't match `tag_rq`).
fn read_uses(
    reader: &mut DcuReader,
    tag: &mut u8,
    tag_rq: u8,
    imported_units: &mut Vec<String>,
) -> Result<(), DcuError> {
    while *tag == tag_rq {
        let unit_name = reader.read_name()?;
        imported_units.push(unit_name);

        // hPack (for D8+, which D13 is)
        let _h_pack = reader.read_uindex()?;
        // L (stamp/checksum — ReadUIndex for D2006+)
        let _l = reader.read_uindex()?;
        // L2 (for D2009+)
        let _l2 = reader.read_uindex()?;

        // Inner import loop: read imported types/values until we hit drStop1.
        skip_import_records(reader)?;

        // After drStop1, read the next tag.
        *tag = reader.read_byte()?;
    }
    Ok(())
}

/// Same as read_uses but discards unit names (used for DLL imports).
fn skip_uses(
    reader: &mut DcuReader,
    tag: &mut u8,
    tag_rq: u8,
) -> Result<(), DcuError> {
    while *tag == tag_rq {
        let _unit_name = reader.read_name()?;

        // DLL uses don't have hPack, but do have L.
        // For D2006+: L = ReadUIndex
        let _l = reader.read_uindex()?;
        // For D7 and D8+ DLL: L1 = ReadULong
        let _l1 = reader.read_u32()?;
        // For D2009+: L2 = ReadUIndex
        let _l2 = reader.read_uindex()?;

        skip_import_records(reader)?;

        *tag = reader.read_byte()?;
    }
    Ok(())
}

/// Skip the import records (drImpType, drImpTypeDef, drImpVal, drStop2, drConstAddInfo)
/// inside a uses clause entry until we encounter drStop1.
fn skip_import_records(reader: &mut DcuReader) -> Result<(), DcuError> {
    loop {
        let imp_tag = reader.read_byte()?;
        match imp_tag {
            DR_IMP_TYPE => {
                let _name = reader.read_name()?;
                let _l = reader.read_u32()?;
            }
            DR_IMP_TYPE_DEF => {
                let _name = reader.read_name()?;
                let _rtti_sz = reader.read_uindex()?;
                let _l = reader.read_u32()?;
            }
            DR_IMP_VAL => {
                let _name = reader.read_name()?;
                let _l = reader.read_u32()?;
            }
            DR_STOP2 => {
                // For D8+: read a u32
                let _l = reader.read_u32()?;
            }
            DR_CONST_ADD_INFO => {
                // For D11.3+, const add info appears in uses clauses.
                // We need to skip it. ReadConstAddInfo reads variable data.
                // For now, try to skip it by reading its content.
                skip_const_add_info(reader)?;
            }
            DR_STOP1 => {
                // End of imports for this uses entry.
                break;
            }
            _ => {
                // Unknown tag in import list — this could indicate a format
                // we don't fully understand yet. Treat as end marker.
                return Err(DcuError::UnknownTag {
                    tag: imp_tag,
                    offset: reader.position() - 1,
                });
            }
        }
    }
    Ok(())
}

/// Skip a drConstAddInfo record. The format varies by version.
/// For D11.3+ native, this appears in uses clauses.
fn skip_const_add_info(reader: &mut DcuReader) -> Result<(), DcuError> {
    // ReadConstAddInfo in DCU32 reads a complex structure.
    // For our purposes, we read the fields that D13 would have:
    // NDX (ReadUIndex) + data based on NDX value.
    // This is complex; for now we'll read the index and skip data.
    let _ndx = reader.read_uindex()?;
    // The exact size depends on the NDX value. For a simple implementation,
    // we read additional fields that are typical for D11.3+.
    let _val = reader.read_u32()?;
    Ok(())
}
