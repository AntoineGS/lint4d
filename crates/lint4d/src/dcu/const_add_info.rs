//! ConstAddInfo sub-record parsing for DCU files.

use crate::dcu::reader::DcuReader;
use crate::dcu::tags::DcuError;

// --- ConstAddInfo tag 0x01 flags ---
/// cafInlinePointer: D2006+ inline pointer present.
const CAF_INLINE_PTR: u32 = 0x0100_0000;
/// cafUsedCl: D2009+ attribute class reference present.
const CAF_USED_CL: u32 = 0x0080_0000;
/// cafDeprecated: deprecated message present.
const CAF_DEPRECATED: u32 = 0x01;
/// cafAttributes: D_XE6+ attribute list present.
const CAF_ATTRIBUTES: u32 = 0x8000_0000;
/// cafInline: D2009+ inline data present.
const CAF_INLINE: u32 = 0x0004_0000;
/// cafBigVal: D2005+ big value pointer present.
const CAF_BIG_VAL: u32 = 0x0008_0000;

/// Skip a drConstAddInfo record in the declaration list context.
/// Uses a tag-based sub-protocol; for D2009+ the stop marker is 0xFF.
///
/// DCU32 reference: TUnit.ReadConstAddInfo
/// For D13 (>= verD2009, >= verD_XE6): caiStop = 0xFF.
pub(crate) fn skip_decl_const_add_info(reader: &mut DcuReader) -> Result<(), DcuError> {
    loop {
        let sub_tag = reader.read_byte()?;
        if sub_tag == 0xFF {
            break;
        }
        match sub_tag {
            0x01 => {
                skip_cai_tag_01(reader)?;
            }
            0x02 => {
                // ReadNDXStr: length-prefixed string
                let _msg = reader.read_name()?;
            }
            0x03 => {
                let _v = reader.read_uindex()?;
            }
            0x04 => {
                // D2006+: two uindex values
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            0x05 => {
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            0x06 => {
                // Result, hDT, V, hDef1
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
                let _v4 = reader.read_uindex()?;
            }
            0x07 => {
                // Result, hDef1, hDef2, V
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
                let _v4 = reader.read_uindex()?;
            }
            0x08 => {
                // Result=ReadUIndex, V=ReadUIndex, SkipBlock(V)
                let _result = reader.read_uindex()?;
                let v = reader.read_uindex()?;
                reader.skip(v as usize)?;
            }
            0x09 => {
                // Result, hDT
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            0x0A => {
                skip_cai_tag_0a(reader)?;
            }
            0x0B => {
                let _v = reader.read_uindex()?;
            }
            0x0C => {
                // Result, V1, V2
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
            }
            0x0D => {
                // Result, ReadNDXStr
                let _v = reader.read_uindex()?;
                let _s = reader.read_name()?;
            }
            0x0E => {
                let _v = reader.read_uindex()?;
            }
            0x10 => {
                // D2009+: V1, V2, V3
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
            }
            0x11 => {
                // D_XE4+: V1, V2
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            0x12 => {
                // D2009+: V1, V2
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
            }
            0x13 => {
                // D2009+: V1, V2, V3, 3 strings
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
                let _s1 = reader.read_name()?;
                let _s2 = reader.read_name()?;
                let _s3 = reader.read_name()?;
            }
            0x14 => {
                // D2009+: V1, V2, then list of (V, V, V, name)
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let n = reader.read_uindex()?;
                for _ in 0..n {
                    let _a = reader.read_uindex()?;
                    let _b = reader.read_uindex()?;
                    let _c = reader.read_uindex()?;
                    let _s = reader.read_name()?;
                }
            }
            0x15 => {
                // D_XE2+: V, V1, V2
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
            }
            0x16 => {
                // Result, V, SkipBlock(V)
                let _result = reader.read_uindex()?;
                let v = reader.read_uindex()?;
                reader.skip(v as usize)?;
            }
            0x17 => {
                // D12+: Result, V, V1, V2
                let _v1 = reader.read_uindex()?;
                let _v2 = reader.read_uindex()?;
                let _v3 = reader.read_uindex()?;
                let _v4 = reader.read_uindex()?;
            }
            _ => break,
        }
    }
    Ok(())
}

/// Skip ConstAddInfo sub-tag 0x01, which is the most complex sub-record.
///
/// DCU32 reference: ReadConstAddInfo tag $01 case for D13 (>= verD2009, >= verD_XE6).
fn skip_cai_tag_01(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _h_def = reader.read_uindex()?;
    let f = reader.read_uindex()?;

    // D2006+: inline pointer
    if (f & CAF_INLINE_PTR) != 0 {
        let _ip = reader.read_uindex()?;
    }

    // D2009+: hUsedCl (attribute class)
    if (f & CAF_USED_CL) != 0 {
        let _ip2 = reader.read_uindex()?;
    }

    // D2009+: deprecated message (if F & cafDeprecated)
    if (f & CAF_DEPRECATED) != 0 {
        // ReadNDXStrRef: reads a uindex (string reference)
        let _depr_msg = reader.read_uindex()?;
    }

    // D_XE6+: attributes (if F & cafAttributes)
    if (f & CAF_ATTRIBUTES) != 0 {
        let n = reader.read_uindex()?;
        for _ in 0..n {
            skip_attribute_record(reader)?;
        }
    }

    // D2009+ inline check
    if (f & CAF_INLINE) != 0 {
        skip_cai_inline_data(reader)?;
    }

    // D2005+: big value pointer
    if (f & CAF_BIG_VAL) != 0 {
        let _ip = reader.read_uindex()?;
    }

    Ok(())
}

/// Skip ConstAddInfo sub-tag 0x0A: complex flag-based record.
///
/// DCU32 reference: ReadConstAddInfo tag $0A for D13.
fn skip_cai_tag_0a(reader: &mut DcuReader) -> Result<(), DcuError> {
    let _result = reader.read_uindex()?;
    let _v = reader.read_uindex()?;
    let f = reader.read_uindex()?;

    if (f & 0x01) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x02) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x04) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x08) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x10) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x20) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x40) != 0 {
        // TExtraArgsDeclModifier.Read: len, then len * (name, uindex, uindex, uindex)
        let len = reader.read_uindex()?;
        for _ in 0..len {
            let _s = reader.read_name()?;
            let _a = reader.read_uindex()?;
            let _b = reader.read_uindex()?;
            let _c = reader.read_uindex()?;
        }
    }
    if (f & 0x80) != 0 {
        let _v1 = reader.read_uindex()?;
        let _v2 = reader.read_uindex()?;
        let _v3 = reader.read_uindex()?;
    }
    if (f & 0x100) != 0 {
        // TGeneratedNameDeclModifier: ReadNDXStrRef
        let _v = reader.read_uindex()?;
    }
    if (f & 0x200) != 0 {
        let _s = reader.read_name()?;
    }
    if (f & 0x400) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x800) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x1000) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x2000) != 0 {
        let _v = reader.read_uindex()?;
    }
    if (f & 0x4000) != 0 {
        let _v = reader.read_uindex()?;
    }

    Ok(())
}

/// Skip the inline data structure in ConstAddInfo tag 0x01.
///
/// DCU32 reference: the cafInline branch in ReadConstAddInfo.
fn skip_cai_inline_data(reader: &mut DcuReader) -> Result<(), DcuError> {
    // D2006+: two uindexes
    let _v1 = reader.read_uindex()?;
    let _v2 = reader.read_uindex()?;

    // Len bytes to skip
    let len = reader.read_uindex()?;
    reader.skip(len as usize)?;

    // 5 uindexes
    for _ in 0..5 {
        let _v = reader.read_uindex()?;
    }
    // D_XE2+: extra uindex
    let _v = reader.read_uindex()?;
    // Another uindex
    let _v = reader.read_uindex()?;

    // Len1: entries
    let len1 = reader.read_uindex()?;

    // D2009+: extra reads
    let _v = reader.read_uindex()?;
    let _v = reader.read_uindex()?;
    let len2 = reader.read_uindex()?;
    reader.skip((len2 as usize) * 4)?; // SkipBlock(Len1 * SizeOf(LongInt))

    for _ in 0..len1 {
        let _v = reader.read_uindex()?;
        // D2009+:
        let _r = reader.read_uindex()?;
        let _r2 = reader.read_uindex()?;
        let _v2 = reader.read_uindex()?;
        let z = reader.read_uindex()?;
        // D2010+: extra
        let _r3 = reader.read_uindex()?;
        if z != 0 {
            let _r4 = reader.read_uindex()?;
        }
    }

    // Second list
    let len3 = reader.read_uindex()?;
    for _ in 0..len3 {
        let v = reader.read_uindex()?;
        // D2009+ value dispatch
        let count = match v {
            1 => {
                let _v = reader.read_uindex()?;
                // D_XE+: RefAddrDef. For D13 (>= XE), count = 2
                2
            }
            2 => 1,
            3 => 3,
            4 => 2,
            5 => 4,
            6 => 1,
            _ => 0,
        };
        for _ in 0..count {
            let _v = reader.read_uindex()?;
        }
    }

    // Unit references list
    let len4 = reader.read_uindex()?;
    for _ in 0..len4 {
        let _h_unit = reader.read_uindex()?;
        let cnt = reader.read_uindex()?;
        for _ in 0..cnt {
            let _v = reader.read_uindex()?;
        }
    }

    // D2006+: additional list + D2009 extra
    {
        let len5 = reader.read_uindex()?;
        for _ in 0..len5 {
            let _v = reader.read_uindex()?;
        }
        // D2009+: three more uindexes
        let _v1 = reader.read_uindex()?;
        let _v2 = reader.read_uindex()?;
        let _v3 = reader.read_uindex()?;
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
