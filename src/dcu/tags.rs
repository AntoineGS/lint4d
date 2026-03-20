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

// Declaration tags
pub const DR_VAR: u8 = 0x20;
// Note: 0x22 is shared between DR_THREAD_VAR (top-level) and AR_VAR (method params).
// They are disambiguated by parsing context, not tag value.
pub const DR_THREAD_VAR: u8 = 0x22;
pub const DR_CONST: u8 = 0x25;
pub const DR_TYPE_P: u8 = 0x26;
pub const DR_PROC: u8 = 0x28;
pub const DR_TYPE: u8 = 0x2A;

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

// Type definition tags
pub const DR_RANGE_DEF: u8 = 0x42;
pub const DR_ENUM_DEF: u8 = 0x43;
pub const DR_FLOAT_DEF: u8 = 0x44;
pub const DR_PTR_DEF: u8 = 0x45;
pub const DR_CLASS_DEF: u8 = 0x46;
pub const DR_PROC_TYPE_DEF: u8 = 0x48;
pub const DR_SET_DEF: u8 = 0x4A;
pub const DR_ARRAY_DEF: u8 = 0x4C;
pub const DR_REC_DEF: u8 = 0x4D;
pub const DR_INTERFACE_DEF: u8 = 0x54;

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
