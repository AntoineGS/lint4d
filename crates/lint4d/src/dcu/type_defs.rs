use crate::dcu::class_parser::parse_class_def;
use crate::dcu::decl_parser::read_decl_list;
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;
use crate::dcu::types::fix_tag;
use crate::dcu::DcuVersion;

// --- Type definition skippers ---
// Skip Rec entries (type definitions) in the declaration list.

/// Read the common TTypeDef header (D13): RTTISz, Sz, hAddrDef, X.
/// DCU32 reference: TTypeDef.Create reads exactly 4 values.
/// Note: Sz is ReadIndex (signed) in DCU32, but read_uindex works for
/// non-negative sizes since the encoding is compatible.
pub(crate) fn read_type_def_header(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _rtti_sz = reader.read_uindex()?;
    let _sz = reader.read_index()?; // Signed in DCU32 (ReadIndex)
    let _h_addr_def = reader.read_uindex()?;
    let _x = reader.read_uindex()?; // D2005+ extra field
    Ok(())
}

/// Try to skip a type definition tag. Returns Ok(true) if handled, Ok(false) if not recognized.
pub(crate) fn try_skip_type_def(tag: u8, reader: &mut DcuReader) -> Result<bool, DcuError> {
    match tag {
        DR_ENUM_DEF => skip_enum_def(reader)?,
        DR_RANGE_DEF | DR_BOOL_RANGE_DEF | DR_CH_RANGE_DEF | DR_WCHAR_RANGE_DEF
        | DR_WIDE_RANGE_DEF => skip_range_def(reader)?,
        DR_FLOAT_DEF => skip_float_def(reader)?,
        DR_PTR_DEF | DR_DYN_ARRAY_DEF => skip_ptr_def(reader)?,
        DR_SET_DEF => skip_set_def(reader)?,
        DR_ARRAY_DEF => skip_array_def(reader)?,
        DR_PROC_TYPE_DEF => skip_proc_type_def(reader)?,
        DR_REC_DEF => skip_rec_def(reader)?,
        DR_INTERFACE_DEF => skip_interface_def(reader)?,
        DR_OBJ_VMT_DEF => skip_obj_vmt_def(reader)?,
        DR_OBJ_DEF => skip_obj_def(reader)?,
        DR_VOID | DR_TEMPLATE_ARG_DEF => {
            read_type_def_header(reader)?;
        }
        DR_META_CLASS_DEF => skip_meta_class_def(reader)?,
        DR_VARIANT_DEF => {
            read_type_def_header(reader)?;
            let _b = reader.read_byte()?;
        }
        DR_SHORT_STR_DEF => {
            read_type_def_header(reader)?;
            let _cp = reader.read_uindex()?;
        }
        DR_STRING_DEF | DR_WIDE_STR_DEF => {
            read_type_def_header(reader)?;
            let _cp = reader.read_uindex()?;
        }
        DR_TEXT_DEF | DR_FILE_DEF => {
            read_type_def_header(reader)?;
            let _h = reader.read_uindex()?;
        }
        DR_TEMPLATE_CALL => {
            read_type_def_header(reader)?;
            let _h_dt = reader.read_uindex()?;
            let cnt = reader.read_uindex()?;
            for _ in 0..cnt {
                let _v = reader.read_uindex()?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(crate) fn skip_enum_def(reader: &mut DcuReader) -> Result<(), DcuError> {
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

pub(crate) fn skip_range_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_base = reader.read_uindex()?;
    let _lo = reader.read_index()?;
    let _hi = reader.read_index()?;
    Ok(())
}

pub(crate) fn skip_float_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _b = reader.read_byte()?;
    Ok(())
}

pub(crate) fn skip_ptr_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_ref_dt = reader.read_uindex()?;
    Ok(())
}

pub(crate) fn skip_set_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_base = reader.read_uindex()?;
    Ok(())
}

pub(crate) fn skip_array_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_dt_ndx = reader.read_uindex()?;
    let _h_dt_el = reader.read_uindex()?;
    Ok(())
}

pub(crate) fn skip_proc_type_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_dt_res = reader.read_uindex()?;
    let _add_start = reader.read_uindex()?;
    let mut inner_tag = reader.read_byte()?;
    if fix_tag(inner_tag) >= 0x81 && fix_tag(inner_tag) <= 0x84 {
        inner_tag = reader.read_byte()?;
    }
    let _args = read_decl_list(reader, &mut inner_tag, true)?;
    Ok(())
}

/// Skip an object VMT definition (DR_OBJ_VMT_DEF).
/// DCU32 reference: TObjVMTDef.Create.
pub(crate) fn skip_obj_vmt_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_obj_dt = reader.read_uindex()?;
    let _vmt_sz = reader.read_uindex()?;
    Ok(())
}

/// Skip an object definition (DR_OBJ_DEF).
/// DCU32 reference: TObjDef.Create — inherits from TRecDef.
pub(crate) fn skip_obj_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    // Same structure as TRecDef for D13
    skip_rec_def(reader)
}

/// Skip a metaclass definition (DR_META_CLASS_DEF).
/// DCU32 reference: TMetaClassDef.Create — inherits from TClassDef.
pub(crate) fn skip_meta_class_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    // TMetaClassDef inherits TClassDef. Its Create reads:
    //   inherited Create (= TClassDef.Create)
    //   hCl = ReadUIndex
    let _class_members = parse_class_def(reader)?;
    let _h_cl = reader.read_uindex()?;
    Ok(())
}

/// Skip a record definition (DR_REC_DEF).
/// DCU32 reference: TRecDef.Create (for D13/D_XE2+).
pub(crate) fn skip_rec_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _b2 = reader.read_byte()?;
    let _b1 = reader.read_byte()?;
    if reader.ver >= DcuVersion::DXE2 {
        let _x0 = reader.read_byte()?;
    }
    let _x = reader.read_uindex()?;
    let _d1 = reader.read_uindex()?;
    let _d2 = reader.read_uindex()?;
    let _d3 = reader.read_uindex()?;
    // Read fields
    let mut inner_tag = reader.read_byte()?;
    let _members = read_decl_list(reader, &mut inner_tag, false)?;
    Ok(())
}

/// Skip an interface definition (DR_INTERFACE_DEF).
/// DCU32 reference: TInterfaceDef.Create (for D13).
pub(crate) fn skip_interface_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _bx = reader.read_byte()?; // D2009+
    let _h_parent = reader.read_uindex()?;
    let _vm_cnt = reader.read_index()?; // ReadIndex (signed)
    reader.skip(16)?; // GUID (16 bytes)
    let _b = reader.read_byte()?;
    if reader.ver >= DcuVersion::D2010 {
        let _by = reader.read_uindex()?;
    }
    let cnt = reader.read_uindex()?;
    for _ in 0..cnt {
        let _x1 = reader.read_uindex()?;
        let _x2 = reader.read_uindex()?;
    }
    // Read fields
    let mut inner_tag = reader.read_byte()?;
    let _members = read_decl_list(reader, &mut inner_tag, false)?;
    Ok(())
}
