use crate::config::BlankLineConfig;
use pascal_core::node_kind as K;

/// Post-process formatted output to normalize blank lines.
pub fn normalize_blank_lines(source: &str, config: &BlankLineConfig) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut result = Vec::new();
    let mut consecutive_blanks = 0;

    for line in &lines {
        if line.trim().is_empty() {
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

    if prev_kind == K::DEF_PROC && next_kind == K::DEF_PROC {
        return config.between_procedures;
    }
    if is_section(prev_kind) && is_section(next_kind) {
        return config.between_sections;
    }
    if (is_section(prev_kind) && next_kind == K::DEF_PROC)
        || (prev_kind == K::DEF_PROC && is_section(next_kind))
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
        let result = normalize_blank_lines(input, &config);
        assert_eq!(result, "line1\n\nline2\n");
    }

    #[test]
    fn no_trailing_blank_lines() {
        let config = BlankLineConfig::default();
        let input = "line1\n\n\n";
        let result = normalize_blank_lines(input, &config);
        assert_eq!(result, "line1\n");
    }

    #[test]
    fn ensure_final_newline() {
        let config = BlankLineConfig::default();
        let input = "line1";
        let result = normalize_blank_lines(input, &config);
        assert_eq!(result, "line1\n");
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
