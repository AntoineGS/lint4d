use crate::engine::{Diagnostic, Severity};
use owo_colors::{OwoColorize, Style};

/// Format a slice of diagnostics for a single file in rustc-style terminal output.
///
/// Each diagnostic is rendered as:
/// ```text
/// severity[rule-id]: message
///   --> file:line:col
///    |
/// NN | source line (with optional previous / next context)
///    | ^^^^^ underline spanning the diagnostic columns
///   = help: ...
/// ```
///
/// Returns an empty string when `diagnostics` is empty.
pub fn format_diagnostics(
    file_path: &str,
    source: &[u8],
    diagnostics: &[Diagnostic],
    use_color: bool,
) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }

    let source_str = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = source_str.lines().collect();

    let mut out = String::new();

    for diag in diagnostics {
        // ---- Header line: severity[rule-id]: message ----
        let severity_label = diag.severity.to_string();
        let header = format!("{}[{}]: {}", severity_label, diag.rule_id, diag.message);
        if use_color {
            let style = severity_style(diag.severity);
            out.push_str(&format!("{}\n", header.style(style)));
        } else {
            out.push_str(&header);
            out.push('\n');
        }

        // ---- Arrow line: --> file:line:col [in Scope] ----
        if let Some(ref scope) = diag.scope {
            out.push_str(&format!(
                "  --> {}:{}:{} in {}\n",
                file_path, diag.line, diag.column, scope
            ));
        } else {
            out.push_str(&format!(
                "  --> {}:{}:{}\n",
                file_path, diag.line, diag.column
            ));
        }

        // ---- Source context ----
        // Show previous line, the diagnostic line, and next line.
        let diag_line_idx = diag.line.saturating_sub(1); // 0-based index

        let context_start = if diag_line_idx > 0 {
            diag_line_idx - 1
        } else {
            diag_line_idx
        };
        let context_end = (diag_line_idx + 1).min(lines.len().saturating_sub(1));

        // Determine width of line number column (based on largest line number shown).
        let max_line_no = context_end + 1; // +1 for 1-based display
        let lno_width = max_line_no.to_string().len().max(2);

        // Separator bar line (empty gutter)
        let gutter_blank = " ".repeat(lno_width);
        out.push_str(&format!("{} |\n", gutter_blank));

        for line_idx in context_start..=context_end {
            let line_no = line_idx + 1;
            let line_content = lines.get(line_idx).copied().unwrap_or("");
            out.push_str(&format!(
                "{:>width$} | {}\n",
                line_no,
                line_content,
                width = lno_width
            ));

            // After the diagnostic line emit the underline
            if line_idx == diag_line_idx {
                let col_start = diag.column.saturating_sub(1);
                let col_end = if diag.end_line == diag.line {
                    diag.end_column.saturating_sub(1)
                } else {
                    line_content.len()
                };
                let underline_len = if col_end > col_start {
                    col_end - col_start
                } else {
                    1
                };
                let underline = "^".repeat(underline_len);
                let padding = " ".repeat(col_start);
                if use_color {
                    let style = severity_style(diag.severity);
                    out.push_str(&format!(
                        "{} | {}{}\n",
                        gutter_blank,
                        padding,
                        underline.style(style)
                    ));
                } else {
                    out.push_str(&format!("{} | {}{}\n", gutter_blank, padding, underline));
                }
            }
        }

        // ---- Help line ----
        if let Some(ref help) = diag.help {
            out.push_str(&format!("{} = help: {}\n", gutter_blank, help));
        }

        out.push('\n');
    }

    out
}

fn severity_style(severity: Severity) -> Style {
    match severity {
        Severity::Error => Style::new().red().bold(),
        Severity::Warning => Style::new().yellow().bold(),
        Severity::Hint => Style::new().cyan().bold(),
    }
}
