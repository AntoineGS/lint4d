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
    use streaming_iterator::StreamingIterator;

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
            ("unit U;\r\n// hello\r\ninterface\r\nimplementation\r\nend.\r\n", 17),
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
