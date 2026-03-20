use crate::dcu::header::parse_unit_header;
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;
use crate::dcu::{DcuUnit, TypeInfo, TypeKind};

/// Parse a complete DCU file, extracting the unit name, version, platform,
/// the list of imported unit names, and type declarations.
pub fn parse_dcu(data: &[u8]) -> Result<DcuUnit, DcuError> {
    let header = parse_unit_header(data)?;
    let mut reader = DcuReader::new(data);
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
    let types = read_decl_list(&mut reader, &mut tag)?;

    Ok(DcuUnit {
        name: header.name,
        version: header.version,
        platform: header.platform,
        imported_units,
        types,
    })
}

/// Apply the D2006+ tag fixup: raw tags in 0x2D..0x36 are decremented by 1
/// to map back to the original pre-D2006 constant values.
fn fix_tag(raw: u8) -> u8 {
    if raw >= 0x2D && raw <= 0x36 {
        raw - 1
    } else {
        raw
    }
}

/// Walk the main declaration list, extracting type declarations.
///
/// Reads tags until a stop/structural tag or an unrecognized tag.
/// The `tag` parameter holds the current (already-read) raw tag byte.
fn read_decl_list(
    reader: &mut DcuReader,
    tag: &mut u8,
) -> Result<Vec<TypeInfo>, DcuError> {
    let mut types = Vec::new();

    loop {
        let fixed = fix_tag(*tag);
        match fixed {
            // Type declaration: extract name.
            DR_TYPE => {
                if let Some(ti) = read_type_decl(reader)? {
                    types.push(ti);
                }
            }
            // Type P declaration (VMT pointer type, names usually start with '.').
            // In D13 this has an extra ReadUIndex field after hDef.
            DR_TYPE_P => {
                if let Some(ti) = read_type_p_decl(reader)? {
                    types.push(ti);
                }
            }
            // drUnitAddInfo: namespace segments with nested declaration lists.
            DR_UNIT_ADD_INFO => {
                skip_unit_add_info(reader)?;
                *tag = reader.read_byte()?;
                continue;
            }
            // Variable declarations: TNameFDecl + hDT + Ofs.
            DR_VAR | DR_VAR_C | DR_SPEC_VAR => {
                skip_var_decl(reader)?;
            }
            // Constant declaration: TNameFDecl + hDT + const value.
            DR_CONST => {
                skip_const_decl(reader)?;
            }
            // Thread variable: same layout as TVarDecl in D13.
            DR_THREAD_VAR => {
                skip_var_decl(reader)?;
            }
            // Resource string: same layout as TVarDecl.
            DR_RES_STR => {
                skip_var_decl(reader)?;
            }
            // drStop2: read u32 (for D8+).
            DR_STOP2 => {
                let _l = reader.read_u32()?;
                *tag = reader.read_byte()?;
                continue;
            }
            // drConstAddInfo in declaration list context.
            DR_CONST_ADD_INFO => {
                skip_decl_const_add_info(reader)?;
                *tag = reader.read_byte()?;
                continue;
            }
            // drProcAddInfo: ReadIndex.
            DR_PROC_ADD_INFO => {
                let _v = reader.read_index()?;
                *tag = reader.read_byte()?;
                continue;
            }
            // Call kind tags: no data, just skip the tag.
            0x81..=0x84 => {
                *tag = reader.read_byte()?;
                continue;
            }
            // Stop / structural tags: end of declaration list.
            DR_STOP | DR_STOP_A | DR_CBLOCK | DR_FIXUP => {
                break;
            }
            // drStop1: end of nested list.
            DR_STOP1 => {
                break;
            }
            // drEmbeddedProcStart: skip the entire embedded proc block.
            DR_EMBEDDED_PROC_START => {
                skip_embedded_proc(reader)?;
                *tag = reader.read_byte()?;
                continue;
            }
            // drProc: procedure declaration -- complex, try to skip it.
            DR_PROC => {
                match skip_proc_decl(reader) {
                    Ok(()) => {}
                    Err(_) => break,
                }
            }
            // drExport: TNameDecl + index.
            DR_EXPORT => {
                let _name = reader.read_name()?;
                let _idx = reader.read_uindex()?;
            }
            // Type definition tags (Rec entries). Skip to continue parsing.
            DR_ENUM_DEF => {
                skip_enum_def(reader)?;
            }
            DR_RANGE_DEF | DR_BOOL_RANGE_DEF | DR_CH_RANGE_DEF
            | DR_WCHAR_RANGE_DEF | DR_WIDE_RANGE_DEF => {
                skip_range_def(reader)?;
            }
            DR_FLOAT_DEF => {
                skip_float_def(reader)?;
            }
            DR_PTR_DEF => {
                skip_ptr_def(reader)?;
            }
            DR_SET_DEF => {
                skip_set_def(reader)?;
            }
            DR_ARRAY_DEF => {
                skip_array_def(reader)?;
            }
            DR_PROC_TYPE_DEF => {
                skip_proc_type_def(reader)?;
            }
            DR_CLASS_DEF => {
                skip_class_def(reader)?;
            }
            DR_REC_DEF => {
                skip_rec_def(reader)?;
            }
            DR_INTERFACE_DEF => {
                skip_interface_def(reader)?;
            }
            // Unknown or unhandled tag: stop gracefully.
            _ => {
                break;
            }
        }
        *tag = reader.read_byte()?;
    }

    Ok(types)
}

/// Read a drType declaration (type declaration).
/// Returns Some(TypeInfo) for user-visible types (not starting with '.' or ':').
///
/// Field layout (D13):
///   Name (ReadName)
///   TNameFDecl: F, F1, F4, [Inf], [B2]
///   hDef (ReadUIndex)
fn read_type_decl(
    reader: &mut DcuReader,
) -> Result<Option<TypeInfo>, DcuError> {
    let name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _h_def = reader.read_uindex()?;

    if name.starts_with('.') || name.starts_with(':') || name.is_empty() {
        return Ok(None);
    }

    Ok(Some(TypeInfo {
        name,
        kind: TypeKind::Other,
        parent: None,
        fields: Vec::new(),
        methods: Vec::new(),
        interface_guid: None,
    }))
}

/// Read a drTypeP (VMT pointer type) declaration.
/// In D13, TTypePDecl has an extra ReadUIndex field after hDef
/// that is not present in the DCU32 reference code.
fn read_type_p_decl(
    reader: &mut DcuReader,
) -> Result<Option<TypeInfo>, DcuError> {
    let name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _h_def = reader.read_uindex()?;
    // D13 extra field: observed in binary but not documented in DCU32.
    let _extra = reader.read_uindex()?;

    if name.starts_with('.') || name.starts_with(':') || name.is_empty() {
        return Ok(None);
    }

    Ok(Some(TypeInfo {
        name,
        kind: TypeKind::Other,
        parent: None,
        fields: Vec::new(),
        methods: Vec::new(),
        interface_guid: None,
    }))
}

/// Read TNameFDecl fields: F, F1, F4, optionally Inf and B2.
struct NameFFields {
    _f: u32,
    _f1: u32,
}

fn read_namef_fields(
    reader: &mut DcuReader,
    no_inf: bool,
) -> Result<NameFFields, DcuError> {
    let f = reader.read_uindex()?;
    let f1 = reader.read_uindex()?; // D8+
    let _f4 = reader.read_uindex()?; // D2009+

    if !no_inf && (f & 0x40) != 0 {
        let _inf = reader.read_u32()?;
    }

    if (f1 & 0x80) != 0 {
        let _b2 = reader.read_uindex()?;
        // D8 exact also reads F3 if F & 0x08, but D13 is not D8.
    }

    Ok(NameFFields { _f: f, _f1: f1 })
}

/// Skip a variable declaration (drVar, drVarC, drSpecVar, drThreadVar).
fn skip_var_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _h_dt = reader.read_uindex()?;
    let _ofs = reader.read_uindex()?;
    Ok(())
}

/// Skip a constant declaration (drConst).
fn skip_const_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _h_dt = reader.read_uindex()?;

    // TConstValInfo.Read:
    let kind = reader.read_uindex()?;
    let val_sz = reader.read_uindex()?;
    if val_sz == 0 {
        // For D_XE2+: Kind == 4 (pointer/nil) skips reading Val.
        if kind != 4 {
            let _val = reader.read_index()?;
        }
    } else {
        reader.skip(val_sz as usize)?;
    }
    Ok(())
}

/// Skip a drUnitAddInfo record and its nested declaration list.
fn skip_unit_add_info(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _b = reader.read_uindex()?;

    let mut inner_tag = reader.read_byte()?;
    let _inner_types = read_decl_list(reader, &mut inner_tag)?;
    Ok(())
}

/// Skip a drConstAddInfo record in the declaration list context.
/// Uses a tag-based sub-protocol; for D2009+ the stop marker is 0xFF.
fn skip_decl_const_add_info(reader: &mut DcuReader) -> Result<(), DcuError> {
    loop {
        let sub_tag = reader.read_byte()?;
        if sub_tag >= 0xFF {
            break;
        }
        match sub_tag {
            0x01 => {
                let _h_def = reader.read_uindex()?;
                let f = reader.read_uindex()?;
                if (f & 0x0100_0000) != 0 {
                    let _ip = reader.read_uindex()?;
                }
            }
            0x02 => {
                let _msg = reader.read_name()?;
            }
            0x03 | 0x04 => {
                let _h_def = reader.read_uindex()?;
            }
            0x05 => {
                let _h_def = reader.read_uindex()?;
                let _v = reader.read_uindex()?;
            }
            0x06 | 0x07 => {
                let _h_def = reader.read_uindex()?;
                let _h_def2 = reader.read_uindex()?;
            }
            0x08 => {
                let n = reader.read_uindex()?;
                for _ in 0..n {
                    skip_attribute_record(reader)?;
                }
            }
            0x09 | 0x0B | 0x0C | 0x0D | 0x0E => {
                let _v = reader.read_uindex()?;
            }
            0x0A => {
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            _ => break,
        }
    }
    Ok(())
}

/// Skip a single attribute record within drConstAddInfo.
fn skip_attribute_record(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _h_attr_ctor = reader.read_uindex()?;
    let _z = reader.read_uindex()?;
    let _h_attr_dt = reader.read_uindex()?;
    let arg_cnt = reader.read_uindex()?;
    for _ in 0..arg_cnt {
        let arg_kind = reader.read_uindex()?;
        match arg_kind {
            0 => {
                let _h_arg_t = reader.read_uindex()?;
                let kind = reader.read_uindex()?;
                let sz = reader.read_uindex()?;
                if sz > 0 {
                    reader.skip(sz as usize)?;
                } else if kind != 4 {
                    let _v = reader.read_uindex()?;
                }
            }
            1 => {
                let _h_dt = reader.read_uindex()?;
                let _h_dt_addr = reader.read_uindex()?;
            }
            _ => {
                return Err(DcuError::UnknownTag {
                    tag: arg_kind as u8,
                    offset: reader.position(),
                });
            }
        }
    }
    Ok(())
}

/// Skip an embedded proc block (drEmbeddedProcStart .. drEmbeddedProcEnd).
fn skip_embedded_proc(reader: &mut DcuReader) -> Result<(), DcuError> {
    let mut inner_tag = reader.read_byte()?;
    loop {
        let fixed = fix_tag(inner_tag);
        if fixed == DR_EMBEDDED_PROC_END {
            break;
        }
        if fixed == DR_EMBEDDED_PROC_START {
            skip_embedded_proc(reader)?;
            inner_tag = reader.read_byte()?;
            continue;
        }
        let mut temp_tag = inner_tag;
        let _inner_types = read_decl_list(reader, &mut temp_tag)?;
        if fix_tag(temp_tag) == DR_EMBEDDED_PROC_END {
            break;
        }
        inner_tag = reader.read_byte()?;
    }
    Ok(())
}

/// Skip a procedure declaration (drProc).
fn skip_proc_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
    let name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;

    let _b0 = reader.read_uindex()?;
    let _sz = reader.read_uindex()?;
    // XE+ extra byte
    let _xe_byte = reader.read_byte()?;

    let is_unnamed = name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.starts_with('$');

    if !is_unnamed {
        let _v_proc = reader.read_uindex()?;
        let _h_dt_res = reader.read_uindex()?;
        let _h_class = reader.read_uindex()?;

        let mut inner_tag = reader.read_byte()?;

        // ReadCallKind: consume call kind tag if present.
        if fix_tag(inner_tag) >= 0x81 && fix_tag(inner_tag) <= 0x84 {
            inner_tag = reader.read_byte()?;
        }

        // D2009+: check for drA5Info and drA6Info.
        if fix_tag(inner_tag) == DR_A5_INFO {
            inner_tag = reader.read_byte()?;
        }
        if fix_tag(inner_tag) == DR_A6_INFO {
            skip_a6_def(reader)?;
            inner_tag = reader.read_byte()?;
        }

        // ReadDeclList for args until stop tag.
        let _args = read_decl_list(reader, &mut inner_tag)?;

        if fix_tag(inner_tag) != DR_STOP1 {
            return Err(DcuError::UnknownTag {
                tag: inner_tag,
                offset: reader.position() - 1,
            });
        }
    }

    Ok(())
}

/// Skip a TA6Def (template parameter info).
fn skip_a6_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    let cnt = reader.read_uindex()?;
    for _ in 0..cnt {
        let _h_dt = reader.read_uindex()?;
        let _v = reader.read_uindex()?;
    }
    Ok(())
}

// --- Type definition skippers ---
// Skip Rec entries (type definitions) in the declaration list.

/// Read the common TTypeDef header: RTTISz, hAddrDef, RTTIOfs, Sz, hUnit.
fn read_type_def_header(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _rtti_sz = reader.read_uindex()?;
    let _h_addr_def = reader.read_uindex()?;
    let _rtti_ofs = reader.read_uindex()?;
    let _sz = reader.read_uindex()?;
    let _h_unit = reader.read_uindex()?;
    Ok(())
}

fn skip_enum_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_base = reader.read_uindex()?;
    loop {
        let v = reader.read_index()?;
        if v < 0 {
            break;
        }
    }
    Ok(())
}

fn skip_range_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_base = reader.read_uindex()?;
    let _lo = reader.read_index()?;
    let _hi = reader.read_index()?;
    Ok(())
}

fn skip_float_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _b = reader.read_byte()?;
    Ok(())
}

fn skip_ptr_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_ref_dt = reader.read_uindex()?;
    Ok(())
}

fn skip_set_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_base = reader.read_uindex()?;
    Ok(())
}

fn skip_array_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_dt_ndx = reader.read_uindex()?;
    let _h_dt_el = reader.read_uindex()?;
    Ok(())
}

fn skip_proc_type_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_dt_res = reader.read_uindex()?;
    let _add_start = reader.read_uindex()?;
    let mut inner_tag = reader.read_byte()?;
    if fix_tag(inner_tag) >= 0x81 && fix_tag(inner_tag) <= 0x84 {
        inner_tag = reader.read_byte()?;
    }
    let _args = read_decl_list(reader, &mut inner_tag)?;
    Ok(())
}

fn skip_class_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_parent = reader.read_uindex()?;
    let _inst_base_rtti_sz = reader.read_uindex()?;
    let _inst_base_sz = reader.read_uindex()?;
    let _h_ndx3 = reader.read_uindex()?;
    let _h_ndx4 = reader.read_uindex()?;
    let mut inner_tag = reader.read_byte()?;
    let _members = read_decl_list(reader, &mut inner_tag)?;
    Ok(())
}

fn skip_rec_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_ndx3 = reader.read_uindex()?;
    let _h_ndx4 = reader.read_uindex()?;
    let mut inner_tag = reader.read_byte()?;
    let _members = read_decl_list(reader, &mut inner_tag)?;
    Ok(())
}

fn skip_interface_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_parent = reader.read_uindex()?;
    let _vm_cnt = reader.read_uindex()?;
    let _h_ndx3 = reader.read_uindex()?;
    reader.skip(16)?; // GUID
    let mut inner_tag = reader.read_byte()?;
    let _members = read_decl_list(reader, &mut inner_tag)?;
    Ok(())
}

// --- Source file / uses clause helpers ---

fn is_source_file_tag(tag: u8) -> bool {
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

fn skip_uses(
    reader: &mut DcuReader,
    tag: &mut u8,
    tag_rq: u8,
) -> Result<(), DcuError> {
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
                skip_const_add_info(reader)?;
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

/// Skip a drConstAddInfo record in the uses clause context.
fn skip_const_add_info(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _ndx = reader.read_uindex()?;
    let _val = reader.read_u32()?;
    Ok(())
}
