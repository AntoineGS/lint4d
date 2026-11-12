#![cfg_attr(not(test), allow(dead_code))]

mod format;

#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use format::{parse as parse_table_format, write as write_table_format};

use pascal_project::{ConditionalContext, LayoutPlatform};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SurfaceKey {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) platform: SurfacePlatform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SurfacePlatform {
    Win32,
    Win64,
}

impl SurfaceKey {
    /// Return a key only for a numeric compiler version and Windows layout.
    pub(crate) fn from_context(context: &ConditionalContext) -> Option<Self> {
        let version = context.compiler_version?;
        version.numeric_parts()?;

        let platform = match context.layout.platform.as_ref()? {
            LayoutPlatform::Win32 => SurfacePlatform::Win32,
            LayoutPlatform::Win64 => SurfacePlatform::Win64,
            LayoutPlatform::Other(_) => return None,
        };

        Some(Self {
            major: version.major,
            minor: version.minor,
            platform,
        })
    }

    pub(crate) fn file_stem(&self) -> String {
        let platform = match self.platform {
            SurfacePlatform::Win32 => "Win32",
            SurfacePlatform::Win64 => "Win64",
        };
        format!("{}.{}-{platform}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SurfaceTable {
    pub(crate) banner: String,
    pub(crate) script_version: u32,
    pub(crate) system_blake3: String,
    pub(crate) sysinit_blake3: String,
    pub(crate) candidates: usize,
    surface: HashSet<String>,
    defines: BTreeSet<String>,
    members: BTreeMap<String, HashSet<String>>,
}

impl SurfaceTable {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let parsed = parse_table_format(text)?;
        let surface = parsed
            .surface
            .iter()
            .filter_map(|name| format::canonical_name(name))
            .collect();
        let defines = parsed.defines.into_iter().collect();
        let members = parsed
            .members
            .into_iter()
            .map(|(type_name, names)| {
                let members = names
                    .iter()
                    .filter_map(|name| format::canonical_name(name))
                    .collect();
                (type_name.to_ascii_lowercase(), members)
            })
            .collect();

        Ok(Self {
            banner: parsed.banner,
            script_version: parsed.script_version,
            system_blake3: parsed.system_blake3,
            sysinit_blake3: parsed.sysinit_blake3,
            candidates: parsed.candidates,
            surface,
            defines,
            members,
        })
    }

    pub(crate) fn has_name(&self, name: &str) -> bool {
        format::canonical_name(name).is_some_and(|canonical| self.surface.contains(&canonical))
    }

    pub(crate) fn defines(&self) -> impl Iterator<Item = &str> {
        self.defines.iter().map(String::as_str)
    }

    pub(crate) fn is_defined(&self, symbol: &str) -> bool {
        self.defines
            .iter()
            .any(|define| define.eq_ignore_ascii_case(symbol))
    }

    pub(crate) fn members(&self, type_name: &str) -> Option<&HashSet<String>> {
        let canonical = format::canonical_name(type_name)?;
        self.members.get(&canonical)
    }
}

const TABLES: &[(&str, &str)] = &[];

static CHECKED_IN_TABLES: OnceLock<HashMap<String, Arc<SurfaceTable>>> = OnceLock::new();

pub(crate) fn table_for(key: &SurfaceKey) -> Option<Arc<SurfaceTable>> {
    #[cfg(test)]
    if let Some(table) = TEST_TABLES.with(|tables| {
        tables
            .borrow()
            .iter()
            .rev()
            .find(|(installed_key, _)| installed_key == key)
            .map(|(_, table)| Arc::new(table.clone()))
    }) {
        return Some(table);
    }

    let tables = CHECKED_IN_TABLES.get_or_init(|| {
        TABLES
            .iter()
            .filter_map(|(file_stem, text)| {
                SurfaceTable::parse(text)
                    .ok()
                    .map(|table| ((*file_stem).to_owned(), Arc::new(table)))
            })
            .collect()
    });
    tables.get(&key.file_stem()).cloned()
}

#[cfg(test)]
thread_local! {
    static TEST_TABLES: std::cell::RefCell<Vec<(SurfaceKey, SurfaceTable)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(test)]
struct TestTableGuard;

#[cfg(test)]
impl Drop for TestTableGuard {
    fn drop(&mut self) {
        TEST_TABLES.with(|tables| {
            tables.borrow_mut().pop();
        });
    }
}

#[cfg(test)]
pub(crate) fn with_test_table<R>(
    key: SurfaceKey,
    table: SurfaceTable,
    body: impl FnOnce() -> R,
) -> R {
    TEST_TABLES.with(|tables| tables.borrow_mut().push((key, table)));
    let _guard = TestTableGuard;
    body()
}

pub(crate) fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pascal_project::{CompilerVersion, ConditionalContext, LayoutContext, LayoutPlatform};

    const MINIMAL_TABLE: &str = "# dcc-surface v1\n\
# banner: Embarcadero Delphi for Win32 compiler version 37.0\n\
# script-version: 1\n\
# system-blake3: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
# sysinit-blake3: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
# candidates: 3\n\
[surface]\n\
Length\n\
Writeln\n\
[defines]\n\
MSWINDOWS\n\
[members TObject]\n\
ClassName\n\
Free\n\
[members IInterface]\n\
[members IUnknown]\n\
[members TInterfacedObject]\n";

    fn context(
        compiler_version: Option<CompilerVersion>,
        platform: Option<LayoutPlatform>,
    ) -> ConditionalContext {
        ConditionalContext {
            compiler_version,
            layout: LayoutContext {
                platform,
                ..LayoutContext::default()
            },
            ..ConditionalContext::default()
        }
    }

    #[test]
    fn parses_and_queries_a_minimal_table_case_insensitively() {
        let table = SurfaceTable::parse(MINIMAL_TABLE).unwrap();

        assert!(table.has_name("WRITELN"));
        assert!(table.is_defined("mswindows"));
        assert_eq!(table.defines().collect::<Vec<_>>(), ["MSWINDOWS"]);
        assert!(table.members("tobject").unwrap().contains("free"));
    }

    #[test]
    fn format_writer_round_trips_a_parsed_table() {
        let parsed = format::parse(MINIMAL_TABLE).unwrap();
        let rendered = write_table_format(&parsed);

        assert_eq!(format::parse(&rendered).unwrap(), parsed);
    }

    #[test]
    fn rejects_a_missing_header_line_with_its_line_number() {
        let text = MINIMAL_TABLE.replace("# script-version: 1\n", "");

        let error = SurfaceTable::parse(&text).unwrap_err();

        assert!(error.contains("line 3"), "{error}");
    }

    #[test]
    fn rejects_unknown_sections_with_their_line_number() {
        let text = MINIMAL_TABLE.replace("[defines]", "[unknown]");

        let error = SurfaceTable::parse(&text).unwrap_err();

        assert!(error.contains("line 10"), "{error}");
    }

    #[test]
    fn rejects_unsorted_names_with_their_line_number() {
        let text = MINIMAL_TABLE.replace("Length\nWriteln", "Writeln\nLength");

        let error = SurfaceTable::parse(&text).unwrap_err();

        assert!(error.contains("line 9"), "{error}");
    }

    #[test]
    fn rejects_case_insensitive_duplicates() {
        let text = MINIMAL_TABLE.replace(
            "ClassName\nFree\n[members IInterface]",
            "ClassName\nFree\nFREE\n[members IInterface]",
        );

        assert!(SurfaceTable::parse(&text).is_err());
    }

    #[test]
    fn rejects_names_outside_the_identifier_grammar() {
        let text = MINIMAL_TABLE.replace("Length", "1abc");

        assert!(SurfaceTable::parse(&text).is_err());
    }

    #[test]
    fn surface_key_requires_a_numeric_version_and_windows_platform() {
        let version = CompilerVersion::new(37, 0);
        let key =
            SurfaceKey::from_context(&context(Some(version), Some(LayoutPlatform::Win32))).unwrap();

        assert_eq!(key.major, 37);
        assert_eq!(key.minor, 0);
        assert_eq!(key.platform, SurfacePlatform::Win32);
        assert_eq!(key.file_stem(), "37.0-Win32");
        assert!(SurfaceKey::from_context(&context(Some(version), None)).is_none());
        assert!(
            SurfaceKey::from_context(&context(
                Some(version),
                Some(LayoutPlatform::Other("Linux64".to_owned())),
            ))
            .is_none()
        );
        assert!(SurfaceKey::from_context(&context(None, Some(LayoutPlatform::Win32))).is_none());
        assert!(
            SurfaceKey::from_context(&context(
                Some(CompilerVersion::with_patch(37, 0, 1)),
                Some(LayoutPlatform::Win32),
            ))
            .is_none()
        );
    }

    #[test]
    fn blake3_hex_uses_lowercase_digest_encoding() {
        assert_eq!(
            blake3_hex(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn injected_table_takes_precedence_and_unknown_registry_keys_return_none() {
        let key = SurfaceKey {
            major: 37,
            minor: 0,
            platform: SurfacePlatform::Win32,
        };
        let table = SurfaceTable::parse(MINIMAL_TABLE).unwrap();
        let expected = table.clone();

        with_test_table(key.clone(), table, || {
            assert_eq!(table_for(&key).as_deref(), Some(&expected));
        });

        assert!(
            table_for(&SurfaceKey {
                major: 21,
                minor: 0,
                platform: SurfacePlatform::Win64,
            })
            .is_none()
        );
    }

    #[test]
    fn every_checked_in_table_parses() {
        for (_, text) in TABLES {
            SurfaceTable::parse(text).unwrap();
        }
    }
}
