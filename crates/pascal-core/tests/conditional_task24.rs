use pascal_core::conditional::{self, CompilerVersion, ConditionalContext, ConstantValue, Truth};

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
