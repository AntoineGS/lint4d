//! Reference-derived Windows scalar/member storage, independent of host types.
//!
//! Evidence chains (full per-expression references in tests/fixtures/compile_time/abi.toml):
//! * Delphi storage/boolean/reference mappings and Win32 ordinal/real alignments:
//!   <https://docwiki.embarcadero.com/RADStudio/Athens/en/Internal_Data_Formats_(Delphi)>.
//! * Delphi-specific MSVC alignment compatibility (Win32 and Win64):
//!   <http://rvelthuis.de/articles/articles-convert.html>, Getting alignment right.
//! * Win64 mapped INT/UINT/FP/POINTER required scalar-member alignments:
//!   <https://learn.microsoft.com/en-us/cpp/build/x64-software-conventions?view=msvc-170>.
//! * Win64 Extended is specifically a Double alias, not an 80-bit x87 value:
//!   <https://docwiki.embarcadero.com/Libraries/Florence/en/System.Extended>.
//!
//! The explicitly Win32-only alignment table is never extrapolated to Win64.
//! No Win64 Real48 alignment is established by the above chain. Rudy's later
//! size table omits Win64 Extended: it is NOT used for Extended storage.
use super::{Fact, StorageLayout, UnknownReason};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutPlatform, LayoutSettings,
};

/// Intrinsic types only. Name/qualification/binding proof is the caller's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    Byte,
    ShortInt,
    AnsiChar,
    Boolean,
    Word,
    SmallInt,
    WideChar,
    Integer,
    LongInt,
    Cardinal,
    LongWord,
    Single,
    Int64,
    UInt64,
    Double,
    Currency,
    Pointer,
    NativeInt,
    NativeUInt,
    DynamicArrayReference,
    Extended,
    Char,
    String,
    ShortString,
    ByteBool,
    WordBool,
    LongBool,
    Real,
    Real48,
    AnsiString,
    UnicodeString,
    WideString,
}

/// Return only fully verified size AND inherent alignment. Unknown settings
/// block only the representations they select; record packing is applied later.
/// Versions admitted here are Delphi 2007 (18.5, Win32) and Delphi 2009 through
/// Florence (20.0..37.0), with Win64 available from XE2 (23.0). Future, fractional
/// non-profile and invalid versions are not assumed ABI-compatible.
pub fn builtin_layout(
    ty: BuiltinType,
    context: &ConditionalContext,
    settings: &LayoutSettings,
) -> Fact<StorageLayout> {
    use BuiltinType::*;
    let platform = context.layout.platform.as_ref();
    if matches!(platform, Some(LayoutPlatform::Other(_))) {
        return Fact::Unknown(UnknownReason::UnsupportedTarget);
    }
    let Some(version) = context.compiler_version else {
        return Fact::Unknown(UnknownReason::UnsupportedVersion);
    };
    let pre_unicode = version == CompilerVersion::new(18, 5);
    let modern = (20..=37).any(|major| version == CompilerVersion::new(major, 0));
    if (!pre_unicode && !modern)
        || (matches!(platform, Some(LayoutPlatform::Win64))
            && version < CompilerVersion::new(23, 0))
        || (pre_unicode && matches!(ty, UnicodeString))
    {
        return Fact::Unknown(UnknownReason::UnsupportedVersion);
    }

    // These literal alignments follow ordinal storage rules or specifically
    // mapped FP32/FP64/INT64/byte-array alignment, never size-as-alignment.
    let (size, alignment) = match ty {
        Byte | ShortInt | AnsiChar | Boolean | ByteBool => (1, 1),
        Word | SmallInt | WideChar | WordBool => (2, 2),
        Integer | Cardinal | LongBool | Single => (4, 4),
        Int64 | UInt64 | Double | Currency => (8, 8),
        // Delphi2007 native integers are64-bit even on Win32;2009 changed
        // them to32-bit. This is a version rule, not pointer-size inference.
        // https://delphidabbler.com/notes/version-features footnote4;
        // https://docs.devart.com/odac/work-rad-studio-xe2.htm native sizes.
        NativeInt | NativeUInt if pre_unicode => (8, 8),
        Char => {
            if pre_unicode {
                (1, 1)
            } else {
                (2, 2)
            }
        }
        ShortString => (256, 1),
        // Windows LongInt/LongWord stay32, unlike some other64-bit targets.
        LongInt | LongWord => match platform {
            Some(LayoutPlatform::Win32 | LayoutPlatform::Win64) => (4, 4),
            _ => return Fact::Unknown(UnknownReason::MissingTarget),
        },
        Pointer
        | NativeInt
        | NativeUInt
        | DynamicArrayReference
        | AnsiString
        | UnicodeString
        | WideString => match platform {
            Some(LayoutPlatform::Win32) => (4, 4),
            Some(LayoutPlatform::Win64) => (8, 8),
            _ => return Fact::Unknown(UnknownReason::MissingTarget),
        },
        Extended => match platform {
            Some(LayoutPlatform::Win32) => (10, 8),
            Some(LayoutPlatform::Win64) => (8, 8),
            _ => return Fact::Unknown(UnknownReason::MissingTarget),
        },
        Real48 => match platform {
            Some(LayoutPlatform::Win32) => (6, 2),
            Some(LayoutPlatform::Win64) => return Fact::Unknown(UnknownReason::UnsupportedLayout),
            _ => return Fact::Unknown(UnknownReason::MissingTarget),
        },
        String => {
            return match settings.long_strings {
                ConditionalFact::True => builtin_layout(
                    if pre_unicode {
                        AnsiString
                    } else {
                        UnicodeString
                    },
                    context,
                    settings,
                ),
                ConditionalFact::False => builtin_layout(ShortString, context, settings),
                ConditionalFact::Unknown => Fact::Unknown(UnknownReason::UnsupportedLayout),
            };
        }
        Real => {
            return match settings.real_compatibility {
                ConditionalFact::True => builtin_layout(Real48, context, settings),
                ConditionalFact::False => builtin_layout(Double, context, settings),
                ConditionalFact::Unknown => Fact::Unknown(UnknownReason::UnsupportedLayout),
            };
        }
    };
    Fact::Known(StorageLayout { size, alignment })
}
