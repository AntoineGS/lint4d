use crate::dcu::DcuUnit;
use crate::dcu::const_add_info::skip_decl_const_add_info;
use crate::dcu::decl_parser::read_decl_list_into;
use crate::dcu::header::parse_unit_header;
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;

/// Parse a complete DCU file, extracting the unit name, version, platform,
/// the list of imported unit names, and type declarations.
pub fn parse_dcu(data: &[u8]) -> Result<DcuUnit, DcuError> {
    let header = parse_unit_header(data)?;
    let mut reader = DcuReader::new(data, header.version);
    reader.set_position(header.body_offset);

    // Finish reading the first source file entry (header consumed the tag + name).
    let _file_time = reader.read_u32()?;
    let _file_index = reader.read_uindex()?;

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
    skip_uses(&mut reader, &mut tag, DR_DLL)?;

    // Walk the declaration list to extract type names.
    let mut types = Vec::new();
    // EOF is tolerated: the parser may read past the declaration section
    // into data blocks or debug info, hitting EOF gracefully.
    match read_decl_list_into(&mut reader, &mut tag, &mut types, false) {
        Ok(()) | Err(DcuError::UnexpectedEof { .. }) => {}
        Err(e) => return Err(e),
    }

    Ok(DcuUnit {
        name: header.name,
        version: header.version,
        platform: header.platform,
        imported_units,
        types,
    })
}

/// Apply the D2006+ tag fixup: raw tags in 0x2D..0x36 are remapped.
/// Raw 0x2D wraps to 0x36 (arClassVar). All others decrement by 1.
pub(crate) fn fix_tag(raw: u8) -> u8 {
    if (0x2D..=0x36).contains(&raw) {
        if raw == 0x2D {
            0x36 // arClassVar: raw 0x2D wraps to technical value 0x36
        } else {
            raw - 1
        }
    } else {
        raw
    }
}

// --- Source file / uses clause helpers ---

pub(crate) fn is_source_file_tag(tag: u8) -> bool {
    matches!(tag, DR_SRC | DR_OBJ | DR_RES | DR_ASM | DR_UNIT_INLINE_SRC)
}

fn read_uses(
    reader: &mut DcuReader,
    tag: &mut u8,
    tag_rq: u8,
    imported_units: &mut Vec<String>,
) -> Result<(), DcuError> {
    while *tag == tag_rq {
        let unit_name = reader.read_name()?;
        imported_units.push(unit_name);

        let _h_pack = reader.read_uindex()?;
        let _l = reader.read_uindex()?;
        let _l2 = reader.read_uindex()?;

        skip_import_records(reader)?;
        *tag = reader.read_byte()?;
    }
    Ok(())
}

fn skip_uses(reader: &mut DcuReader, tag: &mut u8, tag_rq: u8) -> Result<(), DcuError> {
    while *tag == tag_rq {
        let _unit_name = reader.read_name()?;

        let _l = reader.read_uindex()?;
        let _l1 = reader.read_u32()?;
        let _l2 = reader.read_uindex()?;

        skip_import_records(reader)?;
        *tag = reader.read_byte()?;
    }
    Ok(())
}

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
                let _l = reader.read_u32()?;
            }
            DR_CONST_ADD_INFO => {
                skip_decl_const_add_info(reader)?;
            }
            DR_STOP1 => break,
            _ => {
                return Err(DcuError::UnknownTag {
                    tag: imp_tag,
                    offset: reader.position() - 1,
                });
            }
        }
    }
    Ok(())
}
