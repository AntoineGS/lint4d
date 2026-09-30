use pascal_core::conditional::{self, CompilerVersion, ConditionalContext, ConstantValue, Truth};

fn activity(source: &str, needle: &str, context: &ConditionalContext) -> Truth {
    conditional::analyze_with_context(source, context)
        .directives
        .iter()
        .find(|directive| directive.body.contains(needle))
        .map(|directive| directive.activity)
        .unwrap_or_else(|| panic!("directive {needle:?} not found"))
}

fn closed() -> ConditionalContext {
    let mut context = ConditionalContext::default().with_absent_define(Truth::False);
    context.set_define("RELEASE", Truth::True);
    context
}

#[test]
fn absent_define_defaults_to_unknown() {
    let source = "{$IFDEF DEBUG}{$DEFINE HIT}{$ENDIF}";
    assert_eq!(
        activity(source, "DEFINE HIT", &ConditionalContext::default()),
        Truth::Unknown
    );
}

#[test]
fn closed_world_makes_unlisted_defines_false() {
    let context = closed();
    assert_eq!(
        activity("{$IFDEF DEBUG}{$DEFINE A}{$ENDIF}", "DEFINE A", &context),
        Truth::False
    );
    assert_eq!(
        activity("{$IFNDEF DEBUG}{$DEFINE B}{$ENDIF}", "DEFINE B", &context),
        Truth::True
    );
    assert_eq!(
        activity(
            "{$IF defined(DEBUG)}{$DEFINE C}{$IFEND}",
            "DEFINE C",
            &context
        ),
        Truth::False
    );
    assert_eq!(
        activity("{$IFDEF RELEASE}{$DEFINE D}{$ENDIF}", "DEFINE D", &context),
        Truth::True
    );
    let analysis = conditional::analyze_with_context(
        "unit U; interface {$IFDEF DEBUG} procedure X; {$ENDIF} implementation end.",
        &context,
    );
    assert!(analysis.unknown_spans.is_empty());
    assert!(!analysis.unknown_activity_requires_fail_closed());
}

#[test]
fn source_define_and_undef_override_the_absent_default() {
    let context = closed();
    let source = "{$DEFINE LOCAL}{$IFDEF LOCAL}{$DEFINE A}{$ENDIF}{$UNDEF RELEASE}{$IFDEF RELEASE}{$DEFINE B}{$ENDIF}";
    assert_eq!(activity(source, "DEFINE A", &context), Truth::True);
    assert_eq!(activity(source, "DEFINE B", &context), Truth::False);
}

#[test]
fn branch_merge_uses_the_absent_default_for_one_sided_keys() {
    // After an IFDEF on a known-false symbol, X was never defined on any path.
    let context = closed();
    let source = "{$IFDEF DEBUG}{$DEFINE X}{$ENDIF}{$IFDEF X}{$DEFINE HIT}{$ENDIF}";
    assert_eq!(activity(source, "DEFINE HIT", &context), Truth::False);
}

#[test]
fn unknown_include_resets_the_absent_default() {
    let context = closed();
    let source =
        "{$IFDEF MAYBE_INCLUDED}{$ENDIF}{$I unknown.inc}{$IFDEF FROM_INCLUDE}{$DEFINE HIT}{$ENDIF}";
    // Without an include callback, an active include clears the environment.
    assert_eq!(activity(source, "DEFINE HIT", &context), Truth::Unknown);
}

#[test]
fn rtl_constants_are_declared_or_absent_only_when_known() {
    let mut known = ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(35, 0))
        .with_constant("RTLVersion111", ConstantValue::Boolean(true));
    known.rtl_constants_known = true;
    let source = "{$IF Declared(RTLVersion111)}{$DEFINE A}{$IFEND}{$IF Declared(RTLVersion113)}{$DEFINE B}{$IFEND}{$IF RTLVersion113}{$DEFINE C}{$IFEND}";
    assert_eq!(activity(source, "DEFINE A", &known), Truth::True);
    assert_eq!(activity(source, "DEFINE B", &known), Truth::False);
    assert_eq!(activity(source, "DEFINE C", &known), Truth::False);

    let unknown = ConditionalContext::default();
    assert_eq!(activity(source, "DEFINE B", &unknown), Truth::Unknown);
    assert_eq!(activity(source, "DEFINE C", &unknown), Truth::Unknown);
}

#[test]
fn rtl_rule_does_not_apply_to_other_identifiers() {
    let mut context = closed();
    context.rtl_constants_known = true;
    let source = "{$IF Declared(TFoo)}{$DEFINE A}{$IFEND}";
    assert_eq!(activity(source, "DEFINE A", &context), Truth::Unknown);
}

#[test]
fn fingerprint_distinguishes_the_new_fields() {
    let open = ConditionalContext::default();
    let closed = ConditionalContext::default().with_absent_define(Truth::False);
    let mut rtl = ConditionalContext::default();
    rtl.rtl_constants_known = true;
    assert_ne!(open.fingerprint(), closed.fingerprint());
    assert_ne!(open.fingerprint(), rtl.fingerprint());
}
