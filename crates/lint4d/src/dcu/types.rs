use crate::dcu::DcuUnit;
use crate::dcu::const_add_info::skip_decl_const_add_info;
use crate::dcu::decl_parser::{read_decl_list_into_for_provider, read_decl_list_into_with_exports};
use crate::dcu::header::{parse_unit_header, parse_unit_header_for_provider};
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;

/// Parse a complete DCU file, extracting the unit name, version, platform,
/// the list of imported unit names, and type declarations.
pub fn parse_dcu(data: &[u8]) -> Result<DcuUnit, DcuError> {
    parse_dcu_with_exported_type_indices(data).map(|(unit, _)| unit)
}

/// Resource bounds for the strict compiled-navigation parser. Both limits are
/// charged during decoding, before retaining a new type/member record or
/// allocating a decoded Pascal name.
#[derive(Debug, Clone, Copy)]
pub struct ProviderParseLimits {
    pub max_records: usize,
    pub max_decoded_bytes: usize,
}

/// Strict parser output with independent root-export and class-definition
/// evidence. The ordinary lint parser intentionally retains its tolerant API.
#[derive(Debug)]
pub struct ProviderDcu {
    pub unit: DcuUnit,
    pub exported_type_indices: Vec<usize>,
    pub class_definition_type_indices: Vec<usize>,
    pub decoded_records: usize,
    pub decoded_bytes: usize,
}

/// Parse a complete unit declaration list for compiled navigation. Unlike
/// [`parse_dcu`], this rejects EOF and unknown declaration tails after a valid
/// prefix; only the observed `DR_STOP` root terminator authorizes a provider.
pub fn parse_dcu_for_provider(
    data: &[u8],
    limits: ProviderParseLimits,
) -> Result<ProviderDcu, DcuError> {
    let (header, mut reader) =
        parse_unit_header_for_provider(data, limits.max_records, limits.max_decoded_bytes)?;

    let _file_time = reader.read_u32()?;
    let _file_index = reader.read_uindex()?;
    let mut tag = reader.read_byte()?;
    while is_source_file_tag(tag) {
        reader.charge_decoded_record()?;
        let _name = reader.read_name()?;
        let _ft = reader.read_u32()?;
        let _idx = reader.read_uindex()?;
        tag = reader.read_byte()?;
    }

    let mut imported_units = Vec::new();
    read_uses(&mut reader, &mut tag, DR_UNIT, &mut imported_units)?;
    read_uses(&mut reader, &mut tag, DR_UNIT1, &mut imported_units)?;
    skip_uses(&mut reader, &mut tag, DR_DLL)?;

    let mut types = Vec::new();
    let mut exported_type_indices = Vec::new();
    let mut class_definition_type_indices = Vec::new();
    read_decl_list_into_for_provider(
        &mut reader,
        &mut tag,
        &mut types,
        &mut exported_type_indices,
        &mut class_definition_type_indices,
    )?;
    if tag != DR_STOP {
        return Err(DcuError::UnknownTag {
            tag,
            offset: reader.position(),
        });
    }
    let (decoded_records, decoded_bytes) = reader.provider_decode_usage();
    Ok(ProviderDcu {
        unit: DcuUnit {
            name: header.name,
            version: header.version,
            platform: header.platform,
            imported_units,
            types,
        },
        exported_type_indices,
        class_definition_type_indices,
        decoded_records,
        decoded_bytes,
    })
}

/// Parse a DCU and return indices of type records proven to occur directly in
/// the unit-root declaration list. `DcuUnit::types` intentionally retains the
/// historical flattened parser view; callers that expose unit exports must use
/// this provenance instead of treating every decoded record as public.
pub fn parse_dcu_with_exported_type_indices(
    data: &[u8],
) -> Result<(DcuUnit, Vec<usize>), DcuError> {
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
    let mut exported_type_indices = Vec::new();
    // EOF is tolerated: the parser may read past the declaration section
    // into data blocks or debug info, hitting EOF gracefully.
    match read_decl_list_into_with_exports(
        &mut reader,
        &mut tag,
        &mut types,
        &mut exported_type_indices,
    ) {
        Ok(()) | Err(DcuError::UnexpectedEof { .. }) => {}
        Err(e) => return Err(e),
    }

    Ok((
        DcuUnit {
            name: header.name,
            version: header.version,
            platform: header.platform,
            imported_units,
            types,
        },
        exported_type_indices,
    ))
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
        reader.charge_decoded_record()?;
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

#[cfg(test)]
mod provider_strictness_tests {
    use super::*;
    use crate::dcu::decl_parser::read_decl_list_into;

    fn root_terminator_offset(data: &[u8]) -> usize {
        let header = parse_unit_header(data).expect("fixture header");
        let mut reader = DcuReader::new(data, header.version);
        reader.set_position(header.body_offset);
        reader.read_u32().unwrap();
        reader.read_uindex().unwrap();
        let mut tag = reader.read_byte().unwrap();
        while is_source_file_tag(tag) {
            reader.read_name().unwrap();
            reader.read_u32().unwrap();
            reader.read_uindex().unwrap();
            tag = reader.read_byte().unwrap();
        }
        let mut imported_units = Vec::new();
        read_uses(&mut reader, &mut tag, DR_UNIT, &mut imported_units).unwrap();
        read_uses(&mut reader, &mut tag, DR_UNIT1, &mut imported_units).unwrap();
        skip_uses(&mut reader, &mut tag, DR_DLL).unwrap();
        let mut types = Vec::new();
        read_decl_list_into(&mut reader, &mut tag, &mut types, false).unwrap();
        assert!(!types.is_empty());
        assert_eq!(tag, DR_STOP);
        reader.position() - 1
    }

    #[test]
    fn provider_parser_rejects_malformed_fixture_tails_while_tolerant_api_remains_tolerant() {
        let fixture = include_bytes!(
            "../../tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu"
        );
        let terminal = root_terminator_offset(fixture);
        let limits = ProviderParseLimits {
            max_records: 32_768,
            max_decoded_bytes: 4 * 1024 * 1024,
        };

        let mut truncated = fixture.to_vec();
        truncated.truncate(terminal);
        assert!(parse_dcu_for_provider(&truncated, limits).is_err());
        assert!(
            !parse_dcu(&truncated)
                .expect("legacy tolerant parser still tolerates a truncated declaration tail")
                .types
                .is_empty()
        );

        let mut unknown = fixture.to_vec();
        unknown[terminal] = 0xFF;
        assert!(parse_dcu_for_provider(&unknown, limits).is_err());
        assert!(
            !parse_dcu(&unknown)
                .expect("legacy tolerant parser still tolerates an unknown declaration tail")
                .types
                .is_empty()
        );
    }
}
