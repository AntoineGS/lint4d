//! This crate provides Pascal language support for the [tree-sitter][] parsing library.
//!
//! Typically, you will use the [LANGUAGE][] constant to add this language to a
//! tree-sitter [Parser][], and then use the parser to parse some code:
//!
//! ```
//! let code = r#"
//! "#;
//! let mut parser = tree_sitter::Parser::new();
//! let language = tree_sitter_pascal::LANGUAGE;
//! parser
//!     .set_language(&language.into())
//!     .expect("Error loading Pascal parser");
//! let tree = parser.parse(code, None).unwrap();
//! assert!(!tree.root_node().has_error());
//! ```
//!
//! [Parser]: https://docs.rs/tree-sitter/*/tree_sitter/struct.Parser.html
//! [tree-sitter]: https://tree-sitter.github.io/

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_pascal() -> *const ();
}

/// The tree-sitter [`LanguageFn`][LanguageFn] for this grammar.
///
/// [LanguageFn]: https://docs.rs/tree-sitter-language/*/tree_sitter_language/struct.LanguageFn.html
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_pascal) };

/// The content of the [`node-types.json`][] file for this grammar.
///
/// [`node-types.json`]: https://tree-sitter.github.io/tree-sitter/using-parsers#static-node-types
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

// NOTE: uncomment these to include any queries that this grammar contains:

// pub const HIGHLIGHTS_QUERY: &str = include_str!("../../queries/highlights.scm");
// pub const INJECTIONS_QUERY: &str = include_str!("../../queries/injections.scm");
// pub const LOCALS_QUERY: &str = include_str!("../../queries/locals.scm");
// pub const TAGS_QUERY: &str = include_str!("../../queries/tags.scm");

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use streaming_iterator::StreamingIterator;
    use tree_sitter::Node;

    const DCC_RESERVED_WORDS: [&str; 64] = [
        "AND",
        "ARRAY",
        "AS",
        "ASM",
        "BEGIN",
        "CASE",
        "CLASS",
        "CONST",
        "CONSTRUCTOR",
        "DESTRUCTOR",
        "DISPINTERFACE",
        "DIV",
        "DO",
        "DOWNTO",
        "ELSE",
        "END",
        "EXCEPT",
        "EXPORTS",
        "FILE",
        "FINALIZATION",
        "FINALLY",
        "FOR",
        "FUNCTION",
        "GOTO",
        "IF",
        "IMPLEMENTATION",
        "IN",
        "INHERITED",
        "INITIALIZATION",
        "INLINE",
        "INTERFACE",
        "IS",
        "LABEL",
        "LIBRARY",
        "MOD",
        "NIL",
        "NOT",
        "OBJECT",
        "OF",
        "OR",
        "PACKED",
        "PROCEDURE",
        "PROGRAM",
        "PROPERTY",
        "RAISE",
        "RECORD",
        "REPEAT",
        "RESOURCESTRING",
        "SET",
        "SHL",
        "SHR",
        "STRING",
        "THEN",
        "THREADVAR",
        "TO",
        "TRY",
        "TYPE",
        "UNIT",
        "UNTIL",
        "USES",
        "VAR",
        "WHILE",
        "WITH",
        "XOR",
    ];

    const CONTEXTUAL_IDENTIFIER_SAMPLES: [&str; 14] = [
        "at",
        "on",
        "out",
        "strict",
        "helper",
        "reference",
        "operator",
        "read",
        "write",
        "default",
        "message",
        "name",
        "index",
        "alias",
    ];

    fn new_parser() -> tree_sitter::Parser {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&super::LANGUAGE.into()).unwrap();
        parser
    }

    fn target_span_after_prefix(source: &str, prefix: &str, target: &str) -> Range<usize> {
        assert!(
            source.starts_with(prefix),
            "source {:?} does not start with target prefix {:?}",
            source,
            prefix
        );
        let start = prefix.len();
        assert!(
            source[start..].starts_with(target),
            "target {:?} does not follow prefix {:?} in {:?}",
            target,
            prefix,
            source
        );
        start..start + target.len()
    }

    #[test]
    fn test_target_span_selects_the_declared_name_when_it_matches_its_prefix() {
        let source = "var var: Integer;";
        assert_eq!(target_span_after_prefix(source, "var ", "var"), 4..7);
    }

    fn exact_node<'tree>(node: Node<'tree>, kind: &str, span: Range<usize>) -> Option<Node<'tree>> {
        if node.kind() == kind && node.byte_range() == span {
            return Some(node);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = exact_node(child, kind, span.clone()) {
                return Some(found);
            }
        }
        None
    }

    fn node_covering<'tree>(
        node: Node<'tree>,
        kind: &str,
        span: Range<usize>,
    ) -> Option<Node<'tree>> {
        let mut best = if node.kind() == kind
            && node.start_byte() <= span.start
            && node.end_byte() >= span.end
        {
            Some(node)
        } else {
            None
        };

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = node_covering(child, kind, span.clone()) {
                if best
                    .map(|current| found.byte_range().len() < current.byte_range().len())
                    .unwrap_or(true)
                {
                    best = Some(found);
                }
            }
        }
        best
    }

    fn contains_kind(node: Node<'_>, kind: &str) -> bool {
        if node.kind() == kind {
            return true;
        }
        let mut cursor = node.walk();
        let found = node
            .children(&mut cursor)
            .any(|child| contains_kind(child, kind));
        found
    }

    fn has_identifier_at(node: Node<'_>, span: Range<usize>) -> bool {
        exact_node(node, "identifier", span).is_some()
    }

    fn case_spellings(word: &str) -> [String; 3] {
        let mixed = word
            .chars()
            .enumerate()
            .map(|(index, ch)| {
                if index % 2 == 0 {
                    ch.to_ascii_uppercase()
                } else {
                    ch.to_ascii_lowercase()
                }
            })
            .collect();
        [word.to_ascii_uppercase(), word.to_ascii_lowercase(), mixed]
    }

    fn assert_clean_identifier(
        parser: &mut tree_sitter::Parser,
        source: &str,
        prefix: &str,
        target: &str,
    ) {
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "unexpected parse error for {:?}: {}",
            source,
            tree.root_node().to_sexp()
        );
        assert!(
            has_identifier_at(
                tree.root_node(),
                target_span_after_prefix(source, prefix, target),
            ),
            "target {:?} was not a full-span identifier in {:?}: {}",
            target,
            source,
            tree.root_node().to_sexp()
        );
    }

    #[test]
    fn test_dcc_reserved_words_are_not_identifier_leaves_in_forced_declarations() {
        assert_eq!(DCC_RESERVED_WORDS.len(), 64);
        let mut parser = new_parser();
        let mut accepted = Vec::new();

        for word in DCC_RESERVED_WORDS {
            for spelling in case_spellings(word) {
                let source = format!("var {}: Integer;", spelling);
                let tree = parser.parse(&source, None).unwrap();
                if has_identifier_at(
                    tree.root_node(),
                    target_span_after_prefix(&source, "var ", &spelling),
                ) {
                    accepted.push(format!("{}: {}", word, spelling));
                }
            }
        }

        assert!(
            accepted.is_empty(),
            "{} of 192 DCC-reserved declaration spellings became identifier leaves; first cases: {}",
            accepted.len(),
            accepted.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
        );
    }

    #[test]
    fn test_keyword_prefixes_are_not_split_at_keyword_syntax_positions() {
        let long_begin = format!("begin{}", "n".repeat(64));
        let long_end = format!("end{}", "less".repeat(16));
        let long_function = format!("function{}", "ing".repeat(16));

        let valid_keyword_controls = [
            (
                "program P; begin end.",
                &["kProgram", "kBegin", "kEnd"][..],
            ),
            (
                "unit U; interface implementation end.",
                &["kUnit", "kInterface", "kImplementation", "kEnd"][..],
            ),
            (
                "unit U; interface function F: Integer; implementation function F: Integer; begin F := 1; end; end.",
                &["kFunction"][..],
            ),
            (
                "program P; begin try Value := 1; finally Value := 2; end; end.",
                &["kTry", "kFinally"][..],
            ),
        ];
        let mut parser = new_parser();
        for (source, required_keywords) in valid_keyword_controls {
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "valid keyword control failed for {:?}: {}",
                source,
                tree.root_node().to_sexp()
            );
            for keyword in required_keywords {
                assert!(
                    contains_kind(tree.root_node(), keyword),
                    "valid keyword control omitted {} in {:?}: {}",
                    keyword,
                    source,
                    tree.root_node().to_sexp()
                );
            }
        }

        let long_identifier_source = "unit VeryLongPrefixNames; interface implementation end.";
        let long_identifier_tree = parser.parse(long_identifier_source, None).unwrap();
        assert!(
            !long_identifier_tree.root_node().has_error(),
            "long identifier after a valid unit keyword failed: {}",
            long_identifier_tree.root_node().to_sexp()
        );
        assert!(has_identifier_at(
            long_identifier_tree.root_node(),
            target_span_after_prefix(long_identifier_source, "unit ", "VeryLongPrefixNames")
        ));

        let mut prefixed_names = [
            "beginn",
            "endless",
            "unitly",
            "programmer",
            "functioning",
            "tryout",
            "verylongprefixnames",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect::<Vec<_>>();
        prefixed_names.extend([long_begin.clone(), long_end.clone(), long_function.clone()]);
        for spelling in prefixed_names {
            let source = format!("var {}: Integer;", spelling);
            assert_clean_identifier(&mut parser, &source, "var ", &spelling);
        }

        for spelling in ["begin", "end", "unit", "program", "function", "try"] {
            let source = format!("var {}: Integer;", spelling);
            let tree = parser.parse(&source, None).unwrap();
            let name_span = target_span_after_prefix(&source, "var ", spelling);
            assert!(
                tree.root_node().has_error() && !has_identifier_at(tree.root_node(), name_span),
                "reserved keyword {:?} was accepted in a forced identifier position: {}",
                spelling,
                tree.root_node().to_sexp()
            );
        }

        let procedure_prefix = "unit T; interface implementation procedure P; ";
        let end_prefix = "program P; begin ";
        let interface_prefix = "unit U; interface ";
        let mut keyword_prefix_cases = vec![
            (
                "begin at a routine block start",
                "unit T; interface implementation procedure P; beginn end; end.".to_owned(),
                procedure_prefix,
                "beginn".to_owned(),
                "begin",
                "kBegin",
            ),
            (
                "end at a program block close",
                "program P; begin endless.".to_owned(),
                end_prefix,
                "endless".to_owned(),
                "end",
                "kEnd",
            ),
            (
                "unit at the compilation-unit start",
                "unitly; interface implementation end.".to_owned(),
                "",
                "unitly".to_owned(),
                "unit",
                "kUnit",
            ),
            (
                "program at the program start",
                "programmer; begin end.".to_owned(),
                "",
                "programmer".to_owned(),
                "program",
                "kProgram",
            ),
            (
                "function at an interface declaration start",
                "unit U; interface functioning: Integer; implementation end.".to_owned(),
                interface_prefix,
                "functioning".to_owned(),
                "function",
                "kFunction",
            ),
            (
                "try at a statement start",
                "program P; begin tryout; except end; end.".to_owned(),
                end_prefix,
                "tryout".to_owned(),
                "try",
                "kTry",
            ),
        ];
        keyword_prefix_cases.extend([
            (
                "long begin prefix at a routine block start",
                format!("{}{} end; end.", procedure_prefix, long_begin),
                procedure_prefix,
                long_begin,
                "begin",
                "kBegin",
            ),
            (
                "long end prefix at a program block close",
                format!("{}{}.", end_prefix, long_end),
                end_prefix,
                long_end,
                "end",
                "kEnd",
            ),
            (
                "long function prefix at an interface declaration start",
                format!(
                    "{}{}: Integer; implementation end.",
                    interface_prefix, long_function
                ),
                interface_prefix,
                long_function,
                "function",
                "kFunction",
            ),
        ]);

        let mut split_keyword_prefixes = Vec::new();
        for (label, source, prefix, spelling, keyword_spelling, keyword_kind) in
            keyword_prefix_cases
        {
            let keyword_span =
                target_span_after_prefix(&source, prefix, &spelling[..keyword_spelling.len()]);
            let tree = parser.parse(&source, None).unwrap();
            if !tree.root_node().has_error()
                || exact_node(tree.root_node(), keyword_kind, keyword_span).is_some()
            {
                split_keyword_prefixes.push(format!(
                    "{} {:?}: {}",
                    label,
                    spelling,
                    tree.root_node().to_sexp()
                ));
            }
        }

        assert!(
            split_keyword_prefixes.is_empty(),
            "{} keyword-prefix cases were accepted or split at a keyword token: {}",
            split_keyword_prefixes.len(),
            split_keyword_prefixes.join("; ")
        );
    }

    #[test]
    fn test_escaped_reserved_words_and_identifier_boundaries_remain_full_identifiers() {
        let mut parser = new_parser();
        let mut escaped_count = 0;
        for word in DCC_RESERVED_WORDS {
            let spelling = format!("&{}", case_spellings(word)[2]);
            let source = format!("var {}: Integer;", spelling);
            assert_clean_identifier(&mut parser, &source, "var ", &spelling);
            escaped_count += 1;
        }
        assert_eq!(escaped_count, 64);

        for spelling in [
            "A".to_owned(),
            "AN".to_owned(),
            "Andromeda".to_owned(),
            "inlineValue".to_owned(),
            "And0".to_owned(),
            "And_".to_owned(),
            "And$Field".to_owned(),
            "And$".to_owned(),
            "A$".to_owned(),
            "AN$".to_owned(),
            "_And".to_owned(),
            format!("And{}", "0123456789".repeat(8)),
        ] {
            let source = format!("var {}: Integer;", spelling);
            assert_clean_identifier(&mut parser, &source, "var ", &spelling);
        }
    }

    #[test]
    fn test_non_reserved_contextual_words_remain_identifiers_in_declarations_and_expressions() {
        let mut parser = new_parser();
        let mut checked = 0;
        let mut failures = Vec::new();

        for word in CONTEXTUAL_IDENTIFIER_SAMPLES {
            for spelling in case_spellings(word) {
                let declaration = format!("var {}: Integer;", spelling);
                let declaration_tree = parser.parse(&declaration, None).unwrap();
                if declaration_tree.root_node().has_error()
                    || !has_identifier_at(
                        declaration_tree.root_node(),
                        target_span_after_prefix(&declaration, "var ", &spelling),
                    )
                {
                    failures.push(format!("declaration {}: {}", word, spelling));
                }
                let expression = format!("begin Value := {}; end", spelling);
                let expression_tree = parser.parse(&expression, None).unwrap();
                if expression_tree.root_node().has_error()
                    || !has_identifier_at(
                        expression_tree.root_node(),
                        target_span_after_prefix(&expression, "begin Value := ", &spelling),
                    )
                {
                    failures.push(format!("expression {}: {}", word, spelling));
                }
                checked += 2;
            }
        }

        assert_eq!(checked, 84);
        assert!(
            failures.is_empty(),
            "{} of 84 contextual declaration/expression cases failed; first cases: {}",
            failures.len(),
            failures
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    #[test]
    fn test_contextual_alias_remains_a_variable_name_after_a_previous_declaration() {
        let source = "var X: Integer; alias: String; Y: Boolean;";
        let mut parser = new_parser();
        assert_clean_identifier(&mut parser, source, "var X: Integer; ", "alias");
    }

    #[test]
    fn test_keyword_expressions_and_fpc_operator_and_inline_branches_are_preserved() {
        let mut parser = new_parser();
        for (source, keyword) in [
            ("begin Value := nil; end", "kNil"),
            ("begin Value := not Ready; end", "kNot"),
            ("begin Value := inherited; end", "kInherited"),
            ("begin Value := Left and Right; end", "kAnd"),
            ("begin Value := true; Value := false; end", "kTrue"),
            ("begin Value := true; Value := false; end", "kFalse"),
        ] {
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "keyword expression failed for {:?}: {}",
                source,
                tree.root_node().to_sexp()
            );
            assert!(
                contains_kind(tree.root_node(), keyword),
                "missing {} branch in {:?}: {}",
                keyword,
                source,
                tree.root_node().to_sexp()
            );
        }

        let operator = "type T = record operator +(A, B: T): T; end;";
        let tree = parser.parse(operator, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "FPC operator declaration failed: {}",
            tree.root_node().to_sexp()
        );
        assert!(contains_kind(tree.root_node(), "kOperator"));

        let inline = "procedure Fast; inline; forward;";
        let tree = parser.parse(inline, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "inline routine attribute failed: {}",
            tree.root_node().to_sexp()
        );
        assert!(contains_kind(tree.root_node(), "kInline"));
    }

    #[test]
    fn test_property_exception_and_out_contextual_tokens_are_preserved() {
        let mut parser = new_parser();
        for (source, keywords) in [
            (
                "type T = class property Value: Integer read GetValue write SetValue; end;",
                ["kRead", "kWrite"].as_slice(),
            ),
            (
                "procedure Handle(out Value: Integer); begin try Item; except on E: Exception do Value := 1; end; end;",
                ["kOut", "kOn"].as_slice(),
            ),
        ] {
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "contextual grammar branch failed for {:?}: {}",
                source,
                tree.root_node().to_sexp()
            );
            for keyword in keywords {
                assert!(
                    contains_kind(tree.root_node(), keyword),
                    "missing {} branch in {:?}: {}",
                    keyword,
                    source,
                    tree.root_node().to_sexp()
                );
            }
        }
    }

    #[test]
    fn test_string_calls_keep_identifier_callee_cst_without_reopening_reserved_names() {
        let source = "program CastCompat; var X, Value: string; begin X := String(Value); X := string(Value); X := PChar(String(Value)); end.";
        let mut parser = new_parser();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "String call/cast failed: {}",
            tree.root_node().to_sexp()
        );

        let mut calls = Vec::new();
        fn collect_calls<'tree>(node: Node<'tree>, calls: &mut Vec<Node<'tree>>) {
            if node.kind() == "exprCall" {
                calls.push(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_calls(child, calls);
            }
        }
        collect_calls(tree.root_node(), &mut calls);
        assert_eq!(
            calls.len(),
            4,
            "unexpected cast CST: {}",
            tree.root_node().to_sexp()
        );
        let mut entity_spellings = Vec::new();
        for call in calls {
            let entity = call.child_by_field_name("entity").unwrap();
            assert_eq!(entity.kind(), "identifier");
            assert_eq!(entity.child_count(), 0);
            entity_spellings.push(source[entity.byte_range()].to_owned());
            assert_eq!(call.child_by_field_name("args").unwrap().kind(), "exprArgs");
        }
        entity_spellings.sort();
        assert_eq!(entity_spellings, ["PChar", "String", "String", "string"]);

        let mut rejected_name_failures = Vec::new();
        for (source, prefix) in [
            ("var String: Integer;", "var "),
            ("begin X := String; end", "begin X := "),
        ] {
            let tree = parser.parse(source, None).unwrap();
            let span = target_span_after_prefix(source, prefix, "String");
            if !tree.root_node().has_error() || has_identifier_at(tree.root_node(), span) {
                rejected_name_failures.push(source);
            }
        }
        assert!(
            rejected_name_failures.is_empty(),
            "{} of 2 reserved String non-call name controls remained identifiers: {}",
            rejected_name_failures.len(),
            rejected_name_failures.join("; ")
        );
    }

    #[test]
    fn test_array_of_const_and_inline_string_keep_existing_named_type_shapes() {
        let mut parser = new_parser();
        let array_source = "procedure Consume(Args: array of const); begin end;";
        let tree = parser.parse(array_source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "array of const failed: {}",
            tree.root_node().to_sexp()
        );
        let const_span =
            target_span_after_prefix(array_source, "procedure Consume(Args: array of ", "const");
        assert!(has_identifier_at(tree.root_node(), const_span.clone()));
        let typeref = node_covering(tree.root_node(), "typeref", const_span)
            .expect("array of const must retain its named typeref node");
        assert_eq!(typeref.named_child_count(), 1);
        assert_eq!(typeref.named_child(0).unwrap().kind(), "identifier");

        let inline_source = "begin var InlineValue: string; InlineValue := InlineValue; end";
        let tree = parser.parse(inline_source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "inline string variable failed: {}",
            tree.root_node().to_sexp()
        );
        let string_span =
            target_span_after_prefix(inline_source, "begin var InlineValue: ", "string");
        assert!(has_identifier_at(tree.root_node(), string_span.clone()));
        let var_def = node_covering(tree.root_node(), "varDef", string_span)
            .expect("inline variable must retain its varDef node");
        let ty = var_def
            .child_by_field_name("type")
            .expect("inline variable must retain the type field");
        assert_eq!(ty.kind(), "typeref");
        assert_eq!(ty.named_child_count(), 1);
        assert_eq!(ty.named_child(0).unwrap().kind(), "identifier");
    }

    #[test]
    fn test_string_remains_an_identifier_leaf_in_typeref_positions() {
        let source = "unit T; interface uses Generics.Collections; type TFoo = class private FDict: TObjectDictionary<string, TStringList>; function GetText: string; procedure SetText(Value: string); end; implementation end.";
        let mut parser = new_parser();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "String typeref positions failed: {}",
            tree.root_node().to_sexp()
        );

        let spans = source
            .match_indices("string")
            .map(|(start, spelling)| start..start + spelling.len())
            .collect::<Vec<_>>();
        assert_eq!(spans.len(), 3);
        for span in spans.iter().take(2) {
            assert!(
                exact_node(tree.root_node(), "identifier", span.clone()).is_some(),
                "String typeref at {:?} must retain its identifier leaf: {}",
                span,
                tree.root_node().to_sexp()
            );
        }
        assert!(
            exact_node(tree.root_node(), "kString", spans[2].clone()).is_some(),
            "ordinary string type must retain its kString leaf: {}",
            tree.root_node().to_sexp()
        );
    }

    #[test]
    fn test_generic_keyword_constraints_keep_their_original_typeref_shapes() {
        let mut parser = new_parser();
        for constraint in ["class", "record", "interface", "constructor"] {
            let source = format!(
                "unit T; interface type TConstrained<T: {constraint}> = class end; implementation end."
            );
            let tree = parser.parse(&source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "generic {constraint} constraint failed: {}",
                tree.root_node().to_sexp()
            );

            let constraint_start = source.find(&format!(": {constraint}")).unwrap() + 2;
            let constraint_span = constraint_start..constraint_start + constraint.len();
            assert!(
                exact_node(tree.root_node(), "identifier", constraint_span.clone()).is_some(),
                "generic {} constraint must retain its identifier leaf",
                constraint
            );
            let constraint_type = node_covering(tree.root_node(), "typeref", constraint_span)
                .expect("generic keyword constraint must retain its named typeref node");
            assert_eq!(constraint_type.named_child_count(), 1);
            assert_eq!(constraint_type.named_child(0).unwrap().kind(), "identifier");

            let declaration_start = source.rfind("class").unwrap();
            let declaration_span = declaration_start..declaration_start + "class".len();
            assert!(exact_node(tree.root_node(), "kClass", declaration_span).is_some());
        }
    }

    #[test]
    fn test_external_word_order_matches_scanner_enum_and_spellings() {
        let grammar = include_str!("../../grammar.js");
        let scanner = include_str!("../../src/scanner.c");

        let external_start = grammar.find("externals: $ => [").unwrap() + "externals: $ => [".len();
        let external_end = external_start + grammar[external_start..].find(']').unwrap();
        let external_symbols = grammar[external_start..external_end]
            .split(',')
            .map(str::trim)
            .filter_map(|symbol| symbol.strip_prefix("$."))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(external_symbols.len(), 144);

        let enum_start = scanner.find("typedef enum {").unwrap() + "typedef enum {".len();
        let enum_end = enum_start + scanner[enum_start..].find("} TokenType;").unwrap();
        let scanner_symbols = scanner[enum_start..enum_end]
            .lines()
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .map(|symbol| symbol.trim_end_matches(',').to_owned())
            .collect::<Vec<_>>();
        let external_to_scanner = |symbol: &str| match symbol {
            "ppFragmentExpr" => "PP_FRAGMENT_EXPR".to_owned(),
            "ppFragmentStmt" => "PP_FRAGMENT_STMT".to_owned(),
            "_literalStringMultiline" => "LITERAL_STRING_MULTILINE".to_owned(),
            _ => symbol.to_ascii_uppercase(),
        };
        let expected_scanner_symbols = external_symbols
            .iter()
            .map(|symbol| external_to_scanner(symbol))
            .collect::<Vec<_>>();
        assert_eq!(scanner_symbols, expected_scanner_symbols);

        let table_start = scanner
            .find("static const ContextualWord CONTEXTUAL_WORDS[] = {")
            .unwrap();
        let table_body_start =
            table_start + "static const ContextualWord CONTEXTUAL_WORDS[] = {".len();
        let table_end = table_body_start + scanner[table_body_start..].find("};").unwrap();
        let contextual_words = scanner[table_body_start..table_end]
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if !line.starts_with("{\"") {
                    return None;
                }
                let fields = line.split('\"').collect::<Vec<_>>();
                let spelling = fields.get(1)?.to_string();
                let token = line
                    .split(',')
                    .nth(1)?
                    .trim()
                    .trim_end_matches('}')
                    .to_string();
                Some((spelling, token))
            })
            .collect::<Vec<_>>();
        assert_eq!(contextual_words.len(), 76);

        for (index, (spelling, token)) in contextual_words.iter().enumerate() {
            let external = &external_symbols[index + 4];
            assert_eq!(
                token,
                &external_to_scanner(external),
                "token order at {}",
                external
            );
            let grammar_spelling = match external.as_str() {
                "kRaiseAt" => "at".to_owned(),
                symbol if symbol.starts_with('k') => symbol[1..].to_ascii_lowercase(),
                _ => panic!("unexpected contextual terminal {}", external),
            };
            assert_eq!(
                spelling, &grammar_spelling,
                "scanner spelling for {}",
                external
            );
        }

        let table_start = scanner
            .find("static const DccReservedWord DCC_RESERVED_WORDS[] = {")
            .unwrap();
        let table_body_start =
            table_start + "static const DccReservedWord DCC_RESERVED_WORDS[] = {".len();
        let table_end = table_body_start + scanner[table_body_start..].find("};").unwrap();
        let dcc_words = scanner[table_body_start..table_end]
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if !line.starts_with("{\"") {
                    return None;
                }
                let spelling = line.split('\"').nth(1)?.to_string();
                let token = line
                    .split(',')
                    .nth(1)?
                    .trim()
                    .trim_end_matches('}')
                    .to_string();
                Some((spelling, token))
            })
            .collect::<Vec<_>>();
        assert_eq!(dcc_words.len(), DCC_RESERVED_WORDS.len());

        for (index, (spelling, token)) in dcc_words.iter().enumerate() {
            let external = &external_symbols[80 + index];
            assert_eq!(
                spelling,
                &DCC_RESERVED_WORDS[index].to_ascii_lowercase(),
                "DCC spelling order at {}",
                external
            );
            assert_eq!(
                token,
                &external_to_scanner(external),
                "DCC token mapping at {}",
                external
            );
        }
    }

    #[test]
    fn test_can_load_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("Error loading Pascal parser");
    }

    #[test]
    fn test_modern_syntax_queries() {
        let language = super::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let source = b"begin case Code of otherwise Allowed := Item is not TWidget end end";
        let tree = parser.parse(source, None).unwrap();
        assert!(!tree.root_node().has_error());

        let query =
            tree_sitter::Query::new(&language, include_str!("../../queries/highlights.scm"))
                .unwrap();
        for text in ["otherwise", "is", "not"] {
            let mut cursor = tree_sitter::QueryCursor::new();
            let mut captures = cursor.captures(&query, tree.root_node(), source.as_slice());
            let mut found = false;
            while let Some((matched, index)) = captures.next() {
                let capture = matched.captures[*index];
                if query.capture_names()[capture.index as usize] == "keyword"
                    && capture.node.utf8_text(source).unwrap() == text
                {
                    found = true;
                    break;
                }
            }
            assert!(found, "missing keyword capture for {}", text);
        }
    }

    #[test]
    fn test_incomplete_modern_syntax_is_not_accepted() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&super::LANGUAGE.into()).unwrap();
        for source in [
            "const Bits = %;",
            "const Bits = %102;",
            "begin for var Entry in do Visit(Entry) end",
            "begin Value := if Ready then 7 end",
            "begin Value := if Ready then 7 else end",
            "begin Value := Code not in end",
            "begin Value := Item is not end",
        ] {
            let tree = parser.parse(source, None).unwrap();
            assert!(
                tree.root_node().has_error(),
                "accepted invalid input: {}",
                source
            );
        }
    }

    #[test]
    fn test_line_comments_exclude_line_terminators() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&super::LANGUAGE.into()).unwrap();
        for (source, comment_end) in [
            ("unit U;\n// hello\ninterface\nimplementation\nend.\n", 16),
            (
                "unit U;\r\n// hello\r\ninterface\r\nimplementation\r\nend.\r\n",
                17,
            ),
            ("unit U;\r// hello\rinterface\rimplementation\rend.\r", 16),
        ] {
            let tree = parser.parse(source, None).unwrap();
            let mut cursor = tree.walk();
            let unit = tree.root_node().child(0).unwrap();
            let comment = unit
                .children(&mut cursor)
                .find(|node| node.kind() == "comment")
                .unwrap_or_else(|| panic!("{:?}: {}", source, tree.root_node().to_sexp()));
            assert_eq!(comment.end_byte(), comment_end, "{:?}", source);
            assert_eq!(&source[comment.byte_range()], "// hello", "{:?}", source);
        }
    }

    #[test]
    fn test_modern_syntax_with_preprocessor_and_incremental_edits() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&super::LANGUAGE.into()).unwrap();
        for source in [
            "{$IFDEF FAST}procedure Run; begin for var Entry in Entries do \
             if Entry is not TWidget then Visit(Entry) end;{$ENDIF}",
            "begin Value := if Ready then {$IFDEF FAST}%10{$ELSE}%1{$ENDIF} else %0 end",
        ] {
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{}: {}",
                source,
                tree.root_node().to_sexp()
            );
        }

        let mut source = String::from(
            "begin var Flag := Item is not TWidget; Value := if Flag then %10 else %1 end",
        );
        let mut tree = parser.parse(&source, None).unwrap();
        assert!(!tree.root_node().has_error());
        for (old, new) in [
            ("not ", ""),
            ("is TWidget", "is not TWidget"),
            ("%10", "%111"),
        ] {
            let start = source.find(old).unwrap();
            source.replace_range(start..start + old.len(), new);
            tree.edit(&tree_sitter::InputEdit {
                start_byte: start,
                old_end_byte: start + old.len(),
                new_end_byte: start + new.len(),
                start_position: tree_sitter::Point::new(0, start),
                old_end_position: tree_sitter::Point::new(0, start + old.len()),
                new_end_position: tree_sitter::Point::new(0, start + new.len()),
            });
            tree = parser.parse(&source, Some(&tree)).unwrap();
            let fresh = parser.parse(&source, None).unwrap();
            assert!(!tree.root_node().has_error());
            assert_eq!(tree.root_node().to_sexp(), fresh.root_node().to_sexp());
            assert_eq!(tree.root_node().end_byte(), source.len());
        }
    }
}
