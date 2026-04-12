use crate::config::{FmtConfig, IndentStyle};
use crate::doc::{AlignCell, Doc};
use crate::spacing;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    Break,
}

/// Internal enum for merging alignment rows with non-row docs.
enum MergedItem {
    Row(Vec<AlignCell>),
    Other(Doc),
}

/// IQR-based outlier detection on a list of widths.
///
/// Returns a boolean vec of the same length: `true` = outlier.
/// For groups with fewer than 6 items, no outliers are detected
/// (quartile estimates are too noisy with fewer data points).
///
/// A minimum IQR floor of 5 prevents the "zero-IQR collapse" that
/// occurs when most identifiers share the same width: without the
/// floor, IQR→0 and the fence equals Q3 exactly, flagging any name
/// even one character longer than the mode.
fn detect_outliers(widths: &[usize]) -> Vec<bool> {
    const MIN_GROUP: usize = 6;
    const MIN_IQR: f64 = 5.0;

    let n = widths.len();
    if n < MIN_GROUP {
        return vec![false; n];
    }

    let mut sorted: Vec<usize> = widths.to_vec();
    sorted.sort_unstable();

    let q1 = percentile(&sorted, 25.0);
    let q3 = percentile(&sorted, 75.0);
    let iqr = (q3 - q1).max(MIN_IQR);
    let upper_fence = q3 + 1.5 * iqr;

    widths.iter().map(|&w| (w as f64) > upper_fence).collect()
}

/// Compute the p-th percentile of a sorted slice using linear interpolation.
fn percentile(sorted: &[usize], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0] as f64;
    }
    let rank = (p / 100.0) * (sorted.len() as f64 - 1.0);
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    let frac = rank - lower as f64;
    sorted[lower] as f64 + frac * (sorted[upper] as f64 - sorted[lower] as f64)
}

pub struct Renderer {
    output: String,
    current_column: usize,
    indent_size: usize,
    indent_style: IndentStyle,
    max_line_length: usize,
    last_token_kind: &'static str,
    last_token_parent_kind: &'static str,
}

impl Renderer {
    pub fn new(config: &FmtConfig) -> Self {
        Renderer {
            output: String::new(),
            current_column: 0,
            indent_size: config.indent_size,
            indent_style: config.indent_style,
            max_line_length: config.max_line_length,
            last_token_kind: "",
            last_token_parent_kind: "",
        }
    }

    pub fn render(mut self, doc: Doc) -> String {
        // Work stack: (indent_level, mode, doc)
        let mut stack: Vec<(usize, Mode, Doc)> = vec![(0, Mode::Break, doc)];

        while let Some((indent, mode, doc)) = stack.pop() {
            match doc {
                Doc::Empty => {}

                Doc::Token {
                    text,
                    kind,
                    parent_kind,
                } => {
                    self.emit_with_spacing(&text, kind, parent_kind, indent);
                }

                Doc::Raw(text) => {
                    for ch in text.chars() {
                        if ch == '\n' {
                            self.current_column = 0;
                        } else {
                            self.current_column += 1;
                        }
                    }
                    // If the raw text ends with whitespace, clear spacing state so
                    // the next token won't add a duplicate space.
                    if text.ends_with(|c: char| c.is_whitespace()) {
                        self.last_token_kind = "";
                        self.last_token_parent_kind = "";
                    }
                    self.output.push_str(&text);
                }

                Doc::Hardline => {
                    self.emit_newline();
                }

                Doc::BlankLine => {
                    self.emit_newline();
                    self.output.push('\n');
                }

                Doc::Line | Doc::PreservedLine => match mode {
                    Mode::Flat => {
                        self.output.push(' ');
                        self.current_column += 1;
                        // Clear spacing state so the next token won't add another space.
                        self.last_token_kind = "";
                        self.last_token_parent_kind = "";
                    }
                    Mode::Break => self.emit_newline(),
                },

                Doc::Softline => match mode {
                    Mode::Flat => {}
                    Mode::Break => self.emit_newline(),
                },

                Doc::Concat(docs) => {
                    for d in docs.into_iter().rev() {
                        stack.push((indent, mode, d));
                    }
                }

                Doc::Indent(inner) => {
                    stack.push((indent + 1, mode, *inner));
                }

                Doc::Group(inner) => {
                    if self.fits(indent, &inner) {
                        stack.push((indent, Mode::Flat, *inner));
                    } else {
                        stack.push((indent, Mode::Break, *inner));
                    }
                }

                Doc::IfBreak { broken, flat } => match mode {
                    Mode::Flat => stack.push((indent, mode, *flat)),
                    Mode::Break => stack.push((indent, mode, *broken)),
                },

                Doc::Fill(mut parts) => {
                    if parts.len() < 2 {
                        // Trailing element or empty — render flat.
                        for p in parts.into_iter().rev() {
                            stack.push((indent, Mode::Flat, p));
                        }
                        continue;
                    }

                    let sep = parts.remove(0);
                    let content = parts.remove(0);

                    let sep_mode = if matches!(&sep, Doc::Hardline) {
                        // Hardline always forces a break.
                        Mode::Break
                    } else if mode == Mode::Break && matches!(&sep, Doc::PreservedLine) {
                        // PreservedLine forces break only when the enclosing
                        // Group broke (expression doesn't fit on one line).
                        // In flat mode it degrades to a regular Line so the
                        // parent Group can join everything.
                        Mode::Break
                    } else {
                        // Greedy check: does sep (flat) + content fit?
                        let test = crate::doc::concat(vec![sep.clone(), content.clone()]);
                        if self.fits(indent, &test) {
                            Mode::Flat
                        } else {
                            Mode::Break
                        }
                    };

                    // Push remaining fill (processed after this pair).
                    if !parts.is_empty() {
                        stack.push((indent, mode, Doc::Fill(parts)));
                    }

                    // Push pair: content after sep (LIFO order).
                    stack.push((indent, Mode::Flat, content));
                    stack.push((indent, sep_mode, sep));
                }

                Doc::AlignGroup(children) => {
                    self.render_align_group(children, indent);
                }

                Doc::AlignRow(cells) => {
                    // Standalone AlignRow outside a group — render as
                    // plain concatenation (should not happen in practice).
                    for cell in cells {
                        stack.push((indent, mode, cell.content));
                    }
                }
            }
        }

        self.output
    }

    // ── Alignment group rendering ─────────────────────────────────

    fn render_align_group(&mut self, children: Vec<Doc>, indent: usize) {
        // Separate AlignRow entries from non-row docs (comments, directives).
        // Also identify blank lines that break alignment groups.
        let mut groups: Vec<Vec<(usize, Vec<AlignCell>)>> = vec![Vec::new()];
        let mut non_rows: Vec<(usize, Doc)> = Vec::new(); // (position_index, doc)
        let mut position = 0usize;

        for child in children {
            match child {
                Doc::AlignRow(cells) => {
                    groups.last_mut().unwrap().push((position, cells));
                }
                Doc::BlankLine => {
                    // Blank line breaks the current alignment group.
                    non_rows.push((position, Doc::BlankLine));
                    groups.push(Vec::new());
                }
                other => {
                    // Standalone comments/directives — don't break group.
                    non_rows.push((position, other));
                }
            }
            position += 1;
        }

        // Compute column widths per group and build a position→column_widths map.
        let mut position_col_widths: Vec<Option<Vec<usize>>> = vec![None; position];
        let mut outlier_positions: Vec<bool> = vec![false; position];

        for group in &groups {
            if group.is_empty() {
                continue;
            }

            // Measure cell widths for each row.  Chain spacing state
            // across cells so that inter-cell leading spaces (added by
            // emit_with_spacing during actual rendering) are included in
            // the measured width, keeping measurement and rendering in sync.
            let measured: Vec<(usize, Vec<usize>)> = group
                .iter()
                .map(|(pos, cells)| {
                    let mut last_kind: &'static str = "";
                    let mut last_parent: &'static str = "";
                    let widths: Vec<usize> = cells
                        .iter()
                        .map(|c| {
                            let mut w = 0;
                            Self::measure_width_inner(
                                &c.content,
                                &mut w,
                                &mut last_kind,
                                &mut last_parent,
                            );
                            w
                        })
                        .collect();
                    (*pos, widths)
                })
                .collect();

            // Detect outliers on first column (name widths).
            let first_col_widths: Vec<usize> = measured
                .iter()
                .map(|(_, w)| w.first().copied().unwrap_or(0))
                .collect();
            let outliers = detect_outliers(&first_col_widths);

            // Mark outlier positions.
            for (i, (pos, _)) in measured.iter().enumerate() {
                if outliers[i] {
                    outlier_positions[*pos] = true;
                }
            }

            // Compute max column widths, excluding outliers.
            let max_cols = measured
                .iter()
                .zip(outliers.iter())
                .filter(|(_, &is_outlier)| !is_outlier)
                .map(|((_, widths), _)| widths.len())
                .max()
                .unwrap_or(0);

            let mut col_widths = vec![0usize; max_cols];
            for ((_, widths), &is_outlier) in measured.iter().zip(outliers.iter()) {
                if is_outlier {
                    continue;
                }
                for (col, &w) in widths.iter().enumerate() {
                    if col < col_widths.len() {
                        col_widths[col] = col_widths[col].max(w);
                    }
                }
            }

            // Store resolved widths for each non-outlier position.
            for (pos, _) in group {
                if !outlier_positions[*pos] {
                    position_col_widths[*pos] = Some(col_widths.clone());
                }
            }
        }

        // Merge rows and non-rows back into position order for rendering.
        let mut merged: Vec<(usize, MergedItem)> = Vec::with_capacity(position);
        for group in groups {
            for (pos, cells) in group {
                merged.push((pos, MergedItem::Row(cells)));
            }
        }
        for (pos, doc) in non_rows {
            merged.push((pos, MergedItem::Other(doc)));
        }
        merged.sort_by_key(|(pos, _)| *pos);

        for (pos, item) in merged {
            match item {
                MergedItem::Row(cells) => {
                    if outlier_positions[pos] {
                        // Render with normal single-space formatting.
                        if !self.at_line_start() {
                            self.emit_newline();
                        }
                        let indent_str = self.indent_string(indent);
                        self.output.push_str(&indent_str);
                        self.current_column = indent_str.len();
                        // Clear spacing state — the row starts fresh at indent.
                        self.last_token_kind = "";
                        self.last_token_parent_kind = "";
                        for cell in cells {
                            self.render_doc_inline(cell.content, indent);
                        }
                    } else if let Some(ref col_widths) = position_col_widths[pos] {
                        // Render with aligned padding.
                        if !self.at_line_start() {
                            self.emit_newline();
                        }
                        let indent_str = self.indent_string(indent);
                        self.output.push_str(&indent_str);
                        self.current_column = indent_str.len();
                        // Clear spacing state — the row starts fresh at indent.
                        self.last_token_kind = "";
                        self.last_token_parent_kind = "";
                        for (col_idx, cell) in cells.into_iter().enumerate() {
                            let before_len = self.output.len();
                            self.render_doc_inline(cell.content, indent);
                            let rendered_width = self.output.len() - before_len;
                            if cell.pad {
                                if let Some(&target_width) = col_widths.get(col_idx) {
                                    let pad = target_width.saturating_sub(rendered_width);
                                    if pad > 0 {
                                        self.output.push_str(&" ".repeat(pad));
                                        self.current_column += pad;
                                    }
                                }
                            }
                        }
                    }
                }
                MergedItem::Other(doc) => {
                    // Render non-row items (comments, blank lines, directives).
                    self.render_doc_inline(doc, indent);
                }
            }
        }
    }

    /// Render a Doc inline into the current output (non-stack-based, for use
    /// within alignment group rendering).
    fn render_doc_inline(&mut self, doc: Doc, indent: usize) {
        self.render_doc_inline_mode(doc, indent, Mode::Flat);
    }

    /// Inner render with explicit mode — `Flat` collapses Line/Softline,
    /// `Break` emits newlines (used when a Group doesn't fit).
    fn render_doc_inline_mode(&mut self, doc: Doc, indent: usize, mode: Mode) {
        match doc {
            Doc::Empty => {}
            Doc::Token {
                text,
                kind,
                parent_kind,
            } => {
                self.emit_with_spacing(&text, kind, parent_kind, indent);
            }
            Doc::Raw(text) => {
                for ch in text.chars() {
                    if ch == '\n' {
                        self.current_column = 0;
                    } else {
                        self.current_column += 1;
                    }
                }
                if text.ends_with(|c: char| c.is_whitespace()) {
                    self.last_token_kind = "";
                    self.last_token_parent_kind = "";
                }
                self.output.push_str(&text);
            }
            Doc::Hardline => self.emit_newline(),
            Doc::BlankLine => {
                self.emit_newline();
                self.output.push('\n');
            }
            Doc::Line | Doc::PreservedLine => match mode {
                Mode::Flat => {
                    self.output.push(' ');
                    self.current_column += 1;
                    self.last_token_kind = "";
                    self.last_token_parent_kind = "";
                }
                Mode::Break => self.emit_newline(),
            },
            Doc::Softline => {
                if mode == Mode::Break {
                    self.emit_newline();
                }
            }
            Doc::Concat(docs) => {
                for d in docs {
                    self.render_doc_inline_mode(d, indent, mode);
                }
            }
            Doc::Indent(inner) => {
                self.render_doc_inline_mode(*inner, indent + 1, mode);
            }
            Doc::Group(inner) => {
                let inner_mode = if self.fits(indent, &inner) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                self.render_doc_inline_mode(*inner, indent, inner_mode);
            }
            Doc::IfBreak { broken, flat } => match mode {
                Mode::Flat => self.render_doc_inline_mode(*flat, indent, mode),
                Mode::Break => self.render_doc_inline_mode(*broken, indent, mode),
            },
            Doc::Fill(parts) => {
                for p in parts {
                    self.render_doc_inline_mode(p, indent, mode);
                }
            }
            Doc::AlignGroup(children) => {
                self.render_align_group(children, indent);
            }
            Doc::AlignRow(cells) => {
                for cell in cells {
                    self.render_doc_inline(cell.content, indent);
                }
            }
        }
    }

    /// Measure the rendered width of a Doc without emitting output.
    fn measure_width_inner(
        doc: &Doc,
        width: &mut usize,
        last_kind: &mut &'static str,
        last_parent: &mut &'static str,
    ) {
        match doc {
            Doc::Empty => {}
            Doc::Token {
                text,
                kind,
                parent_kind,
            } => {
                if spacing::would_need_space(last_kind, last_parent, kind, parent_kind) {
                    *width += 1;
                }
                *width += text.len();
                *last_kind = kind;
                *last_parent = parent_kind;
            }
            Doc::Raw(text) => {
                // Count characters, but only on the last line if multi-line.
                if let Some(last_line) = text.lines().last() {
                    *width += last_line.len();
                }
            }
            Doc::Hardline | Doc::BlankLine => {
                // Newlines in a cell shouldn't happen, but handle gracefully.
                *width = 0;
            }
            Doc::Line | Doc::PreservedLine => {
                *width += 1; // space in flat mode
            }
            Doc::Softline => {}
            Doc::Concat(docs) => {
                for d in docs {
                    Self::measure_width_inner(d, width, last_kind, last_parent);
                }
            }
            Doc::Indent(inner) => {
                Self::measure_width_inner(inner, width, last_kind, last_parent);
            }
            Doc::Group(inner) => {
                Self::measure_width_inner(inner, width, last_kind, last_parent);
            }
            Doc::IfBreak { flat, .. } => {
                Self::measure_width_inner(flat, width, last_kind, last_parent);
            }
            Doc::Fill(parts) => {
                for p in parts {
                    Self::measure_width_inner(p, width, last_kind, last_parent);
                }
            }
            Doc::AlignGroup(children) => {
                for c in children {
                    Self::measure_width_inner(c, width, last_kind, last_parent);
                }
            }
            Doc::AlignRow(cells) => {
                for cell in cells {
                    Self::measure_width_inner(&cell.content, width, last_kind, last_parent);
                }
            }
        }
    }

    fn at_line_start(&self) -> bool {
        self.current_column == 0 || self.output.ends_with('\n')
    }

    fn emit_with_spacing(
        &mut self,
        text: &str,
        kind: &'static str,
        parent_kind: &'static str,
        indent: usize,
    ) {
        if self.at_line_start() {
            let indent_str = self.indent_string(indent);
            self.output.push_str(&indent_str);
            self.current_column = indent_str.len();
        } else if spacing::would_need_space(
            self.last_token_kind,
            self.last_token_parent_kind,
            kind,
            parent_kind,
        ) {
            self.output.push(' ');
            self.current_column += 1;
        }
        self.output.push_str(text);
        self.current_column += text.len();
        self.last_token_kind = kind;
        self.last_token_parent_kind = parent_kind;
    }

    fn emit_newline(&mut self) {
        self.output.push('\n');
        self.current_column = 0;
    }

    fn indent_string(&self, level: usize) -> String {
        match self.indent_style {
            IndentStyle::Space => " ".repeat(level * self.indent_size),
            IndentStyle::Tab => "\t".repeat(level),
        }
    }

    /// Compute the character width consumed by indentation at a given level.
    fn indent_width(&self, level: usize) -> usize {
        match self.indent_style {
            IndentStyle::Space => level * self.indent_size,
            IndentStyle::Tab => level,
        }
    }

    /// Check if a Doc fits on the remainder of the current line in Flat mode.
    fn fits(&self, indent: usize, doc: &Doc) -> bool {
        // When at line start, the first token will be preceded by
        // indentation — account for that width up front.
        let effective_column = if self.at_line_start() {
            self.indent_width(indent)
        } else {
            self.current_column
        };
        let mut remaining = self.max_line_length.saturating_sub(effective_column);
        let mut last_kind: &'static str = if self.at_line_start() {
            // At line start emit_with_spacing skips spacing, so clear
            // last_kind so fits_inner doesn't charge a phantom space.
            ""
        } else {
            self.last_token_kind
        };
        let mut last_parent: &'static str = if self.at_line_start() {
            ""
        } else {
            self.last_token_parent_kind
        };
        self.fits_inner(
            doc,
            indent,
            &mut remaining,
            &mut last_kind,
            &mut last_parent,
        )
    }

    fn fits_inner(
        &self,
        doc: &Doc,
        indent: usize,
        remaining: &mut usize,
        last_kind: &mut &'static str,
        last_parent: &mut &'static str,
    ) -> bool {
        match doc {
            Doc::Empty => true,

            Doc::Token {
                text,
                kind,
                parent_kind,
            } => {
                let space = if spacing::would_need_space(last_kind, last_parent, kind, parent_kind)
                {
                    1
                } else {
                    0
                };
                let needed = space + text.len();
                if needed > *remaining {
                    return false;
                }
                *remaining -= needed;
                *last_kind = kind;
                *last_parent = parent_kind;
                true
            }

            Doc::Raw(text) => {
                if text.contains('\n') {
                    return true;
                }
                if text.len() > *remaining {
                    return false;
                }
                *remaining -= text.len();
                true
            }

            Doc::Hardline | Doc::BlankLine => {
                // A hardline starts a new line even in flat mode.  Reset
                // `remaining` to the space available on the new line
                // (accounting for indentation) so subsequent tokens are
                // measured against the correct budget.
                *remaining = self
                    .max_line_length
                    .saturating_sub(self.indent_width(indent));
                // At line start no inter-token space is emitted, so clear
                // the spacing state to match emit_with_spacing behaviour.
                *last_kind = "";
                *last_parent = "";
                true
            }

            Doc::Line | Doc::PreservedLine => {
                if *remaining == 0 {
                    return false;
                }
                *remaining -= 1;
                true
            }

            Doc::Softline => true,

            Doc::Concat(docs) => {
                for d in docs {
                    if !self.fits_inner(d, indent, remaining, last_kind, last_parent) {
                        return false;
                    }
                }
                true
            }

            Doc::Indent(inner) => {
                self.fits_inner(inner, indent + 1, remaining, last_kind, last_parent)
            }

            Doc::Group(inner) => self.fits_inner(inner, indent, remaining, last_kind, last_parent),

            Doc::IfBreak { flat, .. } => {
                self.fits_inner(flat, indent, remaining, last_kind, last_parent)
            }

            Doc::Fill(parts) => {
                // In fit-checking treat Fill like Concat (all flat).
                for part in parts {
                    if !self.fits_inner(part, indent, remaining, last_kind, last_parent) {
                        return false;
                    }
                }
                true
            }

            // Alignment groups are always rendered in break mode (one
            // declaration per line), so they never fit on a single line.
            Doc::AlignGroup(_) => false,

            Doc::AlignRow(cells) => {
                for cell in cells {
                    if !self.fits_inner(&cell.content, indent, remaining, last_kind, last_parent) {
                        return false;
                    }
                }
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::*;
    use pascal_core::node_kind as K;

    fn default_renderer() -> Renderer {
        Renderer::new(&FmtConfig::default())
    }

    fn tok(text: &str, kind: &'static str, parent: &'static str) -> Doc {
        token(text, kind, parent)
    }

    #[test]
    fn render_single_token() {
        let doc = tok("begin", K::K_BEGIN, "");
        let result = default_renderer().render(doc);
        assert_eq!(result, "begin");
    }

    #[test]
    fn render_tokens_with_spacing() {
        let doc = concat(vec![
            tok("x", K::IDENTIFIER, ""),
            tok(":=", K::K_ASSIGN, ""),
            tok("1", K::INTEGER, ""),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "x := 1");
    }

    #[test]
    fn render_no_space_before_semicolon() {
        let doc = concat(vec![
            tok("x", K::IDENTIFIER, ""),
            tok(";", K::SEMICOLON, ""),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "x;");
    }

    #[test]
    fn render_hardline() {
        let doc = concat(vec![
            tok("a", K::IDENTIFIER, ""),
            Doc::Hardline,
            tok("b", K::IDENTIFIER, ""),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "a\nb");
    }

    #[test]
    fn render_blank_line() {
        let doc = concat(vec![
            tok("a", K::IDENTIFIER, ""),
            Doc::BlankLine,
            tok("b", K::IDENTIFIER, ""),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "a\n\nb");
    }

    #[test]
    fn render_indent() {
        let doc = concat(vec![
            tok("begin", K::K_BEGIN, ""),
            indent(concat(vec![
                Doc::Hardline,
                tok("x", K::IDENTIFIER, ""),
                tok(";", K::SEMICOLON, ""),
            ])),
            Doc::Hardline,
            tok("end", K::K_END, ""),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "begin\n  x;\nend");
    }

    #[test]
    fn render_raw_no_spacing() {
        let doc = concat(vec![
            tok("x", K::IDENTIFIER, ""),
            Doc::Raw(" // comment".into()),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "x // comment");
    }

    #[test]
    fn render_empty_is_identity() {
        let doc = concat(vec![Doc::Empty, tok("x", K::IDENTIFIER, ""), Doc::Empty]);
        let result = default_renderer().render(doc);
        assert_eq!(result, "x");
    }

    #[test]
    fn render_group_flat_when_fits() {
        let doc = group(concat(vec![
            tok("a", K::IDENTIFIER, ""),
            Doc::Line,
            tok("b", K::IDENTIFIER, ""),
        ]));
        let result = default_renderer().render(doc);
        assert_eq!(result, "a b");
    }

    #[test]
    fn render_group_breaks_when_overflow() {
        let config = FmtConfig {
            max_line_length: 5,
            ..FmtConfig::default()
        };
        let doc = group(concat(vec![
            tok("aaa", K::IDENTIFIER, ""),
            Doc::Line,
            tok("bbb", K::IDENTIFIER, ""),
        ]));
        let result = Renderer::new(&config).render(doc);
        assert_eq!(result, "aaa\nbbb");
    }

    #[test]
    fn render_group_with_indent_on_break() {
        let config = FmtConfig {
            max_line_length: 5,
            ..FmtConfig::default()
        };
        let doc = group(concat(vec![
            tok("aaa", K::IDENTIFIER, ""),
            indent(concat(vec![Doc::Line, tok("bbb", K::IDENTIFIER, "")])),
        ]));
        let result = Renderer::new(&config).render(doc);
        assert_eq!(result, "aaa\n  bbb");
    }

    #[test]
    fn render_softline_nothing_when_flat() {
        let doc = group(concat(vec![
            tok("a", K::IDENTIFIER, ""),
            Doc::Softline,
            tok("b", K::IDENTIFIER, ""),
        ]));
        let result = default_renderer().render(doc);
        assert_eq!(result, "a b");
    }

    #[test]
    fn render_if_break_flat() {
        let doc = group(concat(vec![
            tok("a", K::IDENTIFIER, ""),
            if_break(Doc::Hardline, Doc::Raw(" ".into())),
            tok("b", K::IDENTIFIER, ""),
        ]));
        let result = default_renderer().render(doc);
        assert_eq!(result, "a b");
    }

    #[test]
    fn render_if_break_broken() {
        let config = FmtConfig {
            max_line_length: 3,
            ..FmtConfig::default()
        };
        let doc = group(concat(vec![
            tok("aa", K::IDENTIFIER, ""),
            if_break(Doc::Hardline, Doc::Raw(" ".into())),
            tok("bb", K::IDENTIFIER, ""),
        ]));
        let result = Renderer::new(&config).render(doc);
        assert_eq!(result, "aa\nbb");
    }

    #[test]
    fn render_fill_all_flat_when_fits() {
        // All items fit on one line — everything stays flat.
        let doc = fill(vec![
            Doc::Line,
            tok("a", K::IDENTIFIER, ""),
            Doc::Line,
            tok("b", K::IDENTIFIER, ""),
            Doc::Line,
            tok("c", K::IDENTIFIER, ""),
        ]);
        let result = default_renderer().render(doc);
        assert_eq!(result, " a b c");
    }

    #[test]
    fn render_fill_greedy_break() {
        // With a narrow line, Fill should break greedily: pack as many
        // items as fit per line.
        let config = FmtConfig {
            max_line_length: 12,
            ..FmtConfig::default()
        };
        let doc = concat(vec![
            tok("xx", K::IDENTIFIER, ""),
            indent(fill(vec![
                Doc::Line,
                tok("+", K::K_ADD, ""),
                tok("aa", K::IDENTIFIER, ""),
                Doc::Line,
                tok("+", K::K_ADD, ""),
                tok("bb", K::IDENTIFIER, ""),
                Doc::Line,
                tok("+", K::K_ADD, ""),
                tok("cc", K::IDENTIFIER, ""),
            ])),
        ]);
        let result = Renderer::new(&config).render(doc);
        // "xx + aa" = 7 chars, then " + bb" = 5 more → 12, fits.
        // " + cc" = 5 more → 17, doesn't fit → breaks.
        assert_eq!(result, "xx + aa + bb\n  + cc");
    }
}
