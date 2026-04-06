use crate::blank_lines::normalize_blank_lines;
use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::doc_builder::DocBuilder;
use crate::renderer::Renderer;
use crate::uses;
use pascal_core::directives::parse_format_regions;
use pascal_core::FileInfo;
use std::collections::HashSet;

/// Format Delphi/Object Pascal source code.
///
/// Returns the formatted source, or an error message.
/// If the source has parse errors, returns the original source unchanged.
pub fn format_source(source: &[u8], info: &FileInfo, config: &FmtConfig) -> Result<String, String> {
    let has_bom = source.starts_with(&[0xEF, 0xBB, 0xBF]);

    let (tree, diagnostics) =
        pascal_core::parser::parse_file(info, source).map_err(|e| e.to_string())?;

    if !diagnostics.is_empty() {
        let original = std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {}", e))?;
        return Ok(original.to_string());
    }

    let resolved_eol = config.end_of_line.resolve(source);
    let comment_map = CommentMap::build(tree.root_node(), source);
    let format_regions = parse_format_regions(source);

    let external_units = match &config.project_root {
        Some(root) => uses::scan_external_paths(root, &config.uses.external_paths),
        None => HashSet::new(),
    };

    let builder = DocBuilder::new(source, config, &comment_map, format_regions, external_units);
    let doc = builder.build(tree.root_node());
    let raw_output = Renderer::new(config).render(doc);

    let normalized = normalize_blank_lines(&raw_output, &config.blank_lines);
    let broken = break_long_lines(&normalized, config.max_line_length, config.indent_size);

    let mut final_output = resolved_eol.apply(&broken);

    if has_bom && !final_output.starts_with('\u{FEFF}') {
        final_output.insert(0, '\u{FEFF}');
    }

    Ok(final_output)
}

/// Post-processing pass: break lines that exceed `max_length`.
///
/// For each long line, finds the best break point (after `;`/`,` inside
/// parentheses, or after `+` outside string literals) and splits the line,
/// adding continuation indent.
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

/// Scan `bytes` for valid break positions — places where a long line can be
/// split.  Returns two tiers of byte offsets that point just past the break
/// token and any trailing spaces (i.e. the start of the next segment).
///
/// Preferred break tokens (first tier):
/// - `;` or `,` inside parentheses
///
/// Fallback break tokens (second tier):
/// - `+` outside string literals (string concatenation)
///
/// The caller should use preferred breaks when available and only fall back
/// to `+` breaks when no preferred breaks exist. This prevents string
/// concatenations from being over-broken into per-token lines.
fn scan_break_positions(bytes: &[u8]) -> (Vec<usize>, Vec<usize>) {
    let mut preferred = Vec::new(); // ; and , inside parens
    let mut fallback = Vec::new(); // + outside strings
    let mut paren_depth = 0i32;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            if in_string {
                // '' is an escaped quote — stay in the string
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_string = false;
            } else {
                in_string = true;
            }
            i += 1;
            continue;
        }
        if !in_string {
            match b {
                b'(' => paren_depth += 1,
                b')' => paren_depth -= 1,
                b';' | b',' if paren_depth > 0 => {
                    let mut end = i + 1;
                    while end < bytes.len() && bytes[end] == b' ' {
                        end += 1;
                    }
                    preferred.push(end);
                }
                b'+' => {
                    let mut end = i + 1;
                    while end < bytes.len() && bytes[end] == b' ' {
                        end += 1;
                    }
                    fallback.push(end);
                }
                _ => {}
            }
        }
        i += 1;
    }
    (preferred, fallback)
}

/// Break a single long line into multiple lines.
///
/// Prefers `;`/`,` break positions over `+` positions to avoid
/// over-breaking string concatenations.
fn break_single_line(line: &str, max_length: usize, indent_size: usize) -> Vec<String> {
    let base_indent = line.len() - line.trim_start().len();
    let continuation_indent = base_indent + indent_size;
    let cont_prefix: String = " ".repeat(continuation_indent);

    let (preferred, fallback) = scan_break_positions(line.as_bytes());
    // Use preferred breaks if available, otherwise fall back to + breaks
    let break_positions = if preferred.is_empty() {
        fallback
    } else {
        preferred
    };

    if break_positions.is_empty() {
        return vec![line.to_string()];
    }

    // Greedily split: keep taking as much as fits within max_length
    let mut lines = Vec::new();
    let mut remaining = line.to_string();
    loop {
        if remaining.len() <= max_length {
            lines.push(remaining);
            break;
        }

        // Find the rightmost break point that keeps the first part <= max_length
        let (pref, fall) = scan_break_positions(remaining.as_bytes());
        let positions = if pref.is_empty() { fall } else { pref };
        let best_bp = positions.iter().copied().rev().find(|&bp| bp <= max_length);

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
