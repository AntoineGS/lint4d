use std::fmt;

#[derive(Debug)]
pub enum DcuError {
    UnexpectedEof { context: &'static str },
    UnsupportedVersion { magic: u32 },
    UnknownTag { tag: u8, offset: usize },
    UnresolvedTypeRef { index: u32 },
    Io(std::io::Error),
}

impl fmt::Display for DcuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { context } => write!(f, "unexpected EOF: {context}"),
            Self::UnsupportedVersion { magic } => write!(f, "unsupported DCU version: magic 0x{magic:08X}"),
            Self::UnknownTag { tag, offset } => write!(f, "unknown tag 0x{tag:02X} at offset {offset}"),
            Self::UnresolvedTypeRef { index } => write!(f, "unresolved type reference: index {index}"),
            Self::Io(e) => write!(f, "IO error: {e}"),
        }
    }
}

impl std::error::Error for DcuError {}

impl From<std::io::Error> for DcuError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// Structural tags
pub const DR_STOP: u8 = 0x00;
pub const DR_STOP_A: u8 = 0x61;
pub const DR_STOP1: u8 = 0x63;
pub const DR_CBLOCK: u8 = 0x6C;
pub const DR_FIXUP: u8 = 0x6D;

// Uses / import tags
pub const DR_UNIT: u8 = 0x64;
pub const DR_UNIT1: u8 = 0x65;
pub const DR_IMP_TYPE: u8 = 0x66;
pub const DR_IMP_VAL: u8 = 0x67;
pub const DR_DLL: u8 = 0x68;
pub const DR_EXPORT: u8 = 0x69;
pub const DR_IMP_TYPE_DEF: u8 = 0x6E;

// Source file tags
pub const DR_SRC: u8 = 0x70;
pub const DR_OBJ: u8 = 0x71;
pub const DR_RES: u8 = 0x72;
pub const DR_ASM: u8 = 0x73;
pub const DR_UNIT_INLINE_SRC: u8 = 0x76;

// Structural tags used within uses clauses
pub const DR_STOP2: u8 = 0x9F;
pub const DR_CONST_ADD_INFO: u8 = 0x9C;

// Declaration tags (fixed values, i.e., after FixTag adjustment)
pub const DR_VAR: u8 = 0x20;
pub const DR_CONST: u8 = 0x25;
pub const DR_TYPE_P: u8 = 0x26;
pub const DR_VAR_C: u8 = 0x27;
pub const DR_PROC: u8 = 0x28;
pub const DR_SYS_PROC: u8 = 0x29;
pub const DR_TYPE: u8 = 0x2A;
pub const DR_THREAD_VAR: u8 = 0x31;
pub const DR_RES_STR: u8 = 0x32;
pub const DR_UNIT_ADD_INFO: u8 = 0x34;
pub const DR_STR_CONST_REC: u8 = 0x35;
pub const DR_SPEC_VAR: u8 = 0x37;

// Class/record member tags (ONLY valid inside class/record member parsing scope)
// AR_VAR (0x22) shares its value with DR_THREAD_VAR — context disambiguates.
pub const AR_VAL: u8 = 0x21;
pub const AR_VAR: u8 = 0x22;
pub const AR_RESULT: u8 = 0x23;
pub const AR_FLD: u8 = 0x2C;
pub const AR_METHOD: u8 = 0x2D;
pub const AR_CONSTR: u8 = 0x2E;
pub const AR_DESTR: u8 = 0x2F;
pub const AR_PROPERTY: u8 = 0x30;

// Additional info tags (declaration list context)
pub const DR_PROC_ADD_INFO: u8 = 0x9E;

// Info / template tags used inside proc declarations
pub const DR_A5_INFO: u8 = 0xA5;
pub const DR_A6_INFO: u8 = 0xA6;

// Type definition tags
pub const DR_VOID: u8 = 0x40;
pub const DR_BOOL_RANGE_DEF: u8 = 0x41;
pub const DR_CH_RANGE_DEF: u8 = 0x42;
pub const DR_ENUM_DEF: u8 = 0x43;
pub const DR_RANGE_DEF: u8 = 0x44;
pub const DR_PTR_DEF: u8 = 0x45;
pub const DR_CLASS_DEF: u8 = 0x46;
pub const DR_OBJ_VMT_DEF: u8 = 0x47;
pub const DR_PROC_TYPE_DEF: u8 = 0x48;
pub const DR_FLOAT_DEF: u8 = 0x49;
pub const DR_SET_DEF: u8 = 0x4A;
pub const DR_SHORT_STR_DEF: u8 = 0x4B;
pub const DR_ARRAY_DEF: u8 = 0x4C;
pub const DR_REC_DEF: u8 = 0x4D;
pub const DR_OBJ_DEF: u8 = 0x4E;
pub const DR_FILE_DEF: u8 = 0x4F;
pub const DR_TEXT_DEF: u8 = 0x50;
pub const DR_WCHAR_RANGE_DEF: u8 = 0x51;
pub const DR_STRING_DEF: u8 = 0x52;
pub const DR_VARIANT_DEF: u8 = 0x53;
pub const DR_INTERFACE_DEF: u8 = 0x54;
pub const DR_WIDE_STR_DEF: u8 = 0x55;
pub const DR_WIDE_RANGE_DEF: u8 = 0x56;
pub const DR_META_CLASS_DEF: u8 = 0x57;

// Embedded / template tags
pub const DR_EMBEDDED_PROC_START: u8 = 0x6A;
pub const DR_EMBEDDED_PROC_END: u8 = 0x6B;
pub const DR_TEMPLATE_ARG_DEF: u8 = 0x59;
pub const DR_TEMPLATE_CALL: u8 = 0x5A;

// Visibility flags
pub const LF_PRIVATE: u8 = 0x00;
pub const LF_PUBLIC: u8 = 0x02;
pub const LF_PROTECTED: u8 = 0x04;
pub const LF_PUBLISHED: u8 = 0x0A;
