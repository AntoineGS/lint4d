use crate::{CompilerVersion, ConditionalFact};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// The target platforms whose compiler-defined symbols are modeled here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetPlatform {
    Win32,
    Win64,
    Other(String),
}

impl TargetPlatform {
    /// Parse a platform name, accepting common aliases for Windows targets.
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }

        match trimmed.to_ascii_lowercase().as_str() {
            "win32" | "x86" => Some(Self::Win32),
            "win64" | "x64" => Some(Self::Win64),
            _ => Some(Self::Other(trimmed.to_owned())),
        }
    }

    /// Return the normalized Windows platform name or the retained other name.
    pub fn name(&self) -> &str {
        match self {
            Self::Win32 => "Win32",
            Self::Win64 => "Win64",
            Self::Other(name) => name,
        }
    }
}

/// Return Delphi's compiler-version conditional symbols for `version`.
pub fn ver_symbols(version: CompilerVersion) -> Vec<String> {
    let Some(first_fractional_digit) = first_fractional_digit(version) else {
        return Vec::new();
    };

    let mut symbols = vec![format!("VER{}{first_fractional_digit}", version.major)];
    if version.cmp_numeric(CompilerVersion::new(18, 5)) == Some(Ordering::Equal) {
        symbols.push("VER180".to_owned());
    }
    symbols.sort();
    symbols
}

fn first_fractional_digit(version: CompilerVersion) -> Option<u8> {
    let (mantissa, scale) = version.numeric_parts()?;
    let denominator = 10_u64.checked_pow(u32::from(scale))?;
    let whole = u64::from(version.major).checked_mul(denominator)?;
    let fraction = mantissa.checked_sub(whole)?;
    u8::try_from(fraction.checked_mul(10)? / denominator).ok()
}

/// Return compiler and platform predefined symbols for a target build.
pub fn predefined_defines(
    version: Option<CompilerVersion>,
    platform: Option<&TargetPlatform>,
    console: ConditionalFact,
) -> BTreeMap<String, ConditionalFact> {
    let mut defines = BTreeMap::new();
    let version_is_numeric = version.is_some_and(|version| version.numeric_parts().is_some());
    let current_symbols = version.map(ver_symbols).unwrap_or_default();

    for symbol in HISTORICAL_VER_SYMBOLS {
        let fact = match version {
            Some(_) if version_is_numeric => {
                if current_symbols.iter().any(|current| current == symbol) {
                    ConditionalFact::True
                } else {
                    ConditionalFact::False
                }
            }
            _ => ConditionalFact::Unknown,
        };
        defines.insert((*symbol).to_owned(), fact);
    }
    for symbol in current_symbols {
        defines.insert(
            symbol,
            if version_is_numeric {
                ConditionalFact::True
            } else {
                ConditionalFact::Unknown
            },
        );
    }

    defines.insert(
        "CONDITIONALEXPRESSIONS".to_owned(),
        since_fact(version, Since::At(14, 0), true),
    );
    defines.insert(
        "UNICODE".to_owned(),
        since_fact(version, Since::At(20, 0), true),
    );
    defines.insert("CPPBUILDER".to_owned(), ConditionalFact::False);
    defines.insert("BCB".to_owned(), ConditionalFact::False);
    defines.insert("CONSOLE".to_owned(), console);
    defines.insert(
        "NATIVECODE".to_owned(),
        since_fact(version, Since::ConfirmedFrom(23, 0), true),
    );
    defines.insert(
        "DCC".to_owned(),
        since_fact(version, Since::ConfirmedFrom(23, 0), true),
    );

    for &(symbol, win32, win64, since) in WINDOWS_SYMBOLS {
        let fact = match platform {
            Some(TargetPlatform::Win32) => since_fact(version, since, win32),
            Some(TargetPlatform::Win64) => since_fact(version, since, win64),
            Some(TargetPlatform::Other(_)) | None => ConditionalFact::Unknown,
        };
        defines.insert(symbol.to_owned(), fact);
    }

    for &symbol in NON_WINDOWS_SYMBOLS {
        let fact = match platform {
            Some(TargetPlatform::Win32 | TargetPlatform::Win64) => ConditionalFact::False,
            Some(TargetPlatform::Other(_)) | None => ConditionalFact::Unknown,
        };
        defines.insert(symbol.to_owned(), fact);
    }

    defines
}

#[derive(Debug, Clone, Copy)]
enum Since {
    Always,
    At(u32, u32),
    ConfirmedFrom(u32, u32),
    Unverified,
}

fn since_fact(version: Option<CompilerVersion>, since: Since, defined: bool) -> ConditionalFact {
    match since {
        Since::Always => bool_fact(defined),
        Since::At(major, minor) => version
            .and_then(|version| version.cmp_numeric(CompilerVersion::new(major, minor)))
            .map_or(ConditionalFact::Unknown, |ordering| match ordering {
                Ordering::Less => ConditionalFact::False,
                Ordering::Equal | Ordering::Greater => bool_fact(defined),
            }),
        Since::ConfirmedFrom(major, minor) => {
            match version
                .and_then(|version| version.cmp_numeric(CompilerVersion::new(major, minor)))
            {
                Some(Ordering::Less) | None => ConditionalFact::Unknown,
                Some(Ordering::Equal | Ordering::Greater) => bool_fact(defined),
            }
        }
        Since::Unverified => ConditionalFact::Unknown,
    }
}

fn bool_fact(value: bool) -> ConditionalFact {
    if value {
        ConditionalFact::True
    } else {
        ConditionalFact::False
    }
}

const HISTORICAL_VER_SYMBOLS: &[&str] = &[
    "VER80", "VER90", "VER93", "VER100", "VER110", "VER120", "VER125", "VER130", "VER140",
    "VER150", "VER160", "VER170", "VER180", "VER185", "VER190", "VER200", "VER210", "VER220",
    "VER230", "VER240", "VER250", "VER260", "VER265", "VER270", "VER280", "VER290", "VER300",
    "VER310", "VER320", "VER330", "VER340", "VER350", "VER360", "VER370",
];

// Thresholds cross-checked against third-party Delphi compatibility sources:
// - Spring4D JEDI include, spring4d/Source/jedi.inc:1780: ASSEMBLER appeared in Delphi 7;
//   also treats undefined CPUX86 as a pre-XE2 compiler.
// - Indy Lib/System/IdCompilerDefines.inc: XE2 gates for CPUX86, CPUX64, WIN64,
//   DCC, and NATIVECODE; CPU32BITS/CPU64BITS are gated on VCL_XE8_OR_ABOVE.
// - Indy defines MSWINDOWS manually only below VCL 6; this confirms Delphi 6+.
const WINDOWS_SYMBOLS: &[(&str, bool, bool, Since)] = &[
    ("MSWINDOWS", true, true, Since::At(14, 0)),
    ("WIN32", true, false, Since::Always),
    ("WIN64", false, true, Since::At(23, 0)),
    ("CPU386", true, false, Since::Always),
    ("CPUX86", true, false, Since::At(23, 0)),
    ("CPUX64", false, true, Since::At(23, 0)),
    ("CPU32BITS", true, false, Since::At(29, 0)),
    ("CPU64BITS", false, true, Since::At(29, 0)),
    ("ASSEMBLER", true, true, Since::At(15, 0)),
    ("CPUINTEL", true, true, Since::Unverified),
];

const NON_WINDOWS_SYMBOLS: &[&str] = &[
    "POSIX",
    "POSIX32",
    "POSIX64",
    "LINUX",
    "LINUX32",
    "LINUX64",
    "MACOS",
    "MACOS32",
    "MACOS64",
    "OSX",
    "OSX32",
    "OSX64",
    "IOS",
    "IOS32",
    "IOS64",
    "IOSSIMULATOR",
    "ANDROID",
    "ANDROID32",
    "ANDROID64",
    "CPUARM",
    "CPUARM32",
    "CPUARM64",
    "NEXTGEN",
    "AUTOREFCOUNT",
    "WEAKREF",
    "WEAKINSTREF",
    "EXTERNALLINKER",
    "ELF",
    "PIC",
    "UNDERSCOREIMPORTNAME",
    "ALIGN_STACK",
    "PC_MAPPED_EXCEPTIONS",
];

#[cfg(test)]
mod tests {
    use super::{TargetPlatform, predefined_defines, ver_symbols};
    use crate::{CompilerVersion, ConditionalFact as F};
    use std::collections::BTreeMap;

    fn facts(major: u32, minor: u32, platform: &str) -> BTreeMap<String, F> {
        let target_platform = TargetPlatform::parse(platform).unwrap();
        predefined_defines(
            Some(CompilerVersion::new(major, minor)),
            Some(&target_platform),
            F::False,
        )
    }

    #[test]
    fn ver_symbol_follows_the_compiler_version() {
        assert_eq!(ver_symbols(CompilerVersion::new(21, 0)), ["VER210"]);
        assert_eq!(ver_symbols(CompilerVersion::new(37, 0)), ["VER370"]);
        assert_eq!(
            ver_symbols(CompilerVersion::parse("18.5").unwrap()),
            ["VER180", "VER185"]
        );
        let d2010 = facts(21, 0, "Win32");
        assert_eq!(d2010["VER210"], F::True);
        assert_eq!(d2010["VER350"], F::False);
    }

    #[test]
    fn d2010_win32_has_no_xe2_platform_symbols() {
        let d2010 = facts(21, 0, "Win32");
        assert_eq!(d2010["MSWINDOWS"], F::True);
        assert_eq!(d2010["WIN32"], F::True);
        assert_eq!(d2010["CPU386"], F::True);
        assert_eq!(d2010["ASSEMBLER"], F::True);
        assert_eq!(d2010["UNICODE"], F::True);
        assert_eq!(d2010["CONDITIONALEXPRESSIONS"], F::True);
        assert_eq!(d2010["CPUX86"], F::False);
        assert_eq!(d2010["WIN64"], F::False);
        assert_eq!(d2010["LINUX"], F::False);
    }

    #[test]
    fn win64_on_a_modern_compiler() {
        let d13 = facts(37, 0, "x64");
        assert_eq!(d13["WIN64"], F::True);
        assert_eq!(d13["CPUX64"], F::True);
        assert_eq!(d13["CPU64BITS"], F::True);
        assert_eq!(d13["WIN32"], F::False);
        assert_eq!(d13["CPUX86"], F::False);
        assert_eq!(d13["ANDROID"], F::False);
    }

    #[test]
    fn unmodeled_platforms_leave_the_platform_group_unknown() {
        let linux = facts(37, 0, "Linux64");
        assert_eq!(linux["MSWINDOWS"], F::Unknown);
        assert_eq!(linux["LINUX"], F::Unknown);
        assert_eq!(linux["VER370"], F::True);
    }

    #[test]
    fn platform_parsing_normalizes_legacy_names() {
        assert_eq!(TargetPlatform::parse("x86"), Some(TargetPlatform::Win32));
        assert_eq!(
            TargetPlatform::parse(" WIN64 "),
            Some(TargetPlatform::Win64)
        );
        assert_eq!(TargetPlatform::parse(""), None);
    }

    #[test]
    fn rtl_version_names_are_not_defines() {
        assert!(
            facts(35, 0, "Win32")
                .keys()
                .all(|name| !name.starts_with("RTLVERSION"))
        );
    }

    #[test]
    fn version_facts_with_an_unsupported_numeric_version_are_unknown() {
        let unsupported = CompilerVersion::with_patch(38, 0, 1);
        assert!(ver_symbols(unsupported).is_empty());
        let facts = predefined_defines(Some(unsupported), Some(&TargetPlatform::Win32), F::Unknown);
        assert!(!facts.contains_key("VER380"));
        assert_eq!(facts["VER370"], F::Unknown);
        assert_eq!(facts["CONDITIONALEXPRESSIONS"], F::Unknown);
        assert_eq!(facts["UNICODE"], F::Unknown);
        assert_eq!(facts["NATIVECODE"], F::Unknown);
        assert_eq!(facts["CONSOLE"], F::Unknown);
        assert_eq!(facts["WIN32"], F::True);
    }

    #[test]
    fn cpuintel_stays_unknown_and_dcc_is_confirmed_from_xe2() {
        let win32 = facts(37, 0, "Win32");
        assert_eq!(win32["CPUINTEL"], F::Unknown);
        assert_eq!(facts(21, 0, "Win32")["DCC"], F::Unknown);
        assert_eq!(win32["DCC"], F::True);
    }

    #[test]
    fn windows_symbols_respect_their_first_version() {
        let before_native = facts(15, 0, "Win32");
        let at_native = facts(16, 0, "Win32");
        assert_eq!(before_native["NATIVECODE"], F::Unknown);
        assert_eq!(at_native["NATIVECODE"], F::Unknown);
        assert_eq!(facts(22, 0, "Win64")["WIN64"], F::False);
        assert_eq!(facts(23, 0, "Win64")["WIN64"], F::True);
        assert_eq!(facts(22, 0, "Win32")["DCC"], F::Unknown);
        assert_eq!(facts(22, 0, "Win32")["NATIVECODE"], F::Unknown);
        assert_eq!(facts(23, 0, "Win32")["DCC"], F::True);
        assert_eq!(facts(23, 0, "Win32")["NATIVECODE"], F::True);
        assert_eq!(facts(14, 0, "Win32")["ASSEMBLER"], F::False);
        assert_eq!(facts(15, 0, "Win32")["ASSEMBLER"], F::True);
        assert_eq!(facts(15, 0, "Win64")["ASSEMBLER"], F::True);
    }

    #[test]
    fn known_version_facts_survive_unknown_platform() {
        let facts = predefined_defines(Some(CompilerVersion::new(35, 0)), None, F::Unknown);

        assert_eq!(facts["VER350"], F::True);
        assert_eq!(facts["UNICODE"], F::True);
        assert_eq!(facts["DCC"], F::True);
        assert_eq!(facts["NATIVECODE"], F::True);
        assert_eq!(facts["MSWINDOWS"], F::Unknown);
        assert_eq!(facts["LINUX"], F::Unknown);
    }

    #[test]
    fn known_platform_facts_survive_unknown_version() {
        let facts = predefined_defines(None, Some(&TargetPlatform::Win32), F::False);

        assert_eq!(facts["WIN32"], F::True);
        assert_eq!(facts["LINUX"], F::False);
        assert_eq!(facts["VER350"], F::Unknown);
        assert_eq!(facts["UNICODE"], F::Unknown);
        assert_eq!(facts["CPUX86"], F::Unknown);
    }
}
