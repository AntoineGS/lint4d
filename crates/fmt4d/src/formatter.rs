use crate::blank_lines::normalize_blank_lines;
use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::printer::Printer;
use crate::uses;
use pascal_core::directives::parse_format_regions;
use pascal_core::FileInfo;
use std::collections::HashSet;

/// Format Delphi/Object Pascal source code.
///
/// Returns the formatted source, or an error message.
/// If the source has parse errors, returns the original source unchanged.
pub fn format_source(source: &[u8], info: &FileInfo, config: &FmtConfig) -> Result<String, String> {
    let (tree, diagnostics) =
        pascal_core::parser::parse_file(info, source).map_err(|e| e.to_string())?;

    // If there are parse errors, return original source unchanged.
    if !diagnostics.is_empty() {
        let original = std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {}", e))?;
        return Ok(original.to_string());
    }

    let comment_map = CommentMap::build(tree.root_node(), source);
    let format_regions = parse_format_regions(source);

    let external_units = match &config.project_root {
        Some(root) => uses::scan_external_paths(root, &config.uses.external_paths),
        None => HashSet::new(),
    };

    let mut printer = Printer::new(source, config, &comment_map, format_regions, external_units);
    printer.print_node(tree.root_node());
    let raw_output = printer.result();

    let normalized = normalize_blank_lines(&raw_output, &config.blank_lines);
    let broken = break_long_lines(&normalized, config.max_line_length, config.indent_size);

    Ok(broken)
}

/// Post-processing pass: break lines that exceed `max_length`.
///
/// For each long line, finds the best break point (after `;` or `,` inside
/// parentheses) and splits the line, adding continuation indent.
fn break_long_lines(source: &str, max_length: usize, indent_size: usize) -> String {
    let mut result = Vec::new();
    for line in source.lines() {
        if line.len() <= max_length {
            result.push(line.to_string());
        } else {
            let broken = break_single_line(line, max_length, indent_size);
            for bl in broken {
                result.push(bl);
            }
        }
    }
    let mut output = result.join("\n");
    if source.ends_with('\n') && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Break a single long line into multiple lines.
fn break_single_line(line: &str, max_length: usize, indent_size: usize) -> Vec<String> {
    let base_indent = line.len() - line.trim_start().len();
    let continuation_indent = base_indent + indent_size;
    let cont_prefix: String = " ".repeat(continuation_indent);

    // Find break points: positions after `;` or `,` that are inside parens
    let mut break_positions = Vec::new();
    let mut paren_depth = 0i32;
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => paren_depth += 1,
            b')' => paren_depth -= 1,
            b';' | b',' if paren_depth > 0 => {
                // Break after this char (+ any trailing space)
                let mut end = i + 1;
                while end < bytes.len() && bytes[end] == b' ' {
                    end += 1;
                }
                break_positions.push(end);
            }
            _ => {}
        }
    }

    if break_positions.is_empty() {
        return vec![line.to_string()];
    }

    // Greedily split: take as much as fits within max_length
    let mut lines = Vec::new();
    let mut start = 0;
    let mut is_first = true;
    for &bp in &break_positions {
        let prefix_len = if is_first { 0 } else { continuation_indent };
        let candidate = if is_first {
            &line[start..bp]
        } else {
            // Would be: cont_prefix + line[start..bp]
            &line[start..bp]
        };
        let total_len = prefix_len + candidate.trim_start().len();
        if total_len > max_length && start < bp && !is_first {
            // Current segment already too long — emit what we have so far
            // Actually, we need to emit up to previous break point
        }
        if is_first {
            if line[start..bp].len() <= max_length {
                // First segment fits
                continue;
            } else {
                // Need to break here
                lines.push(line[start..bp].trim_end().to_string());
                start = bp;
                is_first = false;
            }
        }
    }

    // Simpler approach: break at the best position that keeps first part ≤ max_length
    lines.clear();
    let mut remaining = line.to_string();
    loop {
        if remaining.len() <= max_length {
            lines.push(remaining);
            break;
        }

        // Find break points in remaining
        let mut best_bp: Option<usize> = None;
        let mut paren_depth = 0i32;
        let bytes = remaining.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'(' => paren_depth += 1,
                b')' => paren_depth -= 1,
                b';' | b',' if paren_depth > 0 => {
                    let mut end = i + 1;
                    while end < bytes.len() && bytes[end] == b' ' {
                        end += 1;
                    }
                    // Check if breaking here keeps the first part ≤ max_length
                    if i < max_length {
                        best_bp = Some(end);
                    }
                }
                _ => {}
            }
        }

        if let Some(bp) = best_bp {
            let first_part = remaining[..bp].trim_end().to_string();
            let rest = remaining[bp..].trim_start().to_string();
            lines.push(first_part);
            remaining = format!("{}{}", cont_prefix, rest);
        } else {
            // No break point found — emit as-is
            lines.push(remaining);
            break;
        }
    }

    lines
}
