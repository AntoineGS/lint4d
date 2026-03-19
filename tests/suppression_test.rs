use lint4d::engine::suppress::parse_suppressions;

#[test]
fn parses_ignore_directive_on_own_line() {
    let source = "// lint4d:ignore empty-except\ntry\nexcept\nend;\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].target_line, 2);
    assert_eq!(suppressions[0].rule_id, Some("empty-except".to_string()));
}

#[test]
fn parses_ignore_next_line() {
    let source = "// lint4d:ignore-next-line bare-except\ntry\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].target_line, 2);
    assert_eq!(suppressions[0].rule_id, Some("bare-except".to_string()));
}

#[test]
fn parses_ignore_all_rules() {
    let source = "// lint4d:ignore\nobj := TObject.Create;\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].target_line, 2);
    assert_eq!(suppressions[0].rule_id, None);
}

#[test]
fn parses_inline_ignore_same_line() {
    let source = "obj := TObject.Create; // lint4d:ignore resource-leak-no-try\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].target_line, 1);
    assert_eq!(suppressions[0].rule_id, Some("resource-leak-no-try".to_string()));
}

#[test]
fn strips_reason_after_dash() {
    let source = "// lint4d:ignore resource-leak-no-try -- owned by form\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].rule_id, Some("resource-leak-no-try".to_string()));
}

#[test]
fn no_suppressions_in_plain_code() {
    let source = "obj := TObject.Create;\nobj.Free;\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert!(suppressions.is_empty());
}

#[test]
fn suppression_matches_diagnostic() {
    let source = "// lint4d:ignore empty-except\ntry except end;\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert!(suppressions[0].matches("empty-except", 2));
    assert!(!suppressions[0].matches("empty-except", 3));
    assert!(!suppressions[0].matches("bare-except", 2));
}

#[test]
fn wildcard_suppression_matches_any_rule() {
    let source = "// lint4d:ignore\nsome code;\n";
    let suppressions = parse_suppressions(source.as_bytes());
    assert!(suppressions[0].matches("any-rule", 2));
    assert!(suppressions[0].matches("other-rule", 2));
}
