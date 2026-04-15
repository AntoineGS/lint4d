use crate::blank_lines::normalize_blank_lines;
use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::directive_map::DirectiveMap;
use crate::doc_builder::DocBuilder;
use crate::renderer::Renderer;
use pascal_core::directives::parse_format_regions;
use pascal_core::{FileInfo, SourceEncoding, detect_encoding, encode_as};
use std::collections::HashSet;

/// Format Delphi/Object Pascal source code.
///
/// Returns the formatted source as a UTF-8 [`String`]. Internal processing
/// always works with UTF-8; if `source` is a legacy 8-bit encoding
/// (Latin-1 / Windows-1252), the bytes are decoded losslessly by
/// [`pascal_core::decode_bytes`] at the boundaries that need text.
///
/// If `source` has parse errors, returns [`crate::error::FmtError::Parse`]
/// — the formatter will not rewrite a file whose AST it doesn't trust.
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
) -> crate::error::Result<String> {
    let has_bom = source.starts_with(&[0xEF, 0xBB, 0xBF]);

    let (tree, diagnostics, patches) = pascal_core::parser::parse_file_with_patches(info, source)
        .map_err(|e| crate::error::FmtError::Parse {
        path: info.path.clone(),
        message: e.to_string(),
    })?;

    if !diagnostics.is_empty() {
        let first = diagnostics
            .first()
            .map(|d| format!("{:?}", d))
            .unwrap_or_else(|| "unknown".to_string());
        return Err(crate::error::FmtError::Parse {
            path: info.path.clone(),
            message: first,
        });
    }

    let resolved_eol = config.end_of_line.resolve(source);
    let comment_map = CommentMap::build(tree.root_node(), source);
    let directive_map = DirectiveMap::build_with_patches(tree.root_node(), source, &patches);
    let format_regions = parse_format_regions(source);

    let builder = DocBuilder::new(
        source,
        config,
        &comment_map,
        &directive_map,
        format_regions,
        external_units,
    );
    let doc = builder.build(tree.root_node());
    // Source length is a good upper bound for output length (the
    // formatter mostly adds/removes whitespace). Slight overshoot
    // avoids the final realloc. Review PERF-H4.
    let raw_output = Renderer::with_capacity(config, source.len() + source.len() / 16).render(doc);

    let normalized = normalize_blank_lines(&raw_output, &config.blank_lines);

    let mut final_output = resolved_eol.apply(&normalized);

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
) -> crate::error::Result<Vec<u8>> {
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
