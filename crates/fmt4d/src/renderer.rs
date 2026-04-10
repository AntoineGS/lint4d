use crate::config::{FmtConfig, IndentStyle};
use crate::doc::Doc;
use crate::spacing;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    Break,
}

pub struct Renderer {
    output: String,
    current_column: usize,
    indent_size: usize,
    indent_style: IndentStyle,
    max_line_length: usize,
    last_token_kind: String,
    last_token_parent_kind: String,
}

impl Renderer {
    pub fn new(config: &FmtConfig) -> Self {
        Renderer {
            output: String::new(),
            current_column: 0,
            indent_size: config.indent_size,
            indent_style: config.indent_style,
            max_line_length: config.max_line_length,
            last_token_kind: String::new(),
            last_token_parent_kind: String::new(),
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
                    self.emit_with_spacing(&text, &kind, &parent_kind, indent);
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
                        self.last_token_kind.clear();
                        self.last_token_parent_kind.clear();
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
                        self.last_token_kind.clear();
                        self.last_token_parent_kind.clear();
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
            }
        }

        self.output
    }

    fn at_line_start(&self) -> bool {
        self.current_column == 0 || self.output.ends_with('\n')
    }

    fn emit_with_spacing(&mut self, text: &str, kind: &str, parent_kind: &str, indent: usize) {
        if self.at_line_start() {
            let indent_str = self.indent_string(indent);
            self.output.push_str(&indent_str);
            self.current_column = indent_str.len();
        } else if spacing::would_need_space(
            &self.last_token_kind,
            &self.last_token_parent_kind,
            kind,
            parent_kind,
        ) {
            self.output.push(' ');
            self.current_column += 1;
        }
        self.output.push_str(text);
        self.current_column += text.len();
        self.last_token_kind = kind.to_string();
        self.last_token_parent_kind = parent_kind.to_string();
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
        let mut last_kind = if self.at_line_start() {
            // At line start emit_with_spacing skips spacing, so clear
            // last_kind so fits_inner doesn't charge a phantom space.
            String::new()
        } else {
            self.last_token_kind.clone()
        };
        let mut last_parent = if self.at_line_start() {
            String::new()
        } else {
            self.last_token_parent_kind.clone()
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
        last_kind: &mut String,
        last_parent: &mut String,
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
                *last_kind = kind.clone();
                *last_parent = parent_kind.clone();
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
                last_kind.clear();
                last_parent.clear();
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

    fn tok(text: &str, kind: &str, parent: &str) -> Doc {
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
        let mut config = FmtConfig::default();
        config.max_line_length = 5;
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
        let mut config = FmtConfig::default();
        config.max_line_length = 5;
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
        let mut config = FmtConfig::default();
        config.max_line_length = 3;
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
        let mut config = FmtConfig::default();
        config.max_line_length = 12;
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
