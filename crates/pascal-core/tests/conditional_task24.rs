use pascal_core::conditional::{
    self, CompilerVersion, ConditionalContext, ConstantValue, IncludeTransition, Truth,
};
use pascal_core::resolver::NoCancellation;

fn directive_activity(source: &str, needle: &str, context: &ConditionalContext) -> Truth {
    let analysis = conditional::analyze_with_context(source, context);
    analysis
        .directives
        .iter()
        .find(|directive| directive.body.contains(needle))
        .map(|directive| directive.activity)
        .unwrap_or_else(|| panic!("directive {needle:?} not found in {source:?}"))
}

#[test]
fn explicit_compiler_version_is_precise_and_unknown_is_not_inferred() {
    let known = ConditionalContext::default().with_compiler_version(CompilerVersion::new(24, 0));
    let source = "{$IF CompilerVersion >= 24.0}{$DEFINE NEW}{$ELSE}{$DEFINE OLD}{$ENDIF}";
    let analysis = conditional::analyze_with_context(source, &known);
    assert_eq!(
        directive_activity(source, "DEFINE NEW", &known),
        Truth::True
    );
    assert!(
        analysis
            .projected_source
            .contains("                         ")
    );

    let unknown = conditional::analyze_with_context(source, &ConditionalContext::default());
    assert_eq!(
        unknown
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE NEW"))
            .expect("new branch")
            .activity,
        Truth::Unknown
    );
}

#[test]
fn ifopt_switches_are_sequential_and_unknown_options_remain_unknown() {
    let mut context = ConditionalContext::default();
    context.set_option("R", Truth::True);
    let source = concat!(
        "{$IFOPT R+}{$DEFINE FIRST}{$ENDIF}",
        "{$R-}{$IFOPT R+}{$DEFINE WRONG}{$ENDIF}",
        "{$IFOPT R-}{$DEFINE SECOND}{$ENDIF}",
        "{$IFOPT Q+}{$DEFINE UNKNOWN}{$ENDIF}",
    );
    let analysis = conditional::analyze_with_context(source, &context);
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE FIRST"))
            .expect("first branch")
            .activity,
        Truth::True
    );
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE WRONG"))
            .expect("disabled branch")
            .activity,
        Truth::False
    );
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE SECOND"))
            .expect("negative option branch")
            .activity,
        Truth::True
    );
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE UNKNOWN"))
            .expect("unknown option branch")
            .activity,
        Truth::Unknown
    );
}

#[test]
fn typed_constants_support_checked_arithmetic_and_reject_unsafe_values() {
    let context =
        ConditionalContext::default().with_constant("Threshold", ConstantValue::Integer(3));
    let known = conditional::analyze_with_context(
        "{$IF ((Threshold * 2 + 1) = 7) and not false}{$DEFINE KNOWN}{$ENDIF}",
        &context,
    );
    assert_eq!(
        known
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE KNOWN"))
            .expect("known expression")
            .activity,
        Truth::True
    );

    for expression in [
        "9223372036854775807 + 1 = 0",
        "1 / 0 = 0",
        "UnsupportedFunction(Threshold) = 3",
    ] {
        let source = format!("{{$IF {expression}}}{{$DEFINE MAYBE}}{{$ENDIF}}");
        let analysis = conditional::analyze_with_context(&source, &context);
        assert_eq!(
            analysis
                .directives
                .iter()
                .find(|directive| directive.body.contains("DEFINE MAYBE"))
                .expect("unsafe expression branch")
                .activity,
            Truth::Unknown,
            "expression {expression:?} must stay unknown"
        );
    }
}

#[test]
fn proven_source_constants_and_unknown_branch_merges_are_conservative() {
    let source = concat!(
        "const Threshold = 3;",
        "{$IF Threshold >= 3}{$DEFINE VALUE_OK}{$ENDIF}",
        "{$IF UNKNOWN}{$DEFINE MAYBE}{$ELSE}{$UNDEF MAYBE}{$ENDIF}",
        "{$IFDEF MAYBE}{$DEFINE SHOULD_BE_UNKNOWN}{$ENDIF}",
    );
    let analysis = conditional::analyze_with_context(source, &ConditionalContext::default());
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE VALUE_OK"))
            .expect("source constant branch")
            .activity,
        Truth::True
    );
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE SHOULD_BE_UNKNOWN"))
            .expect("merged branch")
            .activity,
        Truth::Unknown
    );
}

#[test]
fn local_source_constants_and_real_division_are_not_treated_as_proven_facts() {
    let local = concat!(
        "procedure Local;\n",
        "const Threshold = 3;\n",
        "begin\n",
        "end;\n",
        "{$IF Threshold = 3}{$DEFINE LOCAL_VALUE}{$ENDIF}",
    );
    assert_eq!(
        directive_activity(local, "DEFINE LOCAL_VALUE", &ConditionalContext::default()),
        Truth::Unknown
    );

    let real_division = "{$IF 1 / 2 = 0}{$DEFINE INTEGER_DIVISION}{$ENDIF}";
    assert_eq!(
        directive_activity(
            real_division,
            "DEFINE INTEGER_DIVISION",
            &ConditionalContext::default(),
        ),
        Truth::Unknown
    );
}

#[test]
fn unsupported_global_constant_initializers_do_not_poison_projection() {
    let source = concat!(
        "unit Recovered; interface type TEmpty = class end; ",
        "const Known = 1; TRecord = record end; const Later = 2; ",
        "implementation end."
    );
    let analysis = conditional::analyze_with_context(source, &ConditionalContext::default());

    assert!(analysis.complete);
    assert!(analysis.unknown_spans.is_empty());
}

#[test]
fn compiler_version_uses_delphi_numeric_semantics_and_rejects_unsupported_precision() {
    let version = CompilerVersion::parse("18.5").expect("numeric compiler version");
    let context = ConditionalContext::default().with_compiler_version(version);

    assert_eq!(
        directive_activity(
            "{$IF CompilerVersion = 18.50}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::True
    );
    assert!(CompilerVersion::parse("24.0.1").is_none());
}

#[test]
fn compiler_version_decimal_order_is_numeric_not_componentwise() {
    let context = ConditionalContext::default()
        .with_compiler_version(CompilerVersion::parse("18.5").unwrap());
    assert_eq!(
        directive_activity(
            "{$IF CompilerVersion < 18.45}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::False
    );
}

#[test]
fn compiler_version_comparisons_do_not_narrow_large_integer_literals() {
    let context = ConditionalContext::default().with_compiler_version(CompilerVersion::new(18, 0));
    assert_eq!(
        directive_activity(
            "{$IF CompilerVersion = 4294967314}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::False
    );
}

#[test]
fn option_aliases_are_canonical_and_ifopt_requires_a_complete_operand() {
    let context = ConditionalContext::default()
        .with_option("R", Truth::True)
        .with_option("O", Truth::True);

    assert_eq!(
        directive_activity("{$IFOPT r+}{$DEFINE HIT}{$ENDIF}", "DEFINE HIT", &context),
        Truth::True
    );
}

#[test]
fn directly_constructed_context_maps_are_case_insensitive() {
    let mut context = ConditionalContext::default();
    context.defines.insert("feature".to_string(), Truth::True);
    context
        .constants
        .insert("limit".to_string(), ConstantValue::Integer(7));

    assert_eq!(
        directive_activity(
            "{$IFDEF FEATURE}{$DEFINE DEFINE_HIT}{$ENDIF}",
            "DEFINE_HIT",
            &context,
        ),
        Truth::True
    );
    assert_eq!(
        directive_activity(
            "{$IF LIMIT = 7}{$DEFINE CONSTANT_HIT}{$ENDIF}",
            "CONSTANT_HIT",
            &context,
        ),
        Truth::True
    );
}

#[test]
fn long_option_switch_updates_the_short_ifopt_alias() {
    let context = ConditionalContext::default()
        .with_option("R", Truth::True)
        .with_option("O", Truth::True);
    assert_eq!(
        directive_activity(
            "{$RANGECHECKS OFF}{$IFOPT R+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::False
    );
}

#[test]
fn short_option_switch_updates_the_long_ifopt_alias() {
    let context = ConditionalContext::default()
        .with_option("R", Truth::True)
        .with_option("O", Truth::True);
    assert_eq!(
        directive_activity(
            "{$O-}{$IFOPT O+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context
        ),
        Truth::False
    );
}

#[test]
fn malformed_ifopt_operand_is_unknown() {
    let context = ConditionalContext::default()
        .with_option("R", Truth::True)
        .with_option("O", Truth::True);
    assert_eq!(
        directive_activity("{$IFOPT R++}{$DEFINE HIT}{$ENDIF}", "DEFINE HIT", &context),
        Truth::Unknown
    );
}

#[test]
fn malformed_state_changing_option_clears_the_old_fact() {
    let context = ConditionalContext::default().with_option("R", Truth::True);
    assert_eq!(
        directive_activity(
            "{$RANGECHECKS maybe}{$IFOPT R+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::Unknown
    );
}

#[test]
fn malformed_option_suffix_does_not_retain_the_old_fact() {
    let context = ConditionalContext::default().with_option("R", Truth::True);
    assert_eq!(
        directive_activity(
            "{$R++}{$IFOPT R+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::Unknown
    );
}

#[test]
fn state_changing_option_directives_require_a_complete_operand() {
    let context = ConditionalContext::default().with_option("R", Truth::True);
    assert_eq!(
        directive_activity(
            "{$RANGECHECKS OFF unexpected}{$IFOPT R+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::Unknown
    );
    assert_eq!(
        directive_activity(
            "{$R+ unexpected}{$IFOPT R+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::Unknown
    );
}

#[test]
fn unsupported_state_changing_options_do_not_retain_old_facts() {
    let context = ConditionalContext::default().with_option("FutureSwitch", Truth::True);
    assert_eq!(
        directive_activity(
            "{$FutureSwitch OFF}{$IFOPT FutureSwitch+}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::Unknown
    );
}

#[test]
fn class_constants_do_not_become_unqualified_source_facts() {
    let class_constant = concat!(
        "unit X; interface type TFoo = class const X = 7; end; ",
        "{$IF X = 7}{$DEFINE HIT}{$ENDIF} implementation end."
    );
    assert_eq!(
        directive_activity(class_constant, "DEFINE HIT", &ConditionalContext::default(),),
        Truth::Unknown
    );
}

#[test]
fn local_source_constant_shadows_invalidate_the_global_fact() {
    let local_shadow = concat!(
        "unit X; interface const Limit = 1; implementation ",
        "procedure P; const Limit = 2; ",
        "{$IF Limit = 1}{$DEFINE HIT}{$ENDIF} begin end; end."
    );
    assert_eq!(
        directive_activity(local_shadow, "DEFINE HIT", &ConditionalContext::default()),
        Truth::Unknown
    );
}

#[test]
fn local_source_constants_do_not_leak_through_include_callbacks() {
    let mut environment =
        conditional::ConditionalEnvironment::from_context(&ConditionalContext::default());
    let mut include =
        |directive: &pascal_core::conditional::ConditionalDirective,
         environment: &mut conditional::ConditionalEnvironment| {
            assert_eq!(directive.kind, conditional::DirectiveKind::Include);
            let analysis = conditional::analyze_with_include_callback(
                "procedure P; const Limit = 2; begin end;",
                environment,
                &NoCancellation,
                &mut |_directive, _environment| IncludeTransition {
                    complete: true,
                    environment_known: true,
                },
            );
            IncludeTransition {
                complete: analysis.complete,
                environment_known: analysis.complete,
            }
        };
    let analysis = conditional::analyze_with_include_callback(
        "{$I local.inc}{$IF Limit = 1}{$DEFINE HIT}{$ENDIF}",
        &mut environment,
        &NoCancellation,
        &mut include,
    );
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE HIT"))
            .expect("include shadow probe")
            .activity,
        Truth::Unknown
    );
}

#[test]
fn declared_does_not_treat_define_facts_as_pascal_bindings() {
    for source in [
        "{$DEFINE FOO}{$IF Declared(FOO)}{$DEFINE HIT}{$ENDIF}",
        "{$UNDEF FOO}{$IF Declared(FOO)}{$DEFINE HIT}{$ENDIF}",
    ] {
        assert_eq!(
            directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
            Truth::Unknown,
            "compiler symbols are not Pascal declarations: {source}"
        );
    }
}

#[test]
fn defined_requires_function_syntax() {
    let mut context = ConditionalContext::default();
    context.set_define("FOO", Truth::True);
    assert_eq!(
        directive_activity(
            "{$IF Defined FOO}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &context,
        ),
        Truth::Unknown
    );
}

#[test]
fn sizeof_without_proven_type_width_is_unknown() {
    for source in [
        "{$IF SizeOf(65536) = 1}{$DEFINE HIT}{$ENDIF}",
        "{$IF SizeOf(String) = 1}{$DEFINE HIT}{$ENDIF}",
    ] {
        assert_eq!(
            directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
            Truth::Unknown,
            "untyped SizeOf must remain unknown: {source}"
        );
    }
}

#[test]
fn typed_function_arity_and_unary_operand_types_are_checked() {
    for source in [
        "{$IF Length('abc', 9) = 3}{$DEFINE HIT}{$ENDIF}",
        "{$IF +true}{$DEFINE HIT}{$ENDIF}",
        "{$IF Declared(FOO, BAR)}{$DEFINE HIT}{$ENDIF}",
    ] {
        assert_eq!(
            directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
            Truth::Unknown,
            "invalid typed expression must remain unknown: {source}"
        );
    }
    let declared = ConditionalContext::default().with_constant("FOO", ConstantValue::Boolean(true));
    assert_eq!(
        directive_activity(
            "{$IF Declared(FOO, BAR)}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &declared,
        ),
        Truth::Unknown
    );
    assert_eq!(
        directive_activity(
            "{$IF Declared FOO}{$DEFINE HIT}{$ENDIF}",
            "DEFINE HIT",
            &declared,
        ),
        Truth::Unknown
    );
}

#[test]
fn expression_errors_do_not_collapse_inside_boolean_composition() {
    let source = "{$IF true or (1 div 0 = 0)}{$DEFINE HIT}{$ENDIF}";
    assert_eq!(
        directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
        Truth::Unknown
    );
}

#[test]
fn oversized_constant_payload_is_rejected_before_analysis() {
    let context = ConditionalContext::default()
        .with_constant("Blob", ConstantValue::String("a".repeat(2 * 1024 * 1024)));
    let analysis = conditional::analyze_with_context(
        "{$IF Length(Blob) = 2097152}{$DEFINE HIT}{$ENDIF}",
        &context,
    );
    assert!(!analysis.complete);
    assert_eq!(
        analysis
            .directives
            .iter()
            .find(|directive| directive.body.contains("DEFINE HIT"))
            .expect("oversized payload branch")
            .activity,
        Truth::Unknown
    );
}

#[test]
fn oversized_constant_payload_is_rejected_for_include_callback_environments() {
    let context = ConditionalContext::default()
        .with_constant("Blob", ConstantValue::String("a".repeat(2 * 1024 * 1024)));
    let mut environment = conditional::ConditionalEnvironment::from_context(&context);
    let mut include =
        |_directive: &conditional::ConditionalDirective,
         _environment: &mut conditional::ConditionalEnvironment| {
            IncludeTransition {
                complete: true,
                environment_known: true,
            }
        };
    let analysis = conditional::analyze_with_include_callback(
        "{$IF Length(Blob) = 2097152}{$DEFINE HIT}{$ENDIF}",
        &mut environment,
        &NoCancellation,
        &mut include,
    );
    assert!(!analysis.complete);
}

#[test]
fn equal_unknown_branches_merge_option_facts_back_to_known() {
    let source = concat!(
        "{$IF Unknown}{$R+}{$ELSE}{$R+}{$ENDIF}",
        "{$IFOPT R+}{$DEFINE HIT}{$ENDIF}"
    );
    assert_eq!(
        directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
        Truth::True
    );
}

#[test]
fn nested_known_branch_inside_unknown_parent_can_merge_back_to_known() {
    let source = concat!(
        "{$IF Unknown}{$IF True}{$R+}{$ENDIF}{$ELSE}{$R+}{$ENDIF}",
        "{$IFOPT R+}{$DEFINE HIT}{$ENDIF}"
    );
    assert_eq!(
        directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
        Truth::True
    );
}

#[test]
fn equal_unknown_branches_merge_source_constants_back_to_known() {
    let source = concat!(
        "{$IF Unknown}const Shared = 7;{$ELSE}const Shared = 7;{$ENDIF}",
        "{$IF Shared = 7}{$DEFINE HIT}{$ENDIF}"
    );
    assert_eq!(
        directive_activity(source, "DEFINE HIT", &ConditionalContext::default()),
        Truth::True
    );
}

#[test]
fn environment_fingerprint_tags_fact_namespaces() {
    let option = ConditionalContext::default().with_option("R", Truth::True);
    let mut define_context = ConditionalContext::default();
    define_context.set_define("R", Truth::True);
    let left = conditional::ConditionalEnvironment::from_context(&define_context);
    let right = conditional::ConditionalEnvironment::from_context(&option);
    assert_ne!(left.fingerprint(), right.fingerprint());
    assert_eq!(left.fingerprint(), left.fingerprint());
}
