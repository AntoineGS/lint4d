use crate::dcu::const_add_info::skip_decl_const_add_info;
use crate::dcu::header::parse_unit_header;
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;
use crate::dcu::{
    DcuUnit, DcuVersion, FieldInfo, MethodInfo, MethodKind, TypeInfo, TypeKind, TypeRef, Visibility,
};

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
    match read_decl_list_into(&mut reader, &mut tag, &mut types) {
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
fn fix_tag(raw: u8) -> u8 {
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

/// Try to skip a type definition tag. Returns Ok(true) if handled, Ok(false) if not recognized.
fn try_skip_type_def(tag: u8, reader: &mut DcuReader) -> Result<bool, DcuError> {
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

/// Walk the main declaration list, extracting type declarations.
///
/// Reads tags until a stop/structural tag or an unrecognized tag.
/// The `tag` parameter holds the current (already-read) raw tag byte.
fn read_decl_list(reader: &mut DcuReader, tag: &mut u8) -> Result<Vec<TypeInfo>, DcuError> {
    let mut types = Vec::new();
    read_decl_list_into(reader, tag, &mut types)?;
    Ok(types)
}

/// Inner declaration list reader that collects types into a shared Vec.
/// This allows nested read_decl_list calls (e.g., inside proc args) to
/// associate type definitions with type declarations from outer scopes.
fn read_decl_list_into(
    reader: &mut DcuReader,
    tag: &mut u8,
    types: &mut Vec<TypeInfo>,
) -> Result<(), DcuError> {
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
                read_unit_add_info(reader, types)?;
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
            DR_STOP | DR_STOP_A => {
                break;
            }
            // drCBlock: data block. Read size + data, then continue.
            DR_CBLOCK => {
                let data_bl_size = reader.read_uindex()?;
                reader.skip(data_bl_size as usize)?;
                *tag = reader.read_byte()?;
                continue;
            }
            // drFixUp: fixup table. Skip until end of file or stop.
            DR_FIXUP => {
                break;
            }
            // drStop1: end of nested list.
            DR_STOP1 => {
                break;
            }
            // drEmbeddedProcStart: skip the entire embedded proc block.
            DR_EMBEDDED_PROC_START => {
                skip_embedded_proc(reader, types)?;
                *tag = reader.read_byte()?;
                continue;
            }
            // drEmbeddedProcEnd: can appear in arg lists (D2009+).
            DR_EMBEDDED_PROC_END => {
                *tag = reader.read_byte()?;
                continue;
            }
            // drProc: procedure declaration -- complex, try to skip it.
            // If the name is "ClassName.MethodName", associate it as a
            // method of the corresponding class type.
            DR_PROC => {
                let save_pos = reader.position();
                match skip_proc_decl(reader, types) {
                    Ok(proc_name) => {
                        associate_proc_with_class(&proc_name, types);
                    }
                    Err(_e) => {
                        reader.set_position(save_pos);
                        break;
                    }
                }
            }
            // drExport: TNameDecl + index.
            DR_EXPORT => {
                let _name = reader.read_name()?;
                let _idx = reader.read_uindex()?;
            }
            // Local variable / parameter tags (appear in proc arg lists, class bodies).
            AR_VAL | AR_VAR | AR_RESULT | AR_ABS_LOC_VAR => {
                skip_local_decl(reader, false)?;
            }
            // Field declaration (class/record member).
            AR_FLD => {
                skip_local_decl(reader, false)?;
            }
            // Method / constructor / destructor (class member).
            AR_METHOD | AR_CONSTR | AR_DESTR => {
                skip_method_decl(reader, fixed)?;
            }
            // Property declaration.
            AR_PROPERTY => {
                skip_property_decl(reader)?;
            }
            // Class variable (D2006+).
            AR_CLASS_VAR => {
                skip_local_decl(reader, false)?;
            }
            // Label declaration.
            AR_LABEL => {
                skip_local_decl(reader, false)?;
            }
            // arSetDeft: default set value.
            AR_SET_DEFT => {
                let _v = reader.read_u32()?;
            }
            // arCopyDecl: copy of a declaration from parent.
            AR_COPY_DECL => {
                let _name = reader.read_name()?;
                let _nf = read_namef_fields(reader, true)?;
                let _src = reader.read_uindex()?;
            }
            // Class definition: parse and associate with the last type declaration.
            DR_CLASS_DEF => {
                let members = parse_class_def(reader)?;
                // Associate parsed members with the most recently declared type.
                if let Some(last_type) = types.last_mut() {
                    last_type.kind = TypeKind::Class;
                    last_type.fields = members.0;
                    last_type.methods = members.1;
                }
            }
            // Try shared type-def handler for all other type definition tags.
            _ if try_skip_type_def(fixed, reader)? => {}
            // drSysProc: D8+
            DR_SYS_PROC => {
                let _name = reader.read_name()?;
                let _nf = read_namef_fields(reader, false)?;
                let _v = reader.read_uindex()?;
            }
            // drStrConstRec: D8+
            DR_STR_CONST_REC => {
                let _name = reader.read_name()?;
                let _nf = read_namef_fields(reader, false)?;
                let _h_dt = reader.read_uindex()?;
                let _ofs = reader.read_uindex()?;
                let _v = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            // Unknown or unhandled tag: stop gracefully.
            _ => {
                break;
            }
        }
        *tag = reader.read_byte()?;
    }

    Ok(())
}

/// Read a drType declaration (type declaration).
/// Returns Some(TypeInfo) for user-visible types (not starting with '.' or ':').
///
/// Field layout (D13):
///   Name (ReadName)
///   TNameFDecl: F, F1, F4, [Inf], [B2]
///   hDef (ReadUIndex)
fn read_type_decl(reader: &mut DcuReader) -> Result<Option<TypeInfo>, DcuError> {
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
fn read_type_p_decl(reader: &mut DcuReader) -> Result<Option<TypeInfo>, DcuError> {
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

/// Indicates an Inf field is present in TNameFDecl.
const NF_INF: u32 = 0x40;
/// Indicates a B2 field is present (D8+ LocFlagsX bit).
const NF_B2: u32 = 0x80;

/// Read TNameFDecl fields: F, F1, F4, optionally Inf and B2.
struct NameFFields {
    pub _f: u32,
    pub _f1: u32,
}

fn read_namef_fields(reader: &mut DcuReader, no_inf: bool) -> Result<NameFFields, DcuError> {
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

/// Read a drUnitAddInfo record and its nested declaration list.
/// Types found inside are added to the shared types Vec.
fn read_unit_add_info(reader: &mut DcuReader, types: &mut Vec<TypeInfo>) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _b = reader.read_uindex()?;

    let mut inner_tag = reader.read_byte()?;
    read_decl_list_into(reader, &mut inner_tag, types)?;
    Ok(())
}

/// Skip an embedded proc block (drEmbeddedProcStart .. drEmbeddedProcEnd).
/// Passes the shared types Vec so that type definitions inside embedded
/// procs can be associated with type declarations from outer scopes.
fn skip_embedded_proc(reader: &mut DcuReader, types: &mut Vec<TypeInfo>) -> Result<(), DcuError> {
    let mut inner_tag = reader.read_byte()?;
    loop {
        let fixed = fix_tag(inner_tag);
        if fixed == DR_EMBEDDED_PROC_END {
            break;
        }
        if fixed == DR_EMBEDDED_PROC_START {
            skip_embedded_proc(reader, types)?;
            inner_tag = reader.read_byte()?;
            continue;
        }
        let mut temp_tag = inner_tag;
        read_decl_list_into(reader, &mut temp_tag, types)?;
        if fix_tag(temp_tag) == DR_EMBEDDED_PROC_END {
            break;
        }
        inner_tag = reader.read_byte()?;
    }
    Ok(())
}

/// Skip a procedure declaration (drProc), returning the proc name.
/// The `types` parameter allows type definitions found inside proc bodies
/// to be associated with type declarations from the outer scope.
fn skip_proc_decl(reader: &mut DcuReader, types: &mut Vec<TypeInfo>) -> Result<String, DcuError> {
    let name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;

    let _b0 = reader.read_uindex()?;
    let _sz = reader.read_uindex()?;
    if reader.ver >= DcuVersion::DXE {
        let _xe_byte = reader.read_byte()?;
    }

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
        // Pass the shared types list so nested class defs link to outer types.
        read_decl_list_into(reader, &mut inner_tag, types)?;

        if fix_tag(inner_tag) != DR_STOP1 {
            return Err(DcuError::UnknownTag {
                tag: inner_tag,
                offset: reader.position() - 1,
            });
        }
    }

    Ok(name)
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

/// Associate a procedure declaration with a class type based on naming convention.
/// If the proc name is "ClassName.MethodName", add it as a method of ClassName.
fn associate_proc_with_class(proc_name: &str, types: &mut [TypeInfo]) {
    if let Some(dot_pos) = proc_name.find('.') {
        let class_name = &proc_name[..dot_pos];
        let method_name = &proc_name[dot_pos + 1..];
        if !method_name.is_empty() && !method_name.starts_with(':') {
            if let Some(ti) = types
                .iter_mut()
                .find(|t| t.name.eq_ignore_ascii_case(class_name))
            {
                ti.kind = TypeKind::Class;
                let kind = if method_name == "Create" {
                    MethodKind::Constructor
                } else if method_name == "Destroy" {
                    MethodKind::Destructor
                } else {
                    MethodKind::Procedure
                };
                ti.methods.push(MethodInfo {
                    name: method_name.to_string(),
                    kind,
                    params: Vec::new(),
                    return_type: None,
                });
            }
        }
    }
}

// --- Local / member declaration helpers ---

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
fn skip_local_decl(reader: &mut DcuReader, is_method: bool) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _loc_flags = reader.read_uindex()?; // LocFlags
    let _loc_flags_x = reader.read_uindex()?; // LocFlagsX (D8+)
    let _extra = reader.read_uindex()?; // D2009+
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
fn read_field_decl(reader: &mut DcuReader) -> Result<(String, u32), DcuError> {
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
fn visibility_from_flags(flags: u32) -> Visibility {
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
fn skip_method_decl(reader: &mut DcuReader, method_tag: u8) -> Result<(), DcuError> {
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

/// Skip the version-dependent extra bytes after a method declaration.
/// For D13 (XE7+), reads bytes while they match a known set of values.
fn skip_method_extra_bytes(reader: &mut DcuReader) -> Result<(), DcuError> {
    // The set of acceptable bytes for D_XE7+ (nSkip=5 in DCU32):
    // cS20 = [0,1,2,4,8,9,$10,$18,$20,$22,$28,$38,$42,$47,$4F,$60,$80,$84,
    //          Ord(' ')=$20, Ord('!')=$21, Ord('a')=$61]
    // Combined and deduplicated:
    const METHOD_BYTE_SET: &[u8] = &[
        0x00, 0x01, 0x02, 0x04, 0x08, 0x09, 0x10, 0x18, 0x20, 0x21, 0x22, 0x28, 0x38, 0x42, 0x47,
        0x4F, 0x60, 0x61, 0x80, 0x84,
    ];

    loop {
        let b = reader.read_byte()?;
        if !METHOD_BYTE_SET.contains(&b) {
            // Put back the byte that didn't match.
            reader.unread(1);
            break;
        }
    }
    Ok(())
}

/// Read a method declaration, returning the name and kind.
fn read_method_info(
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
fn skip_property_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
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

// --- Type definition skippers ---
// Skip Rec entries (type definitions) in the declaration list.

/// Read the common TTypeDef header (D13): RTTISz, Sz, hAddrDef, X.
/// DCU32 reference: TTypeDef.Create reads exactly 4 values.
/// Note: Sz is ReadIndex (signed) in DCU32, but read_uindex works for
/// non-negative sizes since the encoding is compatible.
fn read_type_def_header(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _rtti_sz = reader.read_uindex()?;
    let _sz = reader.read_index()?; // Signed in DCU32 (ReadIndex)
    let _h_addr_def = reader.read_uindex()?;
    let _x = reader.read_uindex()?; // D2005+ extra field
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

/// Skip an object VMT definition (DR_OBJ_VMT_DEF).
/// DCU32 reference: TObjVMTDef.Create.
fn skip_obj_vmt_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _h_obj_dt = reader.read_uindex()?;
    let _vmt_sz = reader.read_uindex()?;
    Ok(())
}

/// Skip an object definition (DR_OBJ_DEF).
/// DCU32 reference: TObjDef.Create — inherits from TRecDef.
fn skip_obj_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    // Same structure as TRecDef for D13
    skip_rec_def(reader)
}

/// Skip a metaclass definition (DR_META_CLASS_DEF).
/// DCU32 reference: TMetaClassDef.Create — inherits from TClassDef.
fn skip_meta_class_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    // TMetaClassDef inherits TClassDef. Its Create reads:
    //   inherited Create (= TClassDef.Create)
    //   hCl = ReadUIndex
    let _class_members = parse_class_def(reader)?;
    let _h_cl = reader.read_uindex()?;
    Ok(())
}

/// Parse a class definition, extracting fields and methods.
///
/// Returns (fields, methods) collected from the class member sub-list.
/// DCU32 reference: TClassDef.Create (for D13/XE7+).
fn parse_class_def(reader: &mut DcuReader) -> Result<(Vec<FieldInfo>, Vec<MethodInfo>), DcuError> {
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
            AR_CLASS_VAR => match skip_local_decl(reader, false) {
                Ok(()) => {}
                Err(_) => break,
            },
            AR_VAL | AR_VAR | AR_RESULT | AR_ABS_LOC_VAR | AR_LABEL => {
                match skip_local_decl(reader, false) {
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

/// Skip a record definition (DR_REC_DEF).
/// DCU32 reference: TRecDef.Create (for D13/D_XE2+).
fn skip_rec_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    // D2005+: extra bytes and uindex fields
    let _b2 = reader.read_byte()?;
    let _b1 = reader.read_byte()?; // D2006+
    let _x0 = reader.read_byte()?; // D_XE2+ reads a byte (not uindex)
    let _x = reader.read_uindex()?; // D2005+
                                    // D2009+:
    let _d1 = reader.read_uindex()?;
    let _d2 = reader.read_uindex()?;
    // D2010+:
    let _d3 = reader.read_uindex()?;
    // Read fields
    let mut inner_tag = reader.read_byte()?;
    let _members = read_decl_list(reader, &mut inner_tag)?;
    Ok(())
}

/// Skip an interface definition (DR_INTERFACE_DEF).
/// DCU32 reference: TInterfaceDef.Create (for D13).
fn skip_interface_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    read_type_def_header(reader)?;
    let _bx = reader.read_byte()?; // D2009+
    let _h_parent = reader.read_uindex()?;
    let _vm_cnt = reader.read_index()?; // ReadIndex (signed)
    reader.skip(16)?; // GUID (16 bytes)
    let _b = reader.read_byte()?;
    // D2010+:
    let _by = reader.read_uindex()?;
    let cnt = reader.read_uindex()?;
    for _ in 0..cnt {
        let _x1 = reader.read_uindex()?;
        let _x2 = reader.read_uindex()?;
    }
    // Read fields
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
