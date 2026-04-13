use crate::dcu::const_add_info::skip_decl_const_add_info;
use crate::dcu::decl_parser::{skip_const_decl, skip_embedded_proc, skip_proc_decl, skip_var_decl};
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;
use crate::dcu::type_defs::try_skip_type_def;
use crate::dcu::types::fix_tag;
use crate::dcu::{
    DcuVersion, FieldInfo, MethodInfo, MethodKind, TypeInfo, TypeKind, TypeRef, Visibility,
};

/// Indicates an Inf field is present in TNameFDecl.
const NF_INF: u32 = 0x40;
/// Indicates a B2 field is present (D8+ LocFlagsX bit).
const NF_B2: u32 = 0x80;

/// Read TNameFDecl fields: F, F1, F4, optionally Inf and B2.
pub(crate) struct NameFFields {
    pub _f: u32,
    pub _f1: u32,
}

pub(crate) fn read_namef_fields(
    reader: &mut DcuReader,
    no_inf: bool,
) -> Result<NameFFields, DcuError> {
    let f = reader.read_uindex()?;
    let f1 = reader.read_uindex()?; // D8+
    let _f4 = reader.read_uindex()?; // D2009+

    if !no_inf && (f & NF_INF) != 0 {
        let _inf = reader.read_u32()?;
    }

    if (f1 & NF_B2) != 0 {
        let _b2 = reader.read_uindex()?;
        // D8 exact also reads F3 if F & 0x08, but D13 is not D8.
    }

    Ok(NameFFields { _f: f, _f1: f1 })
}

/// Read a drType declaration (type declaration).
/// Returns Some(TypeInfo) for user-visible types (not starting with '.' or ':').
///
/// Field layout (D13):
///   Name (ReadName)
///   TNameFDecl: F, F1, F4, [Inf], [B2]
///   hDef (ReadUIndex)
pub(crate) fn read_type_decl(reader: &mut DcuReader) -> Result<Option<TypeInfo>, DcuError> {
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
pub(crate) fn read_type_p_decl(reader: &mut DcuReader) -> Result<Option<TypeInfo>, DcuError> {
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

/// Skip a TLocalDecl record (AR_VAL, AR_VAR, AR_RESULT, AR_FLD, AR_ABS_LOC_VAR, AR_LABEL).
///
/// TLocalDecl inherits from TNameDecl, NOT TNameFDecl.
/// D13 layout:
///   Name (ReadName) -- from TNameDecl
///   LocFlags (ReadUIndex)
///   LocFlagsX (ReadUIndex) -- D8+
///   Extra (ReadUIndex) -- D2009+
///   hDT (ReadUIndex for non-method, ReadIndex for method)
///   Ndx (ReadIndex for non-method, ReadUIndex for method)
pub(crate) fn skip_local_decl(
    reader: &mut DcuReader,
    is_method: bool,
    in_args: bool,
) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let loc_flags = reader.read_uindex()?; // LocFlags
    let _loc_flags_x = reader.read_uindex()?; // LocFlagsX (D8+)
    let _extra = reader.read_uindex()?; // D2009+
    if reader.ver >= DcuVersion::DXE4 && in_args && (loc_flags & 0x40 != 0) {
        reader.skip(4)?;
    }
    if is_method {
        let _h_dt = reader.read_index()?;
        let _ndx = reader.read_uindex()?;
    } else {
        let _h_dt = reader.read_uindex()?;
        let _ndx = reader.read_index()?;
    }
    Ok(())
}

/// Read a TLocalDecl for a field, returning the name and type index.
pub(crate) fn read_field_decl(reader: &mut DcuReader) -> Result<(String, u32), DcuError> {
    let name = reader.read_name()?;
    let _loc_flags = reader.read_uindex()?;
    let _loc_flags_x = reader.read_uindex()?;
    let _extra = reader.read_uindex()?;
    let h_dt = reader.read_uindex()?;
    let _ndx = reader.read_index()?;
    Ok((name, h_dt))
}

/// Derive field/method visibility from LocFlags.
#[allow(dead_code)]
pub(crate) fn visibility_from_flags(flags: u32) -> Visibility {
    // Visibility is encoded in bits 0..3 of LocFlagsX (or LocFlags for pre-D8).
    // For D8+, LocFlagsX has adjusted bits. We use the raw F here.
    // The exact encoding depends on version but commonly:
    //   0x00 = private, 0x02 = public, 0x04 = protected, 0x0A = published
    let vis_bits = flags & 0x0F;
    match vis_bits {
        0x00 => Visibility::Private,
        0x02 => Visibility::Public,
        0x04 => Visibility::Protected,
        0x0A => Visibility::Published,
        _ => Visibility::Public, // default to public for unknown
    }
}

/// Skip a method declaration (AR_METHOD, AR_CONSTR, AR_DESTR) in a class body.
///
/// TMethodDecl inherits from TLocalDecl, then reads additional method-specific
/// fields. The exact extra fields depend on the method kind and version.
pub(crate) fn skip_method_decl(reader: &mut DcuReader, method_tag: u8) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    // TLocalDecl fields
    let _loc_flags = reader.read_uindex()?;
    let _loc_flags_x = reader.read_uindex()?;
    let _extra = reader.read_uindex()?;
    // TLocalDecl for methods: hDT = ReadIndex, Ndx = ReadUIndex
    let _h_dt = reader.read_index()?;
    let _ndx = reader.read_uindex()?;

    // TMethodDecl extra fields (for non-interface, D13):
    // D2009+ and tag != arMethod: ReadByte
    if method_tag != AR_METHOD {
        let _b = reader.read_byte()?;
    }

    // D7+: hImport = ReadUIndex
    let _h_import = reader.read_uindex()?;

    // D2009+ and tag == arMethod: skip bytes from a version-dependent set.
    // For D13 (>= D_XE7), read bytes while they match a known set.
    if method_tag == AR_METHOD {
        skip_method_extra_bytes(reader)?;
    }

    Ok(())
}

/// Return the version-dependent set of bytes to skip after a method declaration.
/// CnWizards sSkip array: cumulative byte sets per version level.
pub(crate) fn method_skip_set(ver: DcuVersion) -> &'static [u8] {
    if ver >= DcuVersion::DXE7 {
        &[
            0x00, 0x01, 0x02, 0x04, 0x08, 0x09, 0x10, 0x18, 0x20, 0x21, 0x22, 0x28, 0x38, 0x42,
            0x47, 0x4F, 0x60, 0x61, 0x80, 0x84,
        ]
    } else if ver >= DcuVersion::DXE4 {
        &[
            0x00, 0x01, 0x02, 0x04, 0x08, 0x09, 0x10, 0x18, 0x20, 0x21, 0x22, 0x28, 0x38, 0x42,
            0x47, 0x4F, 0x61, 0x80, 0x84,
        ]
    } else if ver >= DcuVersion::DXE3 {
        &[
            0x00, 0x01, 0x02, 0x04, 0x08, 0x09, 0x10, 0x18, 0x20, 0x21, 0x22, 0x28, 0x38, 0x42,
            0x61, 0x80, 0x84,
        ]
    } else if ver >= DcuVersion::DXE2 {
        &[
            0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x18, 0x20, 0x21, 0x28, 0x38, 0x61, 0x80, 0x84,
        ]
    } else if ver >= DcuVersion::D2010 {
        &[
            0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x18, 0x20, 0x21, 0x61, 0x80, 0x84,
        ]
    } else {
        &[
            0x00, 0x02, 0x04, 0x08, 0x10, 0x18, 0x20, 0x21, 0x61, 0x80, 0x84,
        ]
    }
}

/// Skip the version-dependent extra bytes after a method declaration.
pub(crate) fn skip_method_extra_bytes(reader: &mut DcuReader) -> Result<(), DcuError> {
    let allowed = method_skip_set(reader.ver);
    loop {
        let b = reader.read_byte()?;
        if !allowed.contains(&b) {
            reader.unread(1);
            break;
        }
    }
    Ok(())
}

/// Read a method declaration, returning the name and kind.
pub(crate) fn read_method_info(
    reader: &mut DcuReader,
    method_tag: u8,
) -> Result<(String, MethodKind), DcuError> {
    let name = reader.read_name()?;
    // TLocalDecl fields
    let _loc_flags = reader.read_uindex()?;
    let _loc_flags_x = reader.read_uindex()?;
    let _extra = reader.read_uindex()?;
    // TLocalDecl: hDT = ReadIndex, Ndx = ReadUIndex for methods
    let _h_dt = reader.read_index()?;
    let _ndx = reader.read_uindex()?;

    // TMethodDecl extra fields
    if method_tag != AR_METHOD {
        let _b = reader.read_byte()?;
    }
    let _h_import = reader.read_uindex()?;
    if method_tag == AR_METHOD {
        skip_method_extra_bytes(reader)?;
    }

    let kind = match method_tag {
        AR_CONSTR => MethodKind::Constructor,
        AR_DESTR => MethodKind::Destructor,
        AR_METHOD => {
            // Could be procedure or function. Without deeper analysis,
            // default to Procedure. hDT != 0 would suggest Function.
            if _h_dt != 0 {
                MethodKind::Function
            } else {
                MethodKind::Procedure
            }
        }
        _ => MethodKind::Procedure,
    };

    Ok((name, kind))
}

/// Skip a property declaration (AR_PROPERTY).
///
/// TPropDecl layout for D13:
///   Name (ReadName)
///   LocFlags (ReadIndex -- signed!)
///   LocFlagsX (ReadUIndex) -- D8+
///   Extra (ReadUIndex) -- D2009+
///   hDT (ReadUIndex)
///   NDX (ReadIndex)
///   hIndex (ReadIndex)
///   hRead (ReadUIndex)
///   hWrite (ReadUIndex)
///   hStored (ReadUIndex)
///   hReadOrig (ReadUIndex) -- D8+
///   hWriteOrig (ReadUIndex) -- D8+
///   hDeft (ReadIndex)
pub(crate) fn skip_property_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    // Property reads LocFlags with ReadIndex (signed), not ReadUIndex.
    let _loc_flags = reader.read_index()?;
    let _loc_flags_x = reader.read_uindex()?; // D8+
    let _extra = reader.read_uindex()?; // D2009+
    let _h_dt = reader.read_uindex()?;
    let _ndx = reader.read_index()?;
    let _h_index = reader.read_index()?;
    let _h_read = reader.read_uindex()?;
    let _h_write = reader.read_uindex()?;
    let _h_stored = reader.read_uindex()?;
    let _h_read_orig = reader.read_uindex()?; // D8+
    let _h_write_orig = reader.read_uindex()?; // D8+
    let _h_deft = reader.read_index()?;
    Ok(())
}

/// Parse a class definition, extracting fields and methods.
///
/// Returns (fields, methods) collected from the class member sub-list.
/// DCU32 reference: TClassDef.Create (for D13/XE7+).
pub(crate) fn parse_class_def(
    reader: &mut DcuReader,
) -> Result<(Vec<FieldInfo>, Vec<MethodInfo>), DcuError> {
    use crate::dcu::type_defs::read_type_def_header;
    read_type_def_header(reader)?;
    let _bx = reader.read_byte()?;
    if reader.ver >= DcuVersion::DXE2 {
        let _bx_xe2 = reader.read_byte()?;
    } else {
        let _bx_pre_xe2 = reader.read_uindex()?;
    }
    let _bx2 = reader.read_byte()?;
    let _h_parent = reader.read_uindex()?;
    let _inst_base_rtti_sz = reader.read_uindex()?;
    let _inst_base_sz = reader.read_index()?; // ReadIndex (signed)
    let _inst_base_v = reader.read_uindex()?;
    let _vm_cnt = reader.read_uindex()?;
    let _ndx_fe = reader.read_uindex()?;
    let _prop_cnt = reader.read_uindex()?;
    let _b04 = reader.read_uindex()?; // D8+
    // D2010+: BX3
    let _bx3 = reader.read_uindex()?;
    // ReadBeforeIntf: nothing for TClassDef (empty override)
    // ReadClassInterfaces:
    let i_cnt = reader.read_index()?;
    if i_cnt > 0 {
        for _ in 0..i_cnt {
            let _h_intf = reader.read_uindex()?;
            let _m_cnt = reader.read_uindex()?;
            // D2006+ non-MSIL:
            let _x1 = reader.read_uindex()?;
            let _match_cnt = reader.read_uindex()?;
            // D2010+:
            let _x3 = reader.read_uindex()?;
            let _x4 = reader.read_uindex()?;
            for _ in 0.._match_cnt {
                let _b = reader.read_byte()?;
                let _m_name = reader.read_name()?;
                let _n = reader.read_uindex()?;
                let _h_member = reader.read_uindex()?;
            }
        }
    }

    // Read the class member sub-list, collecting fields and methods.
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut tag = reader.read_byte()?;

    loop {
        let fixed = fix_tag(tag);
        match fixed {
            AR_FLD => match read_field_decl(reader) {
                Ok((name, h_dt)) => {
                    if !name.is_empty() && !name.starts_with('.') {
                        fields.push(FieldInfo {
                            name,
                            type_ref: TypeRef::Unresolved(h_dt),
                            visibility: Visibility::Private,
                        });
                    }
                }
                Err(_) => break,
            },
            AR_METHOD | AR_CONSTR | AR_DESTR => match read_method_info(reader, fixed) {
                Ok((name, kind)) => {
                    if !name.is_empty() {
                        methods.push(MethodInfo {
                            name,
                            kind,
                            params: Vec::new(),
                            return_type: None,
                        });
                    }
                }
                Err(_) => break,
            },
            AR_PROPERTY => match skip_property_decl(reader) {
                Ok(()) => {}
                Err(_) => break,
            },
            AR_CLASS_VAR => match skip_local_decl(reader, false, false) {
                Ok(()) => {}
                Err(_) => break,
            },
            AR_VAL | AR_VAR | AR_RESULT | AR_ABS_LOC_VAR | AR_LABEL => {
                match skip_local_decl(reader, false, false) {
                    Ok(()) => {}
                    Err(_) => break,
                }
            }
            AR_SET_DEFT => {
                let _v = reader.read_u32()?;
            }
            AR_COPY_DECL => {
                let _name = reader.read_name()?;
                let _nf = read_namef_fields(reader, true)?;
                let _src = reader.read_uindex()?;
            }
            DR_TYPE => {
                let _ti = read_type_decl(reader)?;
            }
            DR_TYPE_P => {
                let _ti = read_type_p_decl(reader)?;
            }
            DR_PROC => {
                let mut dummy_types = Vec::new();
                match skip_proc_decl(reader, &mut dummy_types) {
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            DR_CONST => match skip_const_decl(reader) {
                Ok(()) => {}
                Err(_) => break,
            },
            // Nested class def: parse but discard.
            DR_CLASS_DEF => {
                let _inner = parse_class_def(reader)?;
            }
            // Call kind tags: no data.
            0x81..=0x84 => {
                tag = reader.read_byte()?;
                continue;
            }
            DR_STOP2 => {
                let _l = reader.read_u32()?;
                tag = reader.read_byte()?;
                continue;
            }
            DR_CONST_ADD_INFO => {
                skip_decl_const_add_info(reader)?;
                tag = reader.read_byte()?;
                continue;
            }
            DR_PROC_ADD_INFO => {
                let _v = reader.read_index()?;
                tag = reader.read_byte()?;
                continue;
            }
            DR_EMBEDDED_PROC_START => {
                let mut dummy_types = Vec::new();
                skip_embedded_proc(reader, &mut dummy_types)?;
                tag = reader.read_byte()?;
                continue;
            }
            DR_EMBEDDED_PROC_END => {
                tag = reader.read_byte()?;
                continue;
            }
            DR_EXPORT => {
                let _name = reader.read_name()?;
                let _idx = reader.read_uindex()?;
            }
            DR_VAR | DR_VAR_C | DR_SPEC_VAR | DR_THREAD_VAR | DR_RES_STR => {
                skip_var_decl(reader)?;
            }
            // Stop tags: end of class member list.
            DR_STOP | DR_STOP1 | DR_STOP_A | DR_CBLOCK | DR_FIXUP => {
                break;
            }
            // Try shared type-def handler; unknown tag stops gracefully.
            _ => match try_skip_type_def(fixed, reader) {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => break,
            },
        }
        tag = reader.read_byte()?;
    }

    Ok((fields, methods))
}
