use std::cmp::Ordering;
use std::collections::HashSet;

use crate::{CompilerVersion, ConditionalContext};

// Bounds lexer work and token storage (at most one token per source byte).
const MAX_SYSTEM_PAS_SCAN_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RtlConstantSource {
    Override,
    SystemPas,
    Table,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPasScan {
    Declared(Vec<String>),
    Inconclusive(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Identifier,
    Equals,
    Semicolon,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    kind: TokenKind,
    conditional_depth: usize,
}

/// Lexically scan a System.pas interface section for unconditional RTL update
/// constants. This intentionally does not depend on the Pascal parser.
pub fn scan_system_pas(source: &str) -> SystemPasScan {
    if source.len() > MAX_SYSTEM_PAS_SCAN_BYTES {
        return SystemPasScan::Inconclusive("System.pas exceeds the 4 MiB scan limit".to_owned());
    }

    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut conditional_depth = 0;
    let mut tokens = Vec::new();
    let mut interface_index = None;
    let mut implementation_index = None;

    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }

        if byte == b'/' && bytes.get(cursor + 1) == Some(&b'/') {
            cursor += 2;
            while cursor < bytes.len() && !matches!(bytes[cursor], b'\r' | b'\n') {
                cursor += 1;
            }
            continue;
        }

        if byte == b'{' {
            let Some(end) = bytes[cursor + 1..].iter().position(|byte| *byte == b'}') else {
                return SystemPasScan::Inconclusive("unterminated brace comment".into());
            };
            let end = cursor + 1 + end;
            if bytes.get(cursor + 1) == Some(&b'$')
                && let Err(reason) =
                    apply_directive(&source[cursor + 2..end], &mut conditional_depth)
            {
                return SystemPasScan::Inconclusive(reason);
            }
            cursor = end + 1;
            continue;
        }

        if byte == b'(' && bytes.get(cursor + 1) == Some(&b'*') {
            let Some(relative_end) = source[cursor + 2..].find("*)") else {
                return SystemPasScan::Inconclusive("unterminated parenthesized comment".into());
            };
            let end = cursor + 2 + relative_end;
            if bytes.get(cursor + 2) == Some(&b'$')
                && let Err(reason) =
                    apply_directive(&source[cursor + 3..end], &mut conditional_depth)
            {
                return SystemPasScan::Inconclusive(reason);
            }
            cursor = end + 2;
            continue;
        }

        if byte == b'\'' {
            let start = cursor;
            cursor += 1;
            let mut closed = false;
            while cursor < bytes.len() {
                if bytes[cursor] == b'\'' {
                    if bytes.get(cursor + 1) == Some(&b'\'') {
                        cursor += 2;
                    } else {
                        cursor += 1;
                        closed = true;
                        break;
                    }
                } else {
                    cursor += 1;
                }
            }
            if !closed {
                return SystemPasScan::Inconclusive("unterminated string literal".into());
            }
            tokens.push(Token {
                text: &source[start..cursor],
                kind: TokenKind::Other,
                conditional_depth,
            });
            continue;
        }

        if is_identifier_start(byte) {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                cursor += 1;
            }
            let text = &source[start..cursor];
            if interface_index.is_some() && text.eq_ignore_ascii_case("implementation") {
                if conditional_depth != 0 {
                    return SystemPasScan::Inconclusive(
                        "interface section ends inside a conditional directive".into(),
                    );
                }
                implementation_index = Some(tokens.len());
                break;
            }
            if interface_index.is_none() && text.eq_ignore_ascii_case("interface") {
                interface_index = Some(tokens.len());
            }
            tokens.push(Token {
                text,
                kind: TokenKind::Identifier,
                conditional_depth,
            });
            continue;
        }

        let start = cursor;
        cursor += 1;
        let kind = match byte {
            b'=' => TokenKind::Equals,
            b';' => TokenKind::Semicolon,
            _ => TokenKind::Other,
        };
        tokens.push(Token {
            text: &source[start..cursor],
            kind,
            conditional_depth,
        });
    }

    let (Some(interface_index), Some(implementation_index)) =
        (interface_index, implementation_index)
    else {
        return SystemPasScan::Inconclusive(
            "could not find both interface and implementation sections".into(),
        );
    };

    let section = &tokens[interface_index + 1..implementation_index];
    let mut declared = Vec::new();
    let mut seen = HashSet::new();
    for window in section.windows(4) {
        let [name, equals, value, semicolon] = window else {
            unreachable!("windows(4) always produces four tokens")
        };
        if name.kind != TokenKind::Identifier
            || !ConditionalContext::is_rtl_version_constant(name.text)
            || equals.kind != TokenKind::Equals
            || value.kind != TokenKind::Identifier
            || !value.text.eq_ignore_ascii_case("true")
            || semicolon.kind != TokenKind::Semicolon
        {
            continue;
        }
        if [name, equals, value, semicolon]
            .iter()
            .any(|token| token.conditional_depth != 0)
        {
            return SystemPasScan::Inconclusive(format!("{} is declared conditionally", name.text));
        }
        if seen.insert(name.text.to_ascii_lowercase()) {
            declared.push(name.text.to_owned());
        }
    }
    SystemPasScan::Declared(declared)
}

/// Return the most recently known declared set for a compiler version.
pub fn latest_known_rtl_constants(version: CompilerVersion) -> Option<Vec<String>> {
    let oldest_with_updates = CompilerVersion::new(33, 0);
    if version.cmp_numeric(oldest_with_updates)? != Ordering::Greater {
        return Some(Vec::new());
    }

    let rows: [(CompilerVersion, &[&str]); 4] = [
        (
            CompilerVersion::new(34, 0),
            &["RTLVersion1041", "RTLVersion1042"],
        ),
        (
            CompilerVersion::new(35, 0),
            &["RTLVersion111", "RTLVersion112", "RTLVersion113"],
        ),
        // No local 12.x System.pas was available to verify the 12.3 row.
        (
            CompilerVersion::new(36, 0),
            &["RTLVersion121", "RTLVersion122", "RTLVersion123"],
        ),
        (CompilerVersion::new(37, 0), &["RTLVersion131"]),
    ];
    rows.into_iter()
        .find(|(known_version, _)| version.cmp_numeric(*known_version) == Some(Ordering::Equal))
        .map(|(_, names)| names.iter().map(|name| (*name).to_owned()).collect())
}

/// Resolve the declared RTL update constants in override, file, then table
/// order. An absent compiler version means the installation override cannot be
/// associated with a compiler and therefore cannot provide a definite answer.
pub fn resolve_rtl_constants(
    version: Option<CompilerVersion>,
    override_names: Option<&[String]>,
    system_pas: Option<Result<SystemPasScan, String>>,
) -> (RtlConstantSource, Option<Vec<String>>, Vec<String>) {
    let Some(version) = version else {
        return (RtlConstantSource::Unknown, None, Vec::new());
    };

    let mut warnings = Vec::new();
    if let Some(names) = override_names {
        if let Some(invalid_name) = names
            .iter()
            .find(|name| !ConditionalContext::is_rtl_version_constant(name))
        {
            warnings.push(format!(
                "rtlVersionConstants override contains invalid name `{invalid_name}`"
            ));
        } else {
            return (RtlConstantSource::Override, Some(names.to_vec()), warnings);
        }
    }

    if let Some(system_pas) = system_pas {
        match system_pas {
            Ok(SystemPasScan::Declared(names)) => {
                return (RtlConstantSource::SystemPas, Some(names), warnings);
            }
            Ok(SystemPasScan::Inconclusive(reason)) => {
                warnings.push(format!(
                    "System.pas RTL constant scan was inconclusive: {reason}"
                ));
            }
            Err(reason) => warnings.push(format!("could not read System.pas: {reason}")),
        }
    }

    match latest_known_rtl_constants(version) {
        Some(names) => (RtlConstantSource::Table, Some(names), warnings),
        None => (RtlConstantSource::Unknown, None, warnings),
    }
}

fn apply_directive(directive: &str, conditional_depth: &mut usize) -> Result<(), String> {
    let directive = directive.trim_start_matches(|character: char| character.is_ascii_whitespace());
    let name_end = directive
        .bytes()
        .position(|byte| !is_identifier_continue(byte))
        .unwrap_or(directive.len());
    let name = &directive[..name_end];
    if matches_ignore_ascii_case(name, &["IF", "IFDEF", "IFNDEF", "IFOPT"]) {
        *conditional_depth = conditional_depth.saturating_add(1);
    } else if matches_ignore_ascii_case(name, &["ENDIF", "IFEND"]) {
        if *conditional_depth == 0 {
            return Err(format!("unmatched {{${name}}} directive"));
        }
        *conditional_depth -= 1;
    }
    Ok(())
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CompilerVersion;

    #[test]
    fn scanner_collects_unconditional_interface_declarations() {
        let source = "unit System; interface const RTLVersion = 35.00; RTLVersion111 = True; \
                      { RTLVersion999 = True; } // RTLVersion998 = True;\n RTLVersion112 = True; \
                      implementation const RTLVersion113 = True; end.";
        assert_eq!(
            scan_system_pas(source),
            SystemPasScan::Declared(vec!["RTLVersion111".into(), "RTLVersion112".into()])
        );
    }

    #[test]
    fn scanner_is_inconclusive_for_conditional_declarations() {
        let source = "unit System; interface {$IFDEF X} const RTLVersion111 = True; {$ENDIF} implementation end.";
        assert!(matches!(
            scan_system_pas(source),
            SystemPasScan::Inconclusive(_)
        ));
    }

    #[test]
    fn scanner_tracks_each_conditional_directive_pair() {
        for (open, close) in [
            ("{$IF TRUE}", "{$ENDIF}"),
            ("{$IFDEF X}", "{$IFEND}"),
            ("{$IFNDEF X}", "{$ENDIF}"),
            ("{$IFOPT C+}", "{$IFEND}"),
        ] {
            let source = format!(
                "unit System; interface {open} const RTLVersion111 = True; {close} implementation end."
            );
            assert!(matches!(
                scan_system_pas(&source),
                SystemPasScan::Inconclusive(_)
            ));
        }
    }

    #[test]
    fn scanner_is_inconclusive_when_a_declaration_crosses_a_conditional_boundary() {
        let source = "unit System; interface const RTLVersion111 {$IFDEF X} = True; {$ENDIF} implementation end.";
        assert!(matches!(
            scan_system_pas(source),
            SystemPasScan::Inconclusive(_)
        ));
    }

    #[test]
    fn old_rtl_declares_nothing() {
        let source = "unit System; interface const RTLVersion = 21.00; implementation end.";
        assert_eq!(scan_system_pas(source), SystemPasScan::Declared(vec![]));
    }

    #[test]
    fn scanner_skips_parenthesized_comments_and_strings() {
        let source = "unit System; interface (* RTLVersion901 = True; *) const S = 'RTLVersion902 = True;'; RTLVersion903 = True ; implementation end.";
        assert_eq!(
            scan_system_pas(source),
            SystemPasScan::Declared(vec!["RTLVersion903".into()])
        );
    }

    #[test]
    fn scanner_deduplicates_case_insensitively_and_matches_whole_identifiers() {
        let source = "unit System; interface const RTLVersion111 = True; rtlversion111 = True; PrefixRTLVersion112 = True; RTLVersion113 = False; implementation end.";
        assert_eq!(
            scan_system_pas(source),
            SystemPasScan::Declared(vec!["RTLVersion111".into()])
        );
    }

    #[test]
    fn scanner_is_inconclusive_without_complete_section_boundaries() {
        assert!(matches!(
            scan_system_pas("unit System; interface const RTLVersion111 = True;"),
            SystemPasScan::Inconclusive(_)
        ));
    }

    #[test]
    fn scanner_rejects_input_over_four_mib_with_a_resolver_warning() {
        const SCAN_LIMIT_BYTES: usize = 4 * 1024 * 1024;
        let comment_body = "x".repeat(SCAN_LIMIT_BYTES - 1);
        let source = format!("{{{comment_body}}}");
        assert_eq!(source.len(), SCAN_LIMIT_BYTES + 1);

        let scan = scan_system_pas(&source);
        let SystemPasScan::Inconclusive(reason) = &scan else {
            panic!("oversized source should be inconclusive: {scan:?}");
        };
        assert!(reason.contains("4 MiB"), "unexpected scan reason: {reason}");

        let (_, _, warnings) =
            resolve_rtl_constants(Some(CompilerVersion::new(35, 0)), None, Some(Ok(scan)));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("4 MiB"));
    }

    #[test]
    fn latest_known_table_covers_supported_version_ranges() {
        assert_eq!(
            latest_known_rtl_constants(CompilerVersion::new(33, 0)),
            Some(vec![])
        );
        assert_eq!(
            latest_known_rtl_constants(CompilerVersion::new(34, 0)),
            Some(vec!["RTLVersion1041".into(), "RTLVersion1042".into()])
        );
        assert_eq!(
            latest_known_rtl_constants(CompilerVersion::new(35, 0)),
            Some(vec![
                "RTLVersion111".into(),
                "RTLVersion112".into(),
                "RTLVersion113".into()
            ])
        );
        assert_eq!(
            latest_known_rtl_constants(CompilerVersion::new(36, 0)),
            Some(vec![
                "RTLVersion121".into(),
                "RTLVersion122".into(),
                "RTLVersion123".into()
            ])
        );
        assert_eq!(
            latest_known_rtl_constants(CompilerVersion::new(37, 0)),
            Some(vec!["RTLVersion131".into()])
        );
        assert_eq!(
            latest_known_rtl_constants(CompilerVersion::new(38, 0)),
            None
        );
    }

    #[test]
    fn resolution_order_is_override_system_pas_table() {
        let v35 = Some(CompilerVersion::new(35, 0));
        let over = vec!["RTLVersion111".to_string()];
        let scan = Ok(SystemPasScan::Declared(vec![
            "RTLVersion111".into(),
            "RTLVersion112".into(),
        ]));
        assert_eq!(
            resolve_rtl_constants(v35, Some(&over), Some(scan.clone())).0,
            RtlConstantSource::Override
        );
        let (source, names, _) = resolve_rtl_constants(v35, None, Some(scan));
        assert_eq!(
            (source, names.unwrap().len()),
            (RtlConstantSource::SystemPas, 2)
        );
        let (source, names, warnings) =
            resolve_rtl_constants(v35, None, Some(Ok(SystemPasScan::Inconclusive("x".into()))));
        assert_eq!(
            (source, names.unwrap().len()),
            (RtlConstantSource::Table, 3)
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            resolve_rtl_constants(None, Some(&over), None).0,
            RtlConstantSource::Unknown
        );
        assert_eq!(
            resolve_rtl_constants(Some(CompilerVersion::new(99, 0)), None, None).1,
            None
        );
    }

    #[test]
    fn resolution_reports_system_pas_errors_and_empty_overrides_are_explicit() {
        let version = Some(CompilerVersion::new(35, 0));
        let (source, names, warnings) =
            resolve_rtl_constants(version, None, Some(Err("unreadable System.pas".into())));
        assert_eq!(source, RtlConstantSource::Table);
        assert_eq!(names.unwrap().len(), 3);
        assert_eq!(warnings.len(), 1);

        let (source, names, warnings) = resolve_rtl_constants(version, Some(&[]), None);
        assert_eq!(source, RtlConstantSource::Override);
        assert_eq!(names, Some(vec![]));
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_direct_override_does_not_become_a_definite_answer() {
        let invalid = vec!["RTLVersion".to_owned()];
        let (source, names, warnings) =
            resolve_rtl_constants(Some(CompilerVersion::new(99, 0)), Some(&invalid), None);
        assert_eq!(source, RtlConstantSource::Unknown);
        assert_eq!(names, None);
        assert_eq!(warnings.len(), 1);
    }
}
