#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchState {
    On,
    Off,
    #[allow(dead_code)] // Consumed by the directive filter in the next stage.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalSwitch {
    Warnings,
    Hints,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeSwitch {
    pub(crate) code: &'static str,
    pub(crate) default: SwitchState,
    pub(crate) global: GlobalSwitch,
    pub(crate) id: Option<String>,
    pub(crate) error_capable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitchTable {
    codes: BTreeMap<&'static str, CodeSwitch>,
    properties: BTreeMap<String, String>,
}

impl SwitchTable {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let lines: Vec<_> = text.lines().collect();
        let mut cursor = 0;

        expect_line(&lines, &mut cursor, "# dcc-switches v1")?;
        let banner = header_value(&lines, &mut cursor, "# banner: ")?;
        if banner.trim().is_empty() {
            return Err(line_error(cursor, "banner must not be empty"));
        }
        let script_version = header_value(&lines, &mut cursor, "# script-version: ")?;
        if script_version != "1" {
            return Err(line_error(
                cursor,
                &format!("unsupported script version {script_version:?}"),
            ));
        }

        expect_line(&lines, &mut cursor, "[codes]")?;
        let mut codes = BTreeMap::new();
        let mut previous_code = None;
        while let Some(line) = lines.get(cursor).copied() {
            if line.starts_with('[') {
                break;
            }
            let line_number = cursor + 1;
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            if fields.len() != 5 {
                return Err(line_error(
                    line_number,
                    "code row must have exactly five fields",
                ));
            }

            let code = known_code(fields[0])
                .ok_or_else(|| line_error(line_number, &format!("unknown code {:?}", fields[0])))?;
            if let Some(previous) = previous_code
                && code <= previous
            {
                return Err(line_error(
                    line_number,
                    &format!("code {code:?} is not sorted uniquely"),
                ));
            }
            previous_code = Some(code);

            let default = match fields[1] {
                "ON" => SwitchState::On,
                "OFF" => SwitchState::Off,
                value => {
                    return Err(line_error(
                        line_number,
                        &format!("invalid default state {value:?}"),
                    ));
                }
            };
            let global = match fields[2] {
                "WARNINGS" => GlobalSwitch::Warnings,
                "HINTS" => GlobalSwitch::Hints,
                value => {
                    return Err(line_error(
                        line_number,
                        &format!("invalid global switch {value:?}"),
                    ));
                }
            };
            let id = if fields[3] == "-" {
                None
            } else if is_warning_id(fields[3]) {
                Some(fields[3].to_owned())
            } else {
                return Err(line_error(
                    line_number,
                    &format!("invalid warning identifier {:?}", fields[3]),
                ));
            };
            let error_capable = match fields[4] {
                "yes" => true,
                "no" => false,
                value => {
                    return Err(line_error(
                        line_number,
                        &format!("invalid error-capable value {value:?}"),
                    ));
                }
            };
            codes.insert(
                code,
                CodeSwitch {
                    code,
                    default,
                    global,
                    id,
                    error_capable,
                },
            );
            cursor += 1;
        }
        if codes.is_empty() {
            return Err(line_error(
                cursor + 1,
                "[codes] must contain at least one code",
            ));
        }

        expect_line(&lines, &mut cursor, "[properties]")?;
        let mut properties = BTreeMap::new();
        let mut previous_property: Option<String> = None;
        while let Some(line) = lines.get(cursor).copied() {
            let line_number = cursor + 1;
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            if fields.len() != 2 {
                return Err(line_error(
                    line_number,
                    "property row must have exactly two fields",
                ));
            }
            let Some(suffix) = fields[0].strip_prefix("DCC_") else {
                return Err(line_error(
                    line_number,
                    &format!("property {:?} must start with DCC_", fields[0]),
                ));
            };
            if !is_identifier(suffix) || !is_identifier(fields[1]) {
                return Err(line_error(
                    line_number,
                    "invalid property or task attribute name",
                ));
            }
            let property_key = fields[0].to_ascii_lowercase();
            match previous_property.as_deref() {
                Some(previous) if property_key.as_str() <= previous => {
                    return Err(line_error(
                        line_number,
                        &format!("property {:?} is not sorted uniquely", fields[0]),
                    ));
                }
                _ => {}
            }
            previous_property = Some(property_key.clone());
            properties.insert(property_key, fields[1].to_owned());
            cursor += 1;
        }
        if properties.is_empty() {
            return Err(line_error(
                cursor + 1,
                "[properties] must contain at least one property",
            ));
        }

        if cursor < lines.len() {
            return Err(line_error(
                cursor + 1,
                "unexpected content after [properties]",
            ));
        }

        Ok(Self { codes, properties })
    }

    pub(crate) fn consensus(tables: &[&Self]) -> Option<Self> {
        let first = *tables.first()?;
        let codes = first
            .codes
            .iter()
            .filter_map(|(code, entry)| {
                let agrees = tables.iter().skip(1).all(|table| {
                    table.codes.get(code).is_some_and(|other| {
                        other.default == entry.default
                            && other.global == entry.global
                            && other.id == entry.id
                            && other.error_capable == entry.error_capable
                    })
                });
                agrees.then_some((*code, entry.clone()))
            })
            .collect();
        let properties = first
            .properties
            .iter()
            .filter_map(|(property, attribute)| {
                let agrees = tables
                    .iter()
                    .skip(1)
                    .all(|table| table.properties.get(property) == Some(attribute));
                agrees.then_some((property.clone(), attribute.clone()))
            })
            .collect();
        Some(Self { codes, properties })
    }

    pub(crate) fn code(&self, code: &str) -> Option<&CodeSwitch> {
        self.codes.get(code.to_ascii_uppercase().as_str())
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &CodeSwitch> {
        self.codes.values()
    }

    pub(crate) fn id_for_property(&self, property: &str) -> Option<&str> {
        self.properties
            .get(&property.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub(crate) fn warning_id_for_property(&self, property: &str) -> Option<&str> {
        let id = self.id_for_property(property)?;
        self.codes
            .values()
            .any(|switch| switch.id.as_deref() == Some(id))
            .then_some(id)
    }
}

const SUPPORTED_CODES: &[&str] = &[
    "H2219", "H2443", "W1000", "W1001", "W1002", "W1003", "W1004", "W1005", "W1006", "W1007",
    "W1009", "W1010",
];

fn known_code(code: &str) -> Option<&'static str> {
    SUPPORTED_CODES.iter().copied().find(|known| *known == code)
}

fn is_warning_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn is_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn expect_line(lines: &[&str], cursor: &mut usize, expected: &str) -> Result<(), String> {
    match lines.get(*cursor) {
        Some(line) if *line == expected => {
            *cursor += 1;
            Ok(())
        }
        Some(line) => Err(line_error(
            *cursor + 1,
            &format!("expected {expected:?}, found {line:?}"),
        )),
        None => Err(line_error(
            *cursor + 1,
            &format!("expected {expected:?}, found end of file"),
        )),
    }
}

fn header_value(lines: &[&str], cursor: &mut usize, prefix: &str) -> Result<String, String> {
    let line_number = *cursor + 1;
    match lines
        .get(*cursor)
        .and_then(|line| line.strip_prefix(prefix))
    {
        Some(value) => {
            *cursor += 1;
            Ok(value.to_owned())
        }
        None => Err(line_error(
            line_number,
            &format!("expected header beginning with {prefix:?}"),
        )),
    }
}

fn line_error(line_number: usize, message: &str) -> String {
    format!("line {line_number}: {message}")
}

const CHECKED_IN_TABLES: &[((u32, u32), &str)] = &[
    ((21, 0), include_str!("dcc_switches/21.0.txt")),
    ((24, 0), include_str!("dcc_switches/24.0.txt")),
    ((37, 0), include_str!("dcc_switches/37.0.txt")),
];

static PARSED_TABLES: OnceLock<BTreeMap<(u32, u32), Arc<SwitchTable>>> = OnceLock::new();
static CONSENSUS_TABLE: OnceLock<Option<Arc<SwitchTable>>> = OnceLock::new();

fn parsed_tables() -> &'static BTreeMap<(u32, u32), Arc<SwitchTable>> {
    PARSED_TABLES.get_or_init(|| {
        CHECKED_IN_TABLES
            .iter()
            .filter_map(|(version, text)| {
                SwitchTable::parse(text)
                    .ok()
                    .map(|table| (*version, Arc::new(table)))
            })
            .collect()
    })
}

pub(crate) fn switch_table(major: u32, minor: u32) -> Option<Arc<SwitchTable>> {
    parsed_tables().get(&(major, minor)).cloned()
}

pub(crate) fn common_switch_table() -> Option<Arc<SwitchTable>> {
    CONSENSUS_TABLE
        .get_or_init(|| {
            let tables = parsed_tables()
                .values()
                .map(|table| table.as_ref())
                .collect::<Vec<_>>();
            SwitchTable::consensus(&tables).map(Arc::new)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{GlobalSwitch, SwitchState, SwitchTable, switch_table};

    const MINIMAL_TABLE: &str = "# dcc-switches v1\n\
# banner: Embarcadero Delphi for Win32 compiler version 37.0\n\
# script-version: 1\n\
[codes]\n\
H2219 ON HINTS - no\n\
W1000 ON WARNINGS SYMBOL_DEPRECATED yes\n\
[properties]\n\
DCC_Hints Hints\n\
DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\n\
DCC_Warnings Warnings\n";

    #[test]
    fn parses_a_valid_table_and_queries_codes_and_properties() {
        let table = SwitchTable::parse(MINIMAL_TABLE).unwrap();

        let warning = table.code("W1000").unwrap();
        assert_eq!(warning.code, "W1000");
        assert_eq!(warning.default, SwitchState::On);
        assert_eq!(warning.global, GlobalSwitch::Warnings);
        assert_eq!(warning.id.as_deref(), Some("SYMBOL_DEPRECATED"));
        assert!(warning.error_capable);
        assert_eq!(table.code("w1000"), Some(warning));

        let hint = table.code("H2219").unwrap();
        assert_eq!(hint.global, GlobalSwitch::Hints);
        assert_eq!(hint.id, None);
        assert_eq!(
            table.id_for_property("DCC_SYMBOL_DEPRECATED"),
            Some("SYMBOL_DEPRECATED")
        );
        assert_eq!(table.id_for_property("DCC_Hints"), Some("Hints"));
        assert_eq!(table.id_for_property("DCC_Warnings"), Some("Warnings"));
        assert_eq!(
            table.warning_id_for_property("DCC_SYMBOL_DEPRECATED"),
            Some("SYMBOL_DEPRECATED")
        );
        assert_eq!(table.warning_id_for_property("DCC_Hints"), None);
        assert_eq!(
            table.id_for_property("dcc_symbol_deprecated"),
            Some("SYMBOL_DEPRECATED")
        );
        assert_eq!(table.id_for_property("DCC_UNKNOWN"), None);
    }

    #[test]
    fn rejects_malformed_table_forms_with_the_source_line() {
        let cases = [
            (MINIMAL_TABLE.replace("# dcc-switches v1", "# dcc-switches v2"), 1),
            (MINIMAL_TABLE.replace("# banner: Embarcadero Delphi for Win32 compiler version 37.0", "# banner: "), 2),
            (MINIMAL_TABLE.replace("# script-version: 1", "# script-version: 2"), 3),
            (MINIMAL_TABLE.replace("[codes]", "[unknown]"), 4),
            (MINIMAL_TABLE.replace("H2219 ON HINTS - no", "H2219 ON HINTS -"), 5),
            (MINIMAL_TABLE.replace("H2219 ON HINTS - no", "H9999 ON HINTS - no"), 5),
            (MINIMAL_TABLE.replace("W1000 ON", "W1000 MAYBE"), 6),
            (MINIMAL_TABLE.replace("H2219 ON HINTS", "H2219 ON INVALID"), 5),
            (
                MINIMAL_TABLE.replace("SYMBOL_DEPRECATED yes", "symbol_deprecated yes"),
                6,
            ),
            (
                MINIMAL_TABLE.replace("SYMBOL_DEPRECATED yes", "SYMBOL_DEPRECATED maybe"),
                6,
            ),
            (MINIMAL_TABLE.replace("H2219 ON HINTS - no", "W1000 ON HINTS - no"), 6),
            (MINIMAL_TABLE.replace("[properties]", "[unknown]"), 7),
            (MINIMAL_TABLE.replace("DCC_Hints Hints", "DCC_Hints"), 8),
            (MINIMAL_TABLE.replace("DCC_Hints Hints", "BAD Hints"), 8),
            (MINIMAL_TABLE.replace("DCC_Hints Hints", "DCC_Hints invalid!"), 8),
            (
                MINIMAL_TABLE.replace(
                    "DCC_Hints Hints\nDCC_SYMBOL_DEPRECATED",
                    "DCC_Hints Hints\nDCC_Hints Hints\nDCC_SYMBOL_DEPRECATED",
                ),
                9,
            ),
            (
                MINIMAL_TABLE.replace(
                    "DCC_Hints Hints\nDCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED",
                    "DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\nDCC_Hints Hints",
                ),
                9,
            ),
            (
                MINIMAL_TABLE.replace(
                    "DCC_Hints Hints\nDCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\nDCC_Warnings Warnings\n",
                    "",
                ),
                8,
            ),
        ];

        for (text, expected_line) in cases {
            let error = SwitchTable::parse(&text).unwrap_err();
            assert!(
                error.contains(&format!("line {expected_line}:")),
                "expected line {expected_line} in {error:?}"
            );
        }
    }

    #[test]
    fn registry_returns_the_37_table_with_w1000() {
        let table = switch_table(37, 0).expect("37.0 switch table");
        let warning = table.code("W1000").expect("W1000 entry");

        assert_eq!(warning.id.as_deref(), Some("SYMBOL_DEPRECATED"));
        assert_eq!(warning.default, SwitchState::On);
        assert_eq!(warning.global, GlobalSwitch::Warnings);
    }

    #[test]
    fn registry_loads_every_derived_version_and_rejects_unknown_versions() {
        for (major, minor) in [(21, 0), (24, 0), (37, 0)] {
            let table = switch_table(major, minor)
                .unwrap_or_else(|| panic!("{major}.{minor} switch table"));

            for code in [
                "H2219", "W1000", "W1001", "W1002", "W1003", "W1004", "W1005", "W1006", "W1007",
                "W1009", "W1010",
            ] {
                assert!(
                    table.code(code).is_some(),
                    "{major}.{minor}: missing {code}"
                );
            }
            assert_eq!(
                table.code("W1000").and_then(|entry| entry.id.as_deref()),
                Some("SYMBOL_DEPRECATED"),
                "{major}.{minor}"
            );
        }
        assert!(switch_table(22, 0).is_none());
    }

    #[test]
    fn consensus_keeps_only_rows_that_agree_across_tables() {
        let first = SwitchTable::parse(
            "# dcc-switches v1\n\
             # banner: synthetic first\n\
             # script-version: 1\n\
             [codes]\n\
             W1000 ON WARNINGS SYMBOL_DEPRECATED yes\n\
             W1009 ON WARNINGS HIDING_MEMBER yes\n\
             [properties]\n\
             DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\n\
             DCC_Warnings Warnings\n",
        )
        .unwrap();
        let second = SwitchTable::parse(
            "# dcc-switches v1\n\
             # banner: synthetic second\n\
             # script-version: 1\n\
             [codes]\n\
             W1000 ON WARNINGS SYMBOL_DEPRECATED yes\n\
             W1009 ON WARNINGS OTHER_MEMBER yes\n\
             [properties]\n\
             DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\n\
             DCC_Warnings Warnings\n",
        )
        .unwrap();

        let consensus = SwitchTable::consensus(&[&first, &second]).unwrap();

        assert!(consensus.code("W1000").is_some());
        assert!(
            consensus.code("W1009").is_none(),
            "a code with differing warning IDs is not safe to infer"
        );
    }
}
