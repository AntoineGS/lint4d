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
    let text = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

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
}
