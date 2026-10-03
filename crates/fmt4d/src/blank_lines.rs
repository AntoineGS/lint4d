use crate::config::BlankLineConfig;
use pascal_core::node_kind as K;
use std::ops::Range;

/// Post-process formatted output to normalize blank lines.
///
/// `protected` holds ascending byte ranges of `source` that were emitted
/// verbatim (multiline string literals, comments, format-off regions). A
/// line that starts inside one of them belongs to that text and is copied
/// unchanged.
pub fn normalize_blank_lines(
    source: &str,
    protected: &[Range<usize>],
    config: &BlankLineConfig,
) -> String {
    let mut result = Vec::new();
    let mut consecutive_blanks = 0;
    let mut spans = protected.iter().peekable();
    let mut offset = 0;

    // Same line splitting as `str::lines`, keeping each line's offset.
    for chunk in source.split_inclusive('\n') {
        let line_start = offset;
        offset += chunk.len();
        let line = chunk
            .strip_suffix('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .unwrap_or(chunk);

        while spans.next_if(|r| r.end <= line_start).is_some() {}
        if spans.peek().is_some_and(|r| r.start < line_start) {
            consecutive_blanks = 0;
            result.push(line);
        } else if line.trim().is_empty() {
            consecutive_blanks += 1;
            if consecutive_blanks <= config.max_consecutive {
                result.push("");
            }
        } else {
            consecutive_blanks = 0;
            result.push(line);
        }
    }

    // Remove trailing blank lines
    while result.last() == Some(&"") {
        result.pop();
    }

    let mut output = result.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

/// Determine number of blank lines to insert between two node kinds.
pub fn needs_blank_line_between(
    prev_kind: &str,
    next_kind: &str,
    config: &BlankLineConfig,
) -> usize {
    let is_section = |k: &str| {
        matches!(
            k,
            K::DECL_VARS | K::DECL_CONSTS | K::DECL_TYPES | K::DECL_USES
        )
    };

    let is_proc = |k: &str| matches!(k, K::DEF_PROC | K::DECL_PROC);

    if is_proc(prev_kind) && is_proc(next_kind) {
        return config.between_procedures;
    }
    if is_section(prev_kind) && is_section(next_kind) {
        return config.between_sections;
    }
    if (is_section(prev_kind) && is_proc(next_kind))
        || (is_proc(prev_kind) && is_section(next_kind))
    {
        return config.between_sections;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_multiple_blank_lines() {
        let config = BlankLineConfig::default();
        let input = "line1\n\n\n\nline2\n";
        let result = normalize_blank_lines(input, &[], &config);
        assert_eq!(result, "line1\n\nline2\n");
    }

    #[test]
    fn no_trailing_blank_lines() {
        let config = BlankLineConfig::default();
        let input = "line1\n\n\n";
        let result = normalize_blank_lines(input, &[], &config);
        assert_eq!(result, "line1\n");
    }

    #[test]
    fn ensure_final_newline() {
        let config = BlankLineConfig::default();
        let input = "line1";
        let result = normalize_blank_lines(input, &[], &config);
        assert_eq!(result, "line1\n");
    }

    #[test]
    fn protected_lines_are_kept_verbatim() {
        let config = BlankLineConfig::default();
        let input = "a := '''\n  x  \n\n  \n\n  ''';\n\n\n\nb;\n";
        let protected = 5..input.find(";\n").unwrap();
        let result = normalize_blank_lines(input, &[protected], &config);
        assert_eq!(result, "a := '''\n  x  \n\n  \n\n  ''';\n\nb;\n");
    }

    #[test]
    fn blank_run_ending_at_protected_text_is_still_collapsed() {
        let config = BlankLineConfig::default();
        let input = "a;\n\n\n\n{ c\n\n\n}\n";
        let start = input.find('{').unwrap();
        let protected = start..input.len() - 1;
        let result = normalize_blank_lines(input, &[protected], &config);
        assert_eq!(result, "a;\n\n{ c\n\n\n}\n");
    }

    #[test]
    fn blank_line_between_procedures() {
        let config = BlankLineConfig::default();
        assert_eq!(
            needs_blank_line_between(K::DEF_PROC, K::DEF_PROC, &config),
            1
        );
    }

    #[test]
    fn blank_line_between_sections() {
        let config = BlankLineConfig::default();
        assert_eq!(
            needs_blank_line_between(K::DECL_VARS, K::DECL_CONSTS, &config),
            1
        );
    }
}
