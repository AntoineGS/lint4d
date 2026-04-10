/// A parsed suppression directive found in source code.
#[derive(Debug, Clone, PartialEq)]
pub struct Suppression {
    /// The 1-based line number that this suppression applies to.
    pub target_line: usize,
    /// The specific rule to suppress, or `None` to suppress all rules.
    pub rule_id: Option<String>,
}

impl Suppression {
    /// Returns `true` when this suppression covers the given rule and line.
    pub fn matches(&self, rule: &str, line: usize) -> bool {
        if self.target_line != line {
            return false;
        }
        match &self.rule_id {
            None => true,
            Some(id) => id == rule,
        }
    }
}

/// Parse all suppression directives from `source` bytes.
///
/// Recognises three forms:
/// - `// lint4d:ignore [rule-id] [-- reason]`  — on its own line: targets the NEXT line.
/// - `// lint4d:ignore-next-line [rule-id]`     — targets the NEXT line.
/// - `code; // lint4d:ignore [rule-id]`         — inline: targets the SAME line.
pub fn parse_suppressions(source: &[u8]) -> Vec<Suppression> {
    // Tolerate legacy Latin-1 / Windows-1252 sources — decoding every byte
    // losslessly so a single accented character in a comment can't silently
    // disable suppressions file-wide.
    let text = crate::text::decode_bytes(source);

    let lines: Vec<&str> = text.lines().collect();
    let mut result = Vec::new();

    for (idx, &line) in lines.iter().enumerate() {
        let line_number = idx + 1; // 1-based

        let Some(comment_start) = find_line_comment(line) else {
            continue;
        };

        let comment = line[comment_start + 2..].trim(); // strip leading `//`

        // Determine whether the `//` starts the line (ignoring whitespace before it).
        let is_line_comment_only = line[..comment_start].trim().is_empty();

        if let Some(rest) = comment.strip_prefix("lint4d:ignore-next-line") {
            let rule_id = extract_rule_id(rest);
            let target_line = line_number + 1;
            result.push(Suppression {
                target_line,
                rule_id,
            });
        } else if let Some(rest) = comment.strip_prefix("lint4d:ignore") {
            let rule_id = extract_rule_id(rest);
            let target_line = if is_line_comment_only {
                // Standalone comment — suppress the line that follows.
                line_number + 1
            } else {
                // Inline comment — suppress the current line.
                line_number
            };
            result.push(Suppression {
                target_line,
                rule_id,
            });
        }
    }

    result
}

/// Extract an optional rule-id from the text that follows a directive keyword.
///
/// - Strips leading whitespace.
/// - Stops at ` -- ` or an em-dash (`\u{2014}`) reason separator.
/// - Returns `None` when the remaining text is empty.
fn extract_rule_id(rest: &str) -> Option<String> {
    let trimmed = rest.trim_start();

    // Strip reason text after ` -- ` or `\u{2014}`.
    let rule_part = if let Some(pos) = trimmed.find(" -- ") {
        &trimmed[..pos]
    } else if let Some(pos) = trimmed.find('\u{2014}') {
        &trimmed[..pos]
    } else {
        trimmed
    };

    let rule = rule_part.trim();
    if rule.is_empty() {
        None
    } else {
        Some(rule.to_string())
    }
}

/// Find the byte offset of the first `//` that is not inside a Delphi string literal.
///
/// Delphi strings are delimited by single-quotes (`'`). A doubled quote (`''`)
/// inside a string is an escaped quote, NOT the end of the string.
pub fn find_line_comment(line: &str) -> Option<usize> {
    let mut in_string = false;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' {
            if in_string && i + 1 < chars.len() && chars[i + 1] == '\'' {
                i += 2; // skip doubled quote — stays inside string
                continue;
            }
            in_string = !in_string;
        } else if !in_string && i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            // Return the byte offset of this `//`.
            return Some(line.char_indices().nth(i).map(|(b, _)| b).unwrap_or(i));
        }
        i += 1;
    }
    None
}

/// A region of source where formatting is disabled via `{$FMT.OFF}` / `{$FMT.ON}`.
#[derive(Debug, Clone, PartialEq)]
pub struct FormatOffRegion {
    /// 1-based start line (the line containing `{$FMT.OFF}`).
    pub start_line: usize,
    /// 1-based end line (the line containing `{$FMT.ON}`, or last line of file).
    pub end_line: usize,
}

/// Parse `{$FMT.OFF}` / `{$FMT.ON}` directive pairs from source bytes.
///
/// Case-insensitive matching. If `{$FMT.OFF}` appears without a matching
/// `{$FMT.ON}`, the region extends to the end of the file.
pub fn parse_format_regions(source: &[u8]) -> Vec<FormatOffRegion> {
    // Tolerate legacy Latin-1 / Windows-1252 sources — otherwise a single
    // accented character would silently disable all `{$FMT.OFF}` regions.
    let text = crate::text::decode_bytes(source);

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let mut regions = Vec::new();
    let mut off_line: Option<usize> = None;

    for (idx, &line) in lines.iter().enumerate() {
        let line_number = idx + 1; // 1-based
        let upper = line.to_uppercase();

        if off_line.is_none() && upper.contains("{$FMT.OFF}") {
            off_line = Some(line_number);
        } else if off_line.is_some() && upper.contains("{$FMT.ON}") {
            regions.push(FormatOffRegion {
                start_line: off_line.unwrap(),
                end_line: line_number,
            });
            off_line = None;
        }
    }

    // Unclosed {$FMT.OFF} — close at end of file
    if let Some(start) = off_line {
        regions.push(FormatOffRegion {
            start_line: start,
            end_line: total_lines,
        });
    }

    regions
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn find_line_comment_basic() {
        assert_eq!(find_line_comment("// foo"), Some(0));
        assert_eq!(find_line_comment("code; // bar"), Some(6));
        assert_eq!(find_line_comment("no comment"), None);
    }

    #[test]
    fn find_line_comment_ignores_slashes_in_strings() {
        // The // inside the string should not be treated as a comment start.
        assert_eq!(
            find_line_comment("s := '//not a comment'; // real"),
            Some(24)
        );
    }

    #[test]
    fn find_line_comment_doubled_quote_escape() {
        // The '' inside the string is an escaped quote, not end-of-string.
        assert_eq!(find_line_comment("s := 'it''s here'; // real"), Some(19));
    }

    #[test]
    fn extract_rule_id_none_when_empty() {
        assert_eq!(extract_rule_id(""), None);
        assert_eq!(extract_rule_id("   "), None);
    }

    #[test]
    fn extract_rule_id_strips_reason() {
        assert_eq!(
            extract_rule_id(" my-rule -- some reason"),
            Some("my-rule".to_string())
        );
    }

    #[test]
    fn extract_rule_id_strips_em_dash_reason() {
        let input = format!(" my-rule \u{2014} some reason");
        assert_eq!(extract_rule_id(&input), Some("my-rule".to_string()));
    }

    // ── Format region tests ────────────────────────────────────────

    #[test]
    fn format_regions_simple_pair() {
        let source = b"line1\n{$FMT.OFF}\nline3\n{$FMT.ON}\nline5\n";
        let regions = parse_format_regions(source);
        assert_eq!(
            regions,
            vec![FormatOffRegion {
                start_line: 2,
                end_line: 4
            }]
        );
    }

    #[test]
    fn format_regions_case_insensitive() {
        let source = b"line1\n{$fmt.off}\nline3\n{$Fmt.On}\nline5\n";
        let regions = parse_format_regions(source);
        assert_eq!(
            regions,
            vec![FormatOffRegion {
                start_line: 2,
                end_line: 4
            }]
        );
    }

    #[test]
    fn format_regions_unclosed_off_extends_to_eof() {
        let source = b"line1\nline2\n{$FMT.OFF}\nline4\nline5\n";
        let regions = parse_format_regions(source);
        assert_eq!(
            regions,
            vec![FormatOffRegion {
                start_line: 3,
                end_line: 5
            }]
        );
    }

    #[test]
    fn format_regions_multiple_pairs() {
        let source = b"line1\n{$FMT.OFF}\nline3\n{$FMT.ON}\nline5\n{$FMT.OFF}\nline7\n{$FMT.ON}\n";
        let regions = parse_format_regions(source);
        assert_eq!(
            regions,
            vec![
                FormatOffRegion {
                    start_line: 2,
                    end_line: 4
                },
                FormatOffRegion {
                    start_line: 6,
                    end_line: 8
                },
            ]
        );
    }

    #[test]
    fn format_regions_no_directives() {
        let source = b"line1\nline2\nline3\n";
        let regions = parse_format_regions(source);
        assert!(regions.is_empty());
    }

    // ── Non-UTF-8 (Latin-1) regression coverage ────────────────────
    // Legacy Delphi sources are often Windows-1252/Latin-1. A single
    // accented byte in a comment previously caused both directive parsers
    // to return an empty vec, silently disabling all suppressions and
    // format-off regions file-wide.

    #[test]
    fn format_regions_parse_latin1_source() {
        // Line 2 contains a 0xE9 ('é') byte — invalid UTF-8, valid Latin-1.
        let mut source: Vec<u8> =
            b"line1\n// caf\xE9\n{$FMT.OFF}\nline4\n{$FMT.ON}\nline6\n".to_vec();
        // sanity: ensure the bytes really are non-UTF-8
        assert!(std::str::from_utf8(&source).is_err());
        let regions = parse_format_regions(&source);
        assert_eq!(
            regions,
            vec![FormatOffRegion {
                start_line: 3,
                end_line: 5
            }],
            "{{$FMT.OFF}} region should be found in Latin-1 source"
        );
        // Silence unused mut warning from the future `let mut` refactor.
        source.clear();
    }

    #[test]
    fn suppressions_parse_latin1_source() {
        // Line 1 contains 0xE9 in a comment; the actual suppression is on
        // a later line. The suppression must still be parsed.
        let source: Vec<u8> =
            b"// caf\xE9\n// lint4d:ignore-next-line my-rule\ncode := 1;\n".to_vec();
        assert!(std::str::from_utf8(&source).is_err());
        let sups = parse_suppressions(&source);
        assert!(
            !sups.is_empty(),
            "suppressions should be parsed in Latin-1 source"
        );
        assert_eq!(sups[0].rule_id, Some("my-rule".to_string()));
        assert_eq!(sups[0].target_line, 3);
    }
}
