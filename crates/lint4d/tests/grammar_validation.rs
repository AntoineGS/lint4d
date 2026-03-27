use std::fs;
use std::path::Path;
use tree_sitter::Parser;
use tree_sitter_language::LanguageFn;
use tree_sitter_pascal::LANGUAGE;

fn parse_and_check_no_errors(fixture_path: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", fixture_path, e));

    let mut parser = Parser::new();
    let language = LanguageFn::from(LANGUAGE);
    parser
        .set_language(&language.into())
        .expect("Failed to set language");

    let tree = parser.parse(&source, None).expect("Failed to parse");
    let root = tree.root_node();

    assert!(
        !root.has_error(),
        "Parse errors in {}: {}",
        fixture_path,
        root.to_sexp()
    );
}

#[test]
fn grammar_parses_class_declaration() {
    parse_and_check_no_errors("tests/fixtures/grammar/class_basic.pas");
}

#[test]
fn grammar_parses_try_finally_and_except() {
    parse_and_check_no_errors("tests/fixtures/grammar/try_finally.pas");
}

#[test]
fn grammar_parses_generics() {
    parse_and_check_no_errors("tests/fixtures/grammar/generics.pas");
}

#[test]
fn grammar_parses_anonymous_methods() {
    parse_and_check_no_errors("tests/fixtures/grammar/anonymous_method.pas");
}

#[test]
fn grammar_parses_record_helpers() {
    parse_and_check_no_errors("tests/fixtures/grammar/record_helper.pas");
}

#[test]
fn grammar_parses_with_statement() {
    parse_and_check_no_errors("tests/fixtures/grammar/with_statement.pas");
}

#[test]
fn grammar_parses_const_declarations() {
    parse_and_check_no_errors("tests/fixtures/grammar/const_declaration.pas");
}

#[test]
fn grammar_parses_interface_declarations() {
    parse_and_check_no_errors("tests/fixtures/grammar/interface_declaration.pas");
}
