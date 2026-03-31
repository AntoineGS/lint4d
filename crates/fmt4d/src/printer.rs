use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::indent::IndentContext;
use crate::spacing;
use crate::uses;
use pascal_core::FormatOffRegion;
use std::collections::HashSet;
use tree_sitter::Node;

/// AST-walking printer that emits formatted Delphi source.
pub struct Printer<'a> {
    source: &'a [u8],
    config: &'a FmtConfig,
    indent: IndentContext,
    comments: &'a CommentMap,
    format_regions: Vec<FormatOffRegion>,
    external_units: HashSet<String>,
    output: String,
    /// True if we are at the start of a new line (need indent on next token).
    at_line_start: bool,
    /// The kind of the last emitted leaf token.
    last_token_kind: String,
}

impl<'a> Printer<'a> {
    pub fn new(
        source: &'a [u8],
        config: &'a FmtConfig,
        comments: &'a CommentMap,
        format_regions: Vec<FormatOffRegion>,
        external_units: HashSet<String>,
    ) -> Self {
        Printer {
            source,
            config,
            indent: IndentContext::new(config.indent_size, config.indent_style),
            comments,
            format_regions,
            external_units,
            output: String::new(),
            at_line_start: true,
            last_token_kind: String::new(),
        }
    }

    pub fn result(self) -> String {
        self.output
    }

    // ── Main dispatch ────────────────────────────────────────────

    pub fn print_node(&mut self, node: Node) {
        // If the node falls entirely within a format-off region, emit verbatim.
        if self.is_in_format_off_region(node) {
            self.emit_verbatim(node);
            return;
        }

        // Emit leading comments for this node.
        self.emit_leading_comments(node);

        match node.kind() {
            "unit" => self.print_unit(node),
            "interface" => self.print_interface_section(node),
            "implementation" => self.print_implementation_section(node),
            "declUses" => self.print_uses(node),
            "block" => self.print_block(node),
            "declClass" | "declRecord" | "declIntf" => self.print_type_body(node),
            "declSection" => self.print_decl_section(node),
            "declVars" | "declConsts" | "declTypes" => self.print_section(node),
            "defProc" => self.print_def_proc(node),
            "try" => self.print_try(node),
            "case" => self.print_case(node),
            "repeat" => self.print_repeat(node),
            "if" | "ifElse" => self.print_if(node),
            "for" | "foreach" | "while" | "with" => self.print_loop(node),
            _ if node.child_count() == 0 && !node.is_extra() => {
                self.print_leaf(node);
            }
            _ => {
                self.recurse_children(node);
            }
        }
    }

    // ── Recursion helper ─────────────────────────────────────────

    fn recurse_children(&mut self, node: Node) {
        for child in node.children(&mut node.walk()) {
            if !child.is_extra() {
                self.print_node(child);
            }
        }
    }

    // ── Leaf token emission ──────────────────────────────────────

    fn print_leaf(&mut self, node: Node) {
        let kind = node.kind();
        let text = self.node_text(node);
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();

        match kind {
            ";" => {
                self.emit_raw(";");
                self.last_token_kind = ";".to_string();
                self.emit_trailing_comments(node);
                self.ensure_newline();
            }
            "kBegin" | "kTry" | "kRepeat" | "kAsm" => {
                self.ensure_newline();
                self.emit_indented(&text);
                self.last_token_kind = kind.to_string();
                self.ensure_newline();
                self.indent.indent();
            }
            "kEnd" => {
                self.indent.dedent();
                self.ensure_newline();
                self.emit_indented(&text);
                self.last_token_kind = kind.to_string();
            }
            "kExcept" | "kFinally" => {
                self.indent.dedent();
                self.ensure_newline();
                self.emit_indented(&text);
                self.last_token_kind = kind.to_string();
                self.ensure_newline();
                self.indent.indent();
            }
            _ => {
                self.emit_token(kind, &text, &parent_kind);
                self.last_token_kind = kind.to_string();
                self.emit_trailing_comments(node);
            }
        }
    }

    /// Emit a token with proper spacing.
    fn emit_token(&mut self, kind: &str, text: &str, parent_kind: &str) {
        if self.at_line_start {
            self.emit_indented(text);
        } else {
            let needs_space = self.needs_space_before(kind, parent_kind);
            if needs_space {
                self.output.push(' ');
            }
            self.output.push_str(text);
        }
    }

    fn needs_space_before(&self, kind: &str, parent_kind: &str) -> bool {
        // No space before `)`, `]`, or `.`
        if kind == ")" || kind == "]" || kind == "." {
            return false;
        }
        // No space after `(`, `[`, or `.`
        if self.last_token_kind == "(" || self.last_token_kind == "[" || self.last_token_kind == "."
        {
            return false;
        }
        // No space before `;`
        if kind == ";" {
            return false;
        }
        // No space before `,`
        if kind == "," {
            return false;
        }
        // No space before `(`  in call/args context
        if kind == "("
            && (parent_kind == "exprCall"
                || parent_kind == "declArgs"
                || parent_kind == "exprParens")
        {
            return false;
        }
        // No space before `:` in declarations (declField, declVar, declArg)
        // But do space before `:` in ternary-like contexts — Delphi doesn't have those,
        // so always no space before `:` is fine.
        if kind == ":" {
            return false;
        }
        // kDot / `.` inside genericDot
        if kind == "kDot" {
            return false;
        }
        // After kDot, no space
        if self.last_token_kind == "kDot" || self.last_token_kind == "." {
            return false;
        }
        // Use spacing module for known operators
        if spacing::space_before(kind) {
            return true;
        }
        // Space after operator-like tokens
        if spacing::space_after(&self.last_token_kind) {
            return true;
        }
        // Space after keywords that expect an expression/identifier
        if is_keyword_needing_space_after(&self.last_token_kind) {
            return true;
        }
        // Default: space between two identifiers/keywords/literals
        if !self.last_token_kind.is_empty()
            && self.last_token_kind != ";"
            && self.last_token_kind != "("
            && self.last_token_kind != "["
        {
            return true;
        }
        false
    }

    // ── Unit structure ─────────────────────────────────────────

    fn print_unit(&mut self, node: Node) {
        // Children: kUnit, moduleName, `;`, interface, implementation, kEnd, kEndDot
        let mut prev_kind = String::new();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            let kind = child.kind().to_string();
            // Blank line before interface, implementation, and final kEnd
            if (kind == "interface" || kind == "implementation" || kind == "kEnd")
                && !prev_kind.is_empty()
            {
                self.ensure_newline();
                self.emit_newline();
            }
            self.print_node(child);
            prev_kind = kind;
        }
    }

    fn print_interface_section(&mut self, node: Node) {
        // Children: kInterface, then declUses/declTypes/declVars/declConsts etc.
        let mut after_header = false;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kInterface" => {
                    self.emit_indented("interface");
                    self.last_token_kind = "kInterface".to_string();
                    self.ensure_newline();
                    after_header = true;
                }
                _ => {
                    if after_header {
                        self.emit_newline();
                        after_header = false;
                    }
                    self.print_node(child);
                }
            }
        }
    }

    fn print_implementation_section(&mut self, node: Node) {
        // Children: kImplementation, then defProc/declUses/etc.
        let mut after_header = false;
        let mut prev_child_kind = String::new();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kImplementation" => {
                    self.emit_indented("implementation");
                    self.last_token_kind = "kImplementation".to_string();
                    self.ensure_newline();
                    after_header = true;
                }
                _ => {
                    let kind = child.kind().to_string();
                    // Blank line after header or between top-level decls
                    if after_header {
                        self.emit_newline();
                        after_header = false;
                    } else if !prev_child_kind.is_empty() {
                        let blanks = crate::blank_lines::needs_blank_line_between(
                            &prev_child_kind,
                            &kind,
                            &self.config.blank_lines,
                        );
                        for _ in 0..blanks {
                            self.ensure_newline();
                            self.emit_newline();
                        }
                    }
                    self.print_node(child);
                    prev_child_kind = kind;
                }
            }
        }
    }

    // ── Block printing ───────────────────────────────────────────

    fn print_block(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    // ── Type body (class/record/interface) ───────────────────────

    fn print_type_body(&mut self, node: Node) {
        // The opening keyword (class/record/interface) comes first.
        // Then body children, then kEnd.
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kClass" | "kRecord" | "kInterface" | "kObject" => {
                    // Emit the keyword on same line as `= class`
                    self.emit_token(child.kind(), &self.node_text(child), "");
                    self.last_token_kind = child.kind().to_string();
                    self.ensure_newline();
                    // Don't indent here — declSection handles its own indent
                }
                "kEnd" => {
                    self.ensure_newline();
                    self.emit_indented("end");
                    self.last_token_kind = "kEnd".to_string();
                }
                "declSection" => {
                    self.print_decl_section(child);
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
    }

    // ── Visibility section (public/private/etc.) ─────────────────

    fn print_decl_section(&mut self, node: Node) {
        let mut first = true;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kPublic" | "kPrivate" | "kProtected" | "kPublished" | "kStrict" => {
                    if first {
                        self.ensure_newline();
                        self.emit_indented(&self.node_text(child));
                        self.last_token_kind = child.kind().to_string();
                        self.ensure_newline();
                        self.indent.indent();
                        first = false;
                    } else {
                        // Strict private/protected: emit space then the keyword
                        self.output.push(' ');
                        self.output.push_str(&self.node_text(child));
                        self.last_token_kind = child.kind().to_string();
                        self.ensure_newline();
                        self.indent.indent();
                    }
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
        if !first {
            self.indent.dedent();
        }
    }

    // ── Section blocks (var/const/type) ──────────────────────────

    fn print_section(&mut self, node: Node) {
        self.emit_leading_comments(node);
        let mut indented = false;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kVar" | "kConst" | "kType" => {
                    self.ensure_newline();
                    self.emit_indented(&self.node_text(child));
                    self.last_token_kind = child.kind().to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    indented = true;
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
        if indented {
            self.indent.dedent();
        }
    }

    // ── Procedure/function definition ────────────────────────────

    fn print_def_proc(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    // ── Uses clause ──────────────────────────────────────────────

    fn print_uses(&mut self, node: Node) {
        self.emit_leading_comments(node);
        let units = uses::extract_uses_units(node, self.source);
        self.ensure_newline();
        self.emit_indented("uses");
        self.last_token_kind = "kUses".to_string();
        self.ensure_newline();
        self.indent.indent();
        let indent_str = self.indent.current();
        let formatted =
            uses::format_uses(&units, &self.config.uses, &indent_str, &self.external_units);
        self.output.push_str(&formatted);
        self.at_line_start = true;
        self.last_token_kind = ";".to_string();
        self.indent.dedent();
    }

    // ── Try..except/finally ──────────────────────────────────────

    fn print_try(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    // ── Case statement ───────────────────────────────────────────

    fn print_case(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    // ── Repeat..until ────────────────────────────────────────────

    fn print_repeat(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    // ── If/ifElse ────────────────────────────────────────────────

    fn print_if(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kElse" => {
                    // `else` aligns with `if` — no extra indent
                    self.ensure_newline();
                    self.emit_indented("else");
                    self.last_token_kind = "kElse".to_string();
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
    }

    // ── Loops (for/while/with) ───────────────────────────────────

    fn print_loop(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    // ── Comment emission ─────────────────────────────────────────

    fn emit_leading_comments(&mut self, node: Node) {
        let comments = self.comments.leading_comments(node.id());
        for comment in comments {
            self.ensure_newline();
            self.emit_indented(&comment.text);
            self.ensure_newline();
        }
    }

    fn emit_trailing_comments(&mut self, node: Node) {
        let comments = self.comments.trailing_comments(node.id());
        for comment in comments {
            self.output.push(' ');
            self.output.push_str(&comment.text);
        }
    }

    // ── Output helpers ───────────────────────────────────────────

    fn emit_indented(&mut self, text: &str) {
        if self.at_line_start {
            self.output.push_str(&self.indent.current());
            self.at_line_start = false;
        }
        self.output.push_str(text);
    }

    fn emit_raw(&mut self, text: &str) {
        self.output.push_str(text);
    }

    fn ensure_newline(&mut self) {
        if !self.at_line_start {
            self.output.push('\n');
            self.at_line_start = true;
        }
    }

    fn emit_newline(&mut self) {
        self.output.push('\n');
        self.at_line_start = true;
    }

    fn node_text(&self, node: Node) -> String {
        std::str::from_utf8(&self.source[node.start_byte()..node.end_byte()])
            .unwrap_or("")
            .to_string()
    }

    /// Check if a node falls entirely within a format-off region.
    fn is_in_format_off_region(&self, node: Node) -> bool {
        let node_start = node.start_position().row + 1; // 1-based
        let node_end = node.end_position().row + 1; // 1-based
        self.format_regions
            .iter()
            .any(|r| node_start >= r.start_line && node_end <= r.end_line)
    }

    /// Emit the original source text for a node verbatim (no formatting).
    fn emit_verbatim(&mut self, node: Node) {
        let text = self.node_text(node);
        self.output.push_str(&text);
        // Update state: check if text ends with a newline
        self.at_line_start = text.ends_with('\n');
        if let Some(last_line) = text.lines().last() {
            // crude: pick up the last "word" as last_token_kind
            if let Some(word) = last_line.split_whitespace().last() {
                self.last_token_kind = word.to_string();
            }
        }
    }
}

/// Keywords after which a space is always needed.
fn is_keyword_needing_space_after(kind: &str) -> bool {
    matches!(
        kind,
        "kProcedure"
            | "kFunction"
            | "kConstructor"
            | "kDestructor"
            | "kClass"
            | "kRecord"
            | "kProperty"
            | "kRaise"
            | "kInherited"
            | "kWith"
            | "kArray"
            | "kSet"
            | "kFile"
            | "kString"
            | "kProgram"
            | "kLibrary"
            | "kUnit"
            | "kUses"
            | "kOf"
            | "kThen"
            | "kDo"
            | "kTo"
            | "kDownto"
            | "kElse"
    )
}
