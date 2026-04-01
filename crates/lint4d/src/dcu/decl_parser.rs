use crate::dcu::class_parser::{
    parse_class_def, read_namef_fields, read_type_decl, read_type_p_decl, skip_local_decl,
    skip_method_decl, skip_property_decl,
};
use crate::dcu::const_add_info::skip_decl_const_add_info;
use crate::dcu::reader::DcuReader;
use crate::dcu::tags::*;
use crate::dcu::type_defs::try_skip_type_def;
use crate::dcu::types::fix_tag;
use crate::dcu::{DcuVersion, MethodInfo, MethodKind, TypeInfo, TypeKind};

/// Walk the main declaration list, extracting type declarations.
///
/// Reads tags until a stop/structural tag or an unrecognized tag.
/// The `tag` parameter holds the current (already-read) raw tag byte.
pub(crate) fn read_decl_list(
    reader: &mut DcuReader,
    tag: &mut u8,
    in_args: bool,
) -> Result<Vec<TypeInfo>, DcuError> {
    let mut types = Vec::new();
    read_decl_list_into(reader, tag, &mut types, in_args)?;
    Ok(types)
}

/// Inner declaration list reader that collects types into a shared Vec.
/// This allows nested read_decl_list calls (e.g., inside proc args) to
/// associate type definitions with type declarations from outer scopes.
pub(crate) fn read_decl_list_into(
    reader: &mut DcuReader,
    tag: &mut u8,
    types: &mut Vec<TypeInfo>,
    in_args: bool,
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
                skip_local_decl(reader, false, in_args)?;
            }
            // Field declaration (class/record member).
            AR_FLD => {
                skip_local_decl(reader, false, in_args)?;
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
                skip_local_decl(reader, false, in_args)?;
            }
            // Label declaration.
            AR_LABEL => {
                skip_local_decl(reader, false, in_args)?;
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

/// Skip a variable declaration (drVar, drVarC, drSpecVar, drThreadVar).
pub(crate) fn skip_var_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _h_dt = reader.read_uindex()?;
    let _ofs = reader.read_uindex()?;
    Ok(())
}

/// Skip a constant declaration (drConst).
pub(crate) fn skip_const_decl(reader: &mut DcuReader) -> Result<(), DcuError> {
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
pub(crate) fn read_unit_add_info(
    reader: &mut DcuReader,
    types: &mut Vec<TypeInfo>,
) -> Result<(), DcuError> {
    let _name = reader.read_name()?;
    let _nf = read_namef_fields(reader, false)?;
    let _b = reader.read_uindex()?;

    let mut inner_tag = reader.read_byte()?;
    read_decl_list_into(reader, &mut inner_tag, types, false)?;
    Ok(())
}

/// Skip an embedded proc block (drEmbeddedProcStart .. drEmbeddedProcEnd).
/// Passes the shared types Vec so that type definitions inside embedded
/// procs can be associated with type declarations from outer scopes.
pub(crate) fn skip_embedded_proc(
    reader: &mut DcuReader,
    types: &mut Vec<TypeInfo>,
) -> Result<(), DcuError> {
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
        read_decl_list_into(reader, &mut temp_tag, types, false)?;
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
pub(crate) fn skip_proc_decl(
    reader: &mut DcuReader,
    types: &mut Vec<TypeInfo>,
) -> Result<String, DcuError> {
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
        read_decl_list_into(reader, &mut inner_tag, types, true)?;

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
pub(crate) fn skip_a6_def(reader: &mut DcuReader) -> Result<(), DcuError> {
    let cnt = reader.read_uindex()?;
    for _ in 0..cnt {
        let _h_dt = reader.read_uindex()?;
        let _v = reader.read_uindex()?;
    }
    Ok(())
}

/// Associate a procedure declaration with a class type based on naming convention.
/// If the proc name is "ClassName.MethodName", add it as a method of ClassName.
pub(crate) fn associate_proc_with_class(proc_name: &str, types: &mut [TypeInfo]) {
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
