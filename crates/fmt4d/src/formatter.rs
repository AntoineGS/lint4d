use crate::blank_lines::normalize_blank_lines;
use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::directive_map::DirectiveMap;
use crate::doc_builder::DocBuilder;
use crate::renderer::Renderer;
use pascal_core::directives::parse_format_regions;
use pascal_core::{detect_encoding, encode_as, FileInfo, SourceEncoding};
use std::collections::HashSet;

/// Format Delphi/Object Pascal source code.
///
/// Returns the formatted source as a UTF-8 [`String`]. Internal processing
/// always works with UTF-8; if `source` is a legacy 8-bit encoding
/// (Latin-1 / Windows-1252), the bytes are decoded losslessly by
/// [`pascal_core::decode_bytes`] at the boundaries that need text.
///
/// If `source` has parse errors, the original text is returned unchanged
/// (decoded as text so callers get a [`String`] regardless of input encoding).
///
/// **For disk I/O, prefer [`format_bytes`]**, which also detects and preserves
/// the source encoding on output.
///
/// `external_units` should be computed once via `uses::scan_external_paths()`
/// and shared across all files to avoid redundant filesystem walks.
pub fn format_source(
    source: &[u8],
    info: &FileInfo,
    config: &FmtConfig,
    external_units: &HashSet<String>,
) -> Result<String, String> {
    let has_bom = source.starts_with(&[0xEF, 0xBB, 0xBF]);

    let (tree, diagnostics) =
        pascal_core::parser::parse_file(info, source).map_err(|e| e.to_string())?;

    if !diagnostics.is_empty() {
        // Return the source unchanged on parse errors. Use the tolerant
        // decoder so legacy Latin-1 sources don't hit a hard "invalid UTF-8"
        // error here — they just pass through as text.
        return Ok(pascal_core::decode_bytes(source).into_owned());
    }

    let resolved_eol = config.end_of_line.resolve(source);
    let comment_map = CommentMap::build(tree.root_node(), source);
    let directive_map = DirectiveMap::build(tree.root_node(), source);
    let format_regions = parse_format_regions(source);

    let builder = DocBuilder::new(
        source,
        config,
        &comment_map,
        &directive_map,
        format_regions,
        external_units.clone(),
    );
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

/// Format Delphi/Object Pascal source code **preserving the on-disk encoding**.
///
/// This is the right entry point for any tool that reads bytes from disk and
/// writes bytes back. It:
///
/// 1. Detects the source encoding ([`SourceEncoding::Utf8`],
///    [`SourceEncoding::Utf8Bom`], or [`SourceEncoding::Latin1`]).
/// 2. Delegates to [`format_source`] for the actual formatting (which always
///    works in UTF-8 internally).
/// 3. Re-encodes the result to the **original** encoding so legacy
///    Latin-1 / Windows-1252 codebases do not get silently upgraded to UTF-8
///    (and so do not generate spurious encoding churn in version control).
///
/// Round-trip contract: if the input is valid UTF-8, the output is valid
/// UTF-8; if the input is non-UTF-8 (treated as Latin-1), the output is
/// non-UTF-8 bytes in the same encoding.
pub fn format_bytes(
    source: &[u8],
    info: &FileInfo,
    config: &FmtConfig,
    external_units: &HashSet<String>,
) -> Result<Vec<u8>, String> {
    let encoding = detect_encoding(source);
    let formatted = format_source(source, info, config, external_units)?;
    // Guard: for Latin-1 sources, strip any U+FEFF that format_source may
    // have inserted via its BOM-handling path. That path looks for the
    // bytes `EF BB BF` at the start of the raw source — for a Latin-1 file
    // those actually represent the three Windows-1252 characters `ï`, `»`,
    // `¿`, not a BOM. Without this strip, a Latin-1 file that happens to
    // start with those three characters would grow a spurious U+FEFF
    // prefix on every format.
    let formatted = if encoding == SourceEncoding::Latin1 {
        formatted
            .strip_prefix('\u{FEFF}')
            .map(|s| s.to_string())
            .unwrap_or(formatted)
    } else {
        formatted
    };
    Ok(encode_as(&formatted, encoding))
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
