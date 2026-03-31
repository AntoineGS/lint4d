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
    /// The parent kind of the last emitted leaf token.
    last_token_parent_kind: String,
    /// Current column position (number of chars since last newline).
    current_column: usize,
    /// Maximum allowed line length (from config).
    /// Stored for future use by line-breaking logic.
    #[allow(dead_code)]
    max_line_length: usize,
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
            last_token_parent_kind: String::new(),
            current_column: 0,
            max_line_length: config.max_line_length,
        }
    }

    pub fn result(self) -> String {
        self.output
    }

    pub fn current_column(&self) -> usize {
        self.current_column
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
            "initialization" | "finalization" => self.print_init_final_section(node),
            "declUses" => self.print_uses(node),
            "block" | "statements" => self.print_block(node),
            "declClass" | "declRecord" | "declIntf" => self.print_type_body(node),
            "declSection" => self.print_decl_section(node),
            "declVars" | "declConsts" | "declTypes" => self.print_section(node),
            "defProc" => self.print_def_proc(node),
            "declProc" => self.print_decl_proc(node),
            "try" => self.print_try(node),
            "case" => self.print_case(node),
            "repeat" => self.print_repeat(node),
            "if" | "ifElse" => self.print_if(node),
            "for" | "foreach" | "while" | "with" => self.print_loop(node),
            // Emit literalChar / literalString as verbatim text (children don't
            // cover the full text, e.g. `#0` has child `#` but not `0`).
            "literalChar" | "literalString" => self.print_verbatim_leaf(node),
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

    /// Emit a node's full source text as a single token, ignoring children.
    /// Used for nodes like `literalChar` where the children don't cover
    /// all source text (e.g. `#0` has child `#` but the `0` is not a child).
    fn print_verbatim_leaf(&mut self, node: Node) {
        let kind = node.kind();
        let text = self.node_text(node);
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();
        self.emit_token(kind, &text, &parent_kind);
        self.set_last_token(kind, &parent_kind);
        self.emit_trailing_comments(node);
    }

    fn print_leaf(&mut self, node: Node) {
        let kind = node.kind();
        let text = self.node_text(node);
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();

        match kind {
            ";" => {
                // Inside a parameter list (declArgs), `;` separates params
                // but should NOT force a newline.  If line-breaking is needed,
                // the line_break pass handles it with continuation indent.
                let in_param_list = Self::is_ancestor(node, "declArgs");
                self.emit_raw(";");
                self.set_last_token(";", &parent_kind);
                self.emit_trailing_comments(node);
                if in_param_list {
                    // Space instead of newline — params continue on same line
                    self.output.push(' ');
                    self.current_column += 1;
                } else {
                    self.ensure_newline();
                }
            }
            "kBegin" | "kTry" | "kRepeat" | "kAsm" => {
                self.ensure_newline();
                self.emit_indented(&text);
                self.set_last_token(kind, &parent_kind);
                self.ensure_newline();
                self.indent.indent();
            }
            "kEnd" => {
                self.indent.dedent();
                self.ensure_newline();
                self.emit_indented(&text);
                self.set_last_token(kind, &parent_kind);
            }
            "kExcept" | "kFinally" => {
                self.indent.dedent();
                self.ensure_newline();
                self.emit_indented(&text);
                self.set_last_token(kind, &parent_kind);
                self.ensure_newline();
                self.indent.indent();
            }
            _ => {
                self.emit_token(kind, &text, &parent_kind);
                self.set_last_token(kind, &parent_kind);
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
                self.current_column += 1;
            }
            self.current_column += text.len();
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
        // No space before `[` in subscript context (array/string indexing)
        if kind == "[" && parent_kind == "exprSubscript" {
            return false;
        }
        // No space before `(` in call/args context (but NOT exprParens —
        // `if (x)` must keep the space between keyword and `(`).
        if kind == "(" && (parent_kind == "exprCall" || parent_kind == "declArgs") {
            return false;
        }
        // No spaces inside generic angle brackets: TList<Integer> not TList < Integer >
        if (kind == "kLt" || kind == "<")
            && (parent_kind == "typerefTpl"
                || parent_kind == "genericTpl"
                || parent_kind == "genericArgs"
                || parent_kind == "typerefArgs"
                || parent_kind == "exprTpl")
        {
            return false;
        }
        if (kind == "kGt" || kind == ">")
            && (parent_kind == "typerefTpl"
                || parent_kind == "genericTpl"
                || parent_kind == "genericArgs"
                || parent_kind == "typerefArgs"
                || parent_kind == "exprTpl")
        {
            return false;
        }
        // No space after `<` in generic context only
        if self.last_token_kind == "kLt" && is_generic_parent(&self.last_token_parent_kind) {
            return false;
        }
        // No space before `:` in declarations
        if kind == ":" {
            return false;
        }
        // No space around `..` in range expressions
        if kind == ".." || self.last_token_kind == ".." {
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
            // Blank line before interface, implementation, initialization, finalization, and final kEnd
            if (kind == "interface"
                || kind == "implementation"
                || kind == "initialization"
                || kind == "finalization"
                || kind == "kEnd")
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
        let mut prev_child_kind = String::new();
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
                    let kind = child.kind().to_string();
                    if after_header {
                        self.emit_newline();
                        after_header = false;
                    } else if !prev_child_kind.is_empty() {
                        // Add blank line between sections (uses→types, types→vars, etc.)
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

    fn print_init_final_section(&mut self, node: Node) {
        // Children: kInitialization/kFinalization, then statement(s)
        let mut after_header = false;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kInitialization" | "kFinalization" => {
                    let text = self.node_text(child);
                    self.emit_indented(&text);
                    self.last_token_kind = child.kind().to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    after_header = true;
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
        if after_header {
            self.indent.dedent();
        }
    }

    // ── Block printing ───────────────────────────────────────────

    fn print_block(&mut self, node: Node) {
        self.emit_leading_comments(node);
        self.print_children_preserving_blank_lines(node);
    }

    /// Print children of a node while preserving single blank lines from the
    /// source between consecutive children.  Blank lines immediately after
    /// block openers (begin/try/repeat) or before closers (end) are stripped.
    fn print_children_preserving_blank_lines(&mut self, node: Node) {
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();
        let mut prev_end_row: Option<usize> = None;
        let mut prev_kind: &str = "";
        for (i, child) in children.iter().enumerate() {
            let kind = child.kind();
            if let Some(prev_end) = prev_end_row {
                // Don't insert blank line right after begin/try/repeat
                let after_opener = is_block_opener(prev_kind);
                // Don't insert blank line right before end/except/finally
                let before_closer = is_block_closer(kind);
                if !after_opener
                    && !before_closer
                    && self.has_blank_line_between(prev_end, child.start_position().row)
                {
                    self.ensure_newline();
                    self.emit_newline();
                }
            }
            self.print_node(*child);
            prev_end_row = Some(child.end_position().row);
            prev_kind = kind;
            let _ = i;
        }
    }

    // ── Type body (class/record/interface) ───────────────────────

    fn print_type_body(&mut self, node: Node) {
        // The opening keyword (class/record/interface) comes first.
        // Then optionally an ancestor list `(...)`, then body children, then kEnd.
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();

        // Detect whether this type body has declSection children (visibility
        // sections like private/public). If not, we need to indent the body
        // fields ourselves (e.g. plain records without visibility).
        let has_visibility_sections = children.iter().any(|c| c.kind() == "declSection");
        let mut body_indented = false;
        // Track whether we're still in the ancestor list portion (before
        // the first body child or kEnd).
        let mut in_ancestor_list = false;

        for child in &children {
            match child.kind() {
                "kClass" | "kRecord" | "kInterface" | "kObject" => {
                    // Emit the keyword on same line as `= class`
                    self.emit_token(child.kind(), &self.node_text(*child), "");
                    self.last_token_kind = child.kind().to_string();
                    in_ancestor_list = true;
                    // DON'T newline yet — ancestor list `(...)` may follow
                }
                // Ancestor list tokens: `(`, `)`, `,`, typeref — keep on same line
                "(" if in_ancestor_list => {
                    self.emit_raw("(");
                    self.last_token_kind = "(".to_string();
                }
                ")" if in_ancestor_list => {
                    self.emit_raw(")");
                    self.last_token_kind = ")".to_string();
                    in_ancestor_list = false;
                    self.ensure_newline();
                }
                "," if in_ancestor_list => {
                    self.emit_raw(",");
                    self.output.push(' ');
                    self.current_column += 1;
                    self.last_token_kind = ",".to_string();
                }
                "typeref" if in_ancestor_list => {
                    let text = self.node_text(*child);
                    self.current_column += text.len();
                    self.output.push_str(&text);
                    self.last_token_kind = "identifier".to_string();
                }
                "kEnd" => {
                    // End ancestor list if still in it (e.g. empty class with no ancestors)
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        self.ensure_newline();
                    }
                    if body_indented {
                        self.indent.dedent();
                        body_indented = false;
                    }
                    self.ensure_newline();
                    self.emit_indented("end");
                    self.last_token_kind = "kEnd".to_string();
                }
                "declSection" => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        self.ensure_newline();
                    }
                    self.print_decl_section(*child);
                }
                _ => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        self.ensure_newline();
                    }
                    // For records/types without visibility sections, indent body fields
                    if !has_visibility_sections && !body_indented && child.kind() != ";" {
                        self.indent.indent();
                        body_indented = true;
                    }
                    self.print_node(*child);
                }
            }
        }
        if body_indented {
            self.indent.dedent();
        }
    }

    // ── Visibility section (public/private/etc.) ─────────────────

    fn print_decl_section(&mut self, node: Node) {
        let mut first = true;
        let mut prev_end_row: Option<usize> = None;
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
                        let kw_text = self.node_text(child);
                        self.output.push(' ');
                        self.current_column += 1;
                        self.current_column += kw_text.len();
                        self.output.push_str(&kw_text);
                        self.last_token_kind = child.kind().to_string();
                        self.ensure_newline();
                        self.indent.indent();
                    }
                    prev_end_row = Some(child.end_position().row);
                }
                _ => {
                    // Preserve blank lines between members
                    if let Some(prev_end) = prev_end_row {
                        if self.has_blank_line_between(prev_end, child.start_position().row) {
                            self.ensure_newline();
                            self.emit_newline();
                        }
                    }
                    self.print_node(child);
                    prev_end_row = Some(child.end_position().row);
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
        let mut prev_child_kind = String::new();
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
                    let kind = child.kind().to_string();
                    // Insert blank line between type declarations in a type section
                    if kind == "declType" && prev_child_kind == "declType" {
                        self.ensure_newline();
                        self.emit_newline();
                    }
                    self.print_node(child);
                    prev_child_kind = kind;
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

    // ── Procedure/function declaration ──────────────────────────
    // Handles `declProc` so that method directives (override, virtual,
    // abstract, reintroduce, etc.) stay on the same line as the method
    // signature instead of being split across lines.

    fn print_decl_proc(&mut self, node: Node) {
        self.emit_leading_comments(node);
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                ";" => {
                    // Check if next sibling is a procAttribute — if so, emit
                    // the semicolon WITHOUT a newline to keep directives on
                    // the same line.
                    let next = children.get(i + 1);
                    if next.map(|n| n.kind()) == Some("procAttribute") {
                        self.emit_raw(";");
                        self.last_token_kind = ";".to_string();
                        self.emit_trailing_comments(child);
                        // Emit a space instead of newline before the directive
                        self.output.push(' ');
                        self.current_column += 1;
                        self.at_line_start = false;
                    } else {
                        // Normal semicolon — emit with newline
                        self.emit_raw(";");
                        self.last_token_kind = ";".to_string();
                        self.emit_trailing_comments(child);
                        self.ensure_newline();
                    }
                }
                _ => {
                    self.print_node(child);
                }
            }
            i += 1;
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
        self.current_column = 0;
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
        let mut after_of = false;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                "kCase" => {
                    self.emit_indented("case");
                    self.last_token_kind = "kCase".to_string();
                }
                "kOf" => {
                    self.output.push(' ');
                    self.current_column += 1;
                    self.output.push_str("of");
                    self.current_column += "of".len();
                    self.last_token_kind = "kOf".to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    after_of = true;
                }
                "caseCase" => {
                    self.print_case_branch(child);
                }
                "kElse" => {
                    // `else` aligns with `case`, not with the branches
                    self.indent.dedent();
                    self.emit_leading_comments(child);
                    self.ensure_newline();
                    self.emit_indented("else");
                    self.last_token_kind = "kElse".to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    // after_of stays true — kEnd will do the final dedent
                }
                "kEnd" => {
                    if after_of {
                        self.indent.dedent();
                        after_of = false;
                    }
                    self.emit_leading_comments(child);
                    self.ensure_newline();
                    self.emit_indented("end");
                    self.last_token_kind = "kEnd".to_string();
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
        if after_of {
            self.indent.dedent();
        }
    }

    fn print_case_branch(&mut self, node: Node) {
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
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                "kThen" | "kDo" => {
                    // Emit the keyword on the same line
                    self.print_node(child);
                    // If the NEXT child is a single statement (not begin..end),
                    // put it on its own indented line.
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != "block" && next.kind() != "kElse" {
                            self.ensure_newline();
                            self.indent.indent();
                            self.print_node(*next);
                            self.indent.dedent();
                            i += 2;
                            continue;
                        }
                    }
                }
                "kElse" => {
                    // `else` aligns with `if` — no extra indent
                    self.ensure_newline();
                    self.emit_indented("else");
                    self.last_token_kind = "kElse".to_string();
                    // If next child is a single statement (not begin..end or another if),
                    // put it on its own indented line.
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != "block" && next.kind() != "if" && next.kind() != "ifElse"
                        {
                            self.ensure_newline();
                            self.indent.indent();
                            self.print_node(*next);
                            self.indent.dedent();
                            i += 2;
                            continue;
                        }
                    }
                }
                _ => {
                    self.print_node(child);
                }
            }
            i += 1;
        }
    }

    // ── Loops (for/while/with) ───────────────────────────────────

    fn print_loop(&mut self, node: Node) {
        self.emit_leading_comments(node);
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                "kDo" => {
                    // Emit `do` on the same line
                    self.print_node(child);
                    // If next child is a single statement (not begin..end),
                    // put it on its own indented line.
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != "block" {
                            self.ensure_newline();
                            self.indent.indent();
                            self.print_node(*next);
                            self.indent.dedent();
                            i += 2;
                            continue;
                        }
                    }
                }
                _ => {
                    self.print_node(child);
                }
            }
            i += 1;
        }
    }

    // ── Comment emission ─────────────────────────────────────────

    fn emit_leading_comments(&mut self, node: Node) {
        let comments = self.comments.leading_comments(node.id());
        if comments.is_empty() {
            return;
        }
        for comment in comments {
            self.ensure_newline();
            self.emit_indented(&comment.text);
            self.ensure_newline();
        }
        // If there was a blank line in the source between the last comment
        // and the node itself, preserve it.
        let last_comment = &comments[comments.len() - 1];
        // Comment may span multiple lines; estimate its end row
        let comment_end_row =
            last_comment.source_row + last_comment.text.lines().count().saturating_sub(1);
        let node_start_row = node.start_position().row;
        if self.has_blank_line_between(comment_end_row, node_start_row) {
            self.emit_newline();
        }
    }

    fn emit_trailing_comments(&mut self, node: Node) {
        let comments = self.comments.trailing_comments(node.id());
        for comment in comments {
            self.output.push(' ');
            self.output.push_str(&comment.text);
            self.current_column += 1 + comment.text.len();
        }
    }

    // ── Output helpers ───────────────────────────────────────────

    fn emit_indented(&mut self, text: &str) {
        if self.at_line_start {
            let indent_str = self.indent.current();
            self.current_column = indent_str.len() + text.len();
            self.output.push_str(&indent_str);
            self.at_line_start = false;
        } else {
            self.current_column += text.len();
        }
        self.output.push_str(text);
    }

    fn emit_raw(&mut self, text: &str) {
        self.current_column += text.len();
        self.output.push_str(text);
    }

    fn ensure_newline(&mut self) {
        if !self.at_line_start {
            self.output.push('\n');
            self.at_line_start = true;
            self.current_column = 0;
        }
    }

    fn emit_newline(&mut self) {
        self.output.push('\n');
        self.at_line_start = true;
        self.current_column = 0;
    }

    fn node_text(&self, node: Node) -> String {
        std::str::from_utf8(&self.source[node.start_byte()..node.end_byte()])
            .unwrap_or("")
            .replace('\r', "")
    }

    /// Check if there is a blank line (empty or whitespace-only row) in the
    /// source between `start_row` (exclusive) and `end_row` (exclusive).
    fn has_blank_line_between(&self, start_row: usize, end_row: usize) -> bool {
        if end_row <= start_row + 1 {
            return false;
        }
        let source_str = std::str::from_utf8(self.source).unwrap_or("");
        for (row_idx, line) in source_str.lines().enumerate() {
            if row_idx > start_row && row_idx < end_row && line.trim().is_empty() {
                return true;
            }
            if row_idx >= end_row {
                break;
            }
        }
        false
    }

    /// Update last-token tracking fields.
    fn set_last_token(&mut self, kind: &str, parent_kind: &str) {
        self.last_token_kind = kind.to_string();
        self.last_token_parent_kind = parent_kind.to_string();
    }

    // ── Measurement (read-only lookahead) ────────────────────────

    /// Measure the single-line width a subtree would produce.
    /// Returns `(width, last_kind, last_parent_kind)` so callers can chain measurements.
    #[allow(dead_code)]
    pub(crate) fn measure_node(
        &self,
        node: Node,
        prev_kind: &str,
        prev_parent_kind: &str,
    ) -> (usize, String, String) {
        // If in format-off region, use approximate width from raw text.
        if self.is_in_format_off_region(node) {
            let text = self.node_text(node);
            let approx = text.lines().map(|l| l.len()).max().unwrap_or(0);
            return (approx, prev_kind.to_string(), prev_parent_kind.to_string());
        }

        let kind = node.kind();

        // Verbatim leaf nodes (literalChar / literalString) — treat as single token.
        if kind == "literalChar" || kind == "literalString" {
            let text = self.node_text(node);
            let parent_kind = node
                .parent()
                .map(|p| p.kind().to_string())
                .unwrap_or_default();
            let space = if self.would_need_space(prev_kind, prev_parent_kind, kind, &parent_kind) {
                1
            } else {
                0
            };
            return (space + text.len(), kind.to_string(), parent_kind);
        }

        // Plain leaf node.
        if node.child_count() == 0 && !node.is_extra() {
            return self.measure_leaf(node, prev_kind, prev_parent_kind);
        }

        // Internal node — recurse into non-extra children.
        let mut total = 0usize;
        let mut cur_kind = prev_kind.to_string();
        let mut cur_parent = prev_parent_kind.to_string();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            let (w, k, p) = self.measure_node(child, &cur_kind, &cur_parent);
            total += w;
            cur_kind = k;
            cur_parent = p;
        }
        (total, cur_kind, cur_parent)
    }

    /// Measure a single leaf token.
    #[allow(dead_code)]
    pub(crate) fn measure_leaf(
        &self,
        node: Node,
        prev_kind: &str,
        prev_parent_kind: &str,
    ) -> (usize, String, String) {
        let kind = node.kind();
        let text = self.node_text(node);
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();
        let space = if self.would_need_space(prev_kind, prev_parent_kind, kind, &parent_kind) {
            1
        } else {
            0
        };
        (space + text.len(), kind.to_string(), parent_kind)
    }

    /// Pure spacing check — mirrors `needs_space_before` but takes explicit
    /// parameters instead of reading `self.last_token_kind`.
    #[allow(dead_code)]
    pub(crate) fn would_need_space(
        &self,
        prev_kind: &str,
        prev_parent_kind: &str,
        kind: &str,
        parent_kind: &str,
    ) -> bool {
        // 1. No previous token → no space.
        if prev_kind.is_empty() {
            return false;
        }
        // 2. No space before `)`, `]`, `.`
        if kind == ")" || kind == "]" || kind == "." {
            return false;
        }
        // 3. No space after `(`, `[`, `.`
        if prev_kind == "(" || prev_kind == "[" || prev_kind == "." {
            return false;
        }
        // 4. No space before `;`
        if kind == ";" {
            return false;
        }
        // 5. No space before `,`
        if kind == "," {
            return false;
        }
        // 6. No space before `[` in subscript context
        if kind == "[" && parent_kind == "exprSubscript" {
            return false;
        }
        // 7. No space before `(` in call/args context
        if kind == "(" && (parent_kind == "exprCall" || parent_kind == "declArgs") {
            return false;
        }
        // 8. No spaces inside generic angle brackets
        if (kind == "kLt" || kind == "<")
            && (parent_kind == "typerefTpl"
                || parent_kind == "genericTpl"
                || parent_kind == "genericArgs"
                || parent_kind == "typerefArgs"
                || parent_kind == "exprTpl")
        {
            return false;
        }
        if (kind == "kGt" || kind == ">")
            && (parent_kind == "typerefTpl"
                || parent_kind == "genericTpl"
                || parent_kind == "genericArgs"
                || parent_kind == "typerefArgs"
                || parent_kind == "exprTpl")
        {
            return false;
        }
        // 9. No space after `<` in generic context
        if prev_kind == "kLt" && is_generic_parent(prev_parent_kind) {
            return false;
        }
        // 10. No space before `:`
        if kind == ":" {
            return false;
        }
        // 11. No space around `..`
        if kind == ".." || prev_kind == ".." {
            return false;
        }
        // 12. No space before/after `kDot` or `.`
        if kind == "kDot" {
            return false;
        }
        if prev_kind == "kDot" || prev_kind == "." {
            return false;
        }
        // 13. spacing::space_before
        if spacing::space_before(kind) {
            return true;
        }
        // 14. spacing::space_after
        if spacing::space_after(prev_kind) {
            return true;
        }
        // 15. keyword needing space after
        if is_keyword_needing_space_after(prev_kind) {
            return true;
        }
        // 16. Default: space between two identifiers/keywords/literals
        if !prev_kind.is_empty() && prev_kind != ";" && prev_kind != "(" && prev_kind != "[" {
            return true;
        }
        // 17. Otherwise
        false
    }

    /// Measure the combined single-line width of a slice of nodes.
    /// Starts from `self.last_token_kind` / `self.last_token_parent_kind`.
    #[allow(dead_code)]
    pub(crate) fn measure_group(&self, nodes: &[Node]) -> (usize, String, String) {
        let mut total = 0usize;
        let mut cur_kind = self.last_token_kind.clone();
        let mut cur_parent = self.last_token_parent_kind.clone();
        for node in nodes {
            let (w, k, p) = self.measure_node(*node, &cur_kind, &cur_parent);
            total += w;
            cur_kind = k;
            cur_parent = p;
        }
        (total, cur_kind, cur_parent)
    }

    /// Check if any ancestor of `node` has the given kind.
    fn is_ancestor(node: Node, ancestor_kind: &str) -> bool {
        let mut current = node.parent();
        while let Some(p) = current {
            if p.kind() == ancestor_kind {
                return true;
            }
            current = p.parent();
        }
        false
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
        let last_line = text.lines().last();
        if self.at_line_start {
            self.current_column = 0;
        } else if let Some(ll) = last_line {
            self.current_column = ll.len();
        }
        if let Some(ll) = last_line {
            // crude: pick up the last "word" as last_token_kind
            if let Some(word) = ll.split_whitespace().last() {
                self.last_token_kind = word.to_string();
            }
        }
    }
}

/// Block-opening keywords after which blank lines should be stripped.
fn is_block_opener(kind: &str) -> bool {
    matches!(kind, "kBegin" | "kTry" | "kRepeat" | "kAsm")
}

/// Block-closing keywords before which blank lines should be stripped.
fn is_block_closer(kind: &str) -> bool {
    matches!(kind, "kEnd" | "kExcept" | "kFinally")
}

/// Parent kinds that indicate a generic type context (not comparison).
fn is_generic_parent(parent_kind: &str) -> bool {
    matches!(
        parent_kind,
        "typerefTpl" | "genericTpl" | "genericArgs" | "typerefArgs" | "exprTpl"
    )
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
