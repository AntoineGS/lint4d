use crate::blank_lines::normalize_blank_lines;
use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::printer::Printer;
use pascal_core::FileInfo;

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

    let mut printer = Printer::new(source, config, &comment_map);
    printer.print_node(tree.root_node());
    let raw_output = printer.result();

    let normalized = normalize_blank_lines(&raw_output, &config.blank_lines);

    Ok(normalized)
}
