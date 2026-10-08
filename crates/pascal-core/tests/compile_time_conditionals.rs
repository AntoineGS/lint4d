//! Regression boundary for layout transport into the conditional evaluator.
mod support {
    pub mod compile_time;
}
use pascal_core::NoCancellation;
use pascal_core::compile_time::*;
use pascal_core::conditional::{
    ConditionalEnvironment, IncludeTransition, Truth, analyze_with_include_callback_context,
};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, LayoutPlatform,
    LayoutSettings,
};
use support::compile_time::{Fixture, activity, context};

fn assert_transport(context: &ConditionalContext) {
    let mut environment = ConditionalEnvironment::from_context(context);
    assert_eq!(environment.to_context().layout, context.layout);
    assert_eq!(
        environment.to_context().layout_explicit,
        context.layout_explicit
    );
    let mut seen = 0;
    let analysis = analyze_with_include_callback_context(
        "{$IF Mystery}{$DEFINE A}{$ELSE}{$DEFINE B}{$ENDIF}{$I child.inc}",
        &mut environment,
        &NoCancellation,
        &mut |_, environment| {
            seen += 1;
            let child_context = environment.to_context();
            assert_eq!(child_context.layout, context.layout);
            assert_eq!(child_context.layout_explicit, context.layout_explicit);
            // Exercise the public environment/context transition, not only a
            // direct clone: this used to erase source-entry layout provenance.
            *environment = ConditionalEnvironment::from_context(&child_context);
            IncludeTransition {
                complete: true,
                environment_known: true,
            }
        },
    );
    assert!(analysis.complete);
    assert_eq!(seen, 1);
    assert_eq!(environment.to_context().layout, context.layout);
    assert_eq!(
        environment.to_context().layout_explicit,
        context.layout_explicit
    );
}

#[test]
fn source_specific_project_and_library_layout_survive_branch_and_include_transitions() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        // Source-specific contexts are supplied by pascal-project. The core
        // evaluator must transport each independently, never rederive defaults.
        for (alignment, enums, strings) in [
            (Some(1), Some(4), ConditionalFact::False),
            (Some(8), None, ConditionalFact::True),
        ] {
            let context = ConditionalContext {
                compiler_version: Some(CompilerVersion::new(36, 0)),
                layout: LayoutContext {
                    platform: Some(platform.clone()),
                    defaults: LayoutSettings {
                        record_alignment: alignment,
                        minimum_enum_size: enums,
                        long_strings: strings,
                        ..Default::default()
                    },
                },
                layout_explicit: false,
                ..Default::default()
            };
            assert_transport(&context);
        }
    }
}

#[test]
fn explicit_unknown_layout_survives_environment_context_round_trip() {
    let context = ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(36, 0))
        .with_layout(LayoutContext::default());
    assert!(context.layout_explicit);
    assert_transport(&context);
}

#[test]
fn environment_identity_includes_target_settings_and_explicit_provenance() {
    let plain = ConditionalContext::default();
    let explicit_unknown = plain.clone().with_layout(LayoutContext::default());
    let win32 = plain.clone().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Win32),
        ..Default::default()
    });
    let win64 = plain.clone().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Win64),
        ..Default::default()
    });
    let aligned = plain.clone().with_layout(LayoutContext {
        defaults: LayoutSettings {
            record_alignment: Some(1),
            ..Default::default()
        },
        ..Default::default()
    });
    let base = ConditionalEnvironment::from_context(&plain).fingerprint();
    for context in [&explicit_unknown, &win32, &win64, &aligned] {
        let environment = ConditionalEnvironment::from_context(context);
        assert!(environment.has_facts());
        assert_ne!(base, environment.fingerprint());
        assert_eq!(environment.fingerprint(), environment.clone().fingerprint());
    }
    assert_ne!(
        ConditionalEnvironment::from_context(&win32).fingerprint(),
        ConditionalEnvironment::from_context(&win64).fingerprint()
    );
}

#[test]
fn unsupported_platform_payload_is_admitted_before_environment_clone() {
    let oversized = ConditionalContext::default().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Other("x".repeat(1024 * 1024 + 1))),
        ..Default::default()
    });
    assert!(ConditionalEnvironment::try_from_context(&oversized).is_none());
    let small = ConditionalContext::default().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Other("Unverified".into())),
        ..Default::default()
    });
    assert_transport(&small);
}

#[test]
fn unavailable_include_does_not_erase_immutable_source_entry_layout() {
    let context = ConditionalContext::default().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Win32),
        ..Default::default()
    });
    let mut environment = ConditionalEnvironment::from_context(&context);
    let analysis = analyze_with_include_callback_context(
        "{$I missing.inc}{$IF Defined(A)}{$DEFINE B}{$ENDIF}",
        &mut environment,
        &NoCancellation,
        &mut |_, _| IncludeTransition {
            complete: true,
            environment_known: false,
        },
    );
    assert!(analysis.complete);
    assert_eq!(analysis.directives[1].condition, Some(Truth::Unknown));
    assert_eq!(environment.to_context().layout, context.layout);
    assert_eq!(
        environment.to_context().layout_explicit,
        context.layout_explicit
    );
}

#[test]
fn sizeof_selects_target_branch_and_admits_initializer_value() {
    let source = "unit T; interface const Width=SizeOf(LongWord); \
        {$IF Width=4}{$DEFINE FIXED}{$ENDIF} \
        {$IF SizeOf(Extended)>=10}{$DEFINE WIDE}{$ELSE}{$DEFINE NARROW}{$ENDIF} \
        implementation end.";
    for (platform, wide, narrow) in [
        (LayoutPlatform::Win32, Truth::True, Truth::False),
        (LayoutPlatform::Win64, Truth::False, Truth::True),
    ] {
        let mut f = Fixture::new(source, context(platform));
        let a = f.analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE FIXED"), Truth::True);
        assert_eq!(activity(&a, "DEFINE WIDE"), wide);
        assert_eq!(activity(&a, "DEFINE NARROW"), narrow);
    }
}

#[test]
fn sizeof_all_operand_categories_work_in_conditions_and_initializers() {
    let source = "unit T; interface type Alias=Word; R=packed record X:Byte; Y:Word; end; \
        A=array[0..2] of Alias; P=^R; var v:R; a1:A; p1:P; i:Integer; \
        const S=SizeOf(Alias)+SizeOf(R)+SizeOf(A)+SizeOf(v)+SizeOf(v.Y)+SizeOf(a1[0])+SizeOf(p1^)+SizeOf(i); \
        {$IF S=25}{$DEFINE CONST_OK}{$ENDIF} \
        {$IF (SizeOf(Byte)=1) and (SizeOf(Alias)=2) and (SizeOf(R)=3) and (SizeOf(A)=6) and (SizeOf(v)=3) and (SizeOf(v.Y)=2) and (SizeOf(a1[0])=2) and (SizeOf(p1^)=3) and (SizeOf(i)=4)}{$DEFINE OPERANDS_OK}{$ENDIF} \
        implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE CONST_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE OPERANDS_OK"), Truth::True);
}

#[test]
fn sizeof_respects_captured_aliases_switches_and_source_order_bounds() {
    let source = "unit T; interface {$A1} type R=record X:Byte; Y:Integer; end; \
        {$A8} type S=record X:Byte; Y:Integer; end; const N=SizeOf(R); \
        type A=array[1..N] of Word; {$IF (SizeOf(R)=5) and (SizeOf(S)=8) and (SizeOf(A)=10)}{$DEFINE ORDER_OK}{$ENDIF} \
        {$IF SizeOf(Later)=1}{$DEFINE TOO_EARLY}{$ENDIF} type Later=Byte; \
        {$IF SizeOf(Later)=1}{$DEFINE LATER_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    // A valid unresolved query does not require a branch-state fork: its body
    // is inactive in no proven path, so phase A conservatively halts typed facts.
    assert_eq!(activity(&a, "DEFINE ORDER_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE TOO_EARLY"), Truth::Unknown);
}

#[test]
fn scoped_parameters_earlier_constants_and_alias_sites_shadow_context() {
    let source = "unit T; interface type Alias=Word; Saved=Alias; const Width=SizeOf(Saved); \
        implementation procedure Test(p:Byte); type Alias=Int64; const Width=SizeOf(p); \
        begin {$IF (Width=1) and (SizeOf(Saved)=2) and (SizeOf(Alias)=8) and (SizeOf(p)=1)}{$DEFINE LOCAL_OK}{$ENDIF} end; \
        {$IF Width=2}{$DEFINE OUTER_OK}{$ENDIF} end.";
    let ctx = context(LayoutPlatform::Win32).with_constant(
        "Width",
        pascal_core::conditional::ConstantValue::Integer(99),
    );
    let a = Fixture::new(source, ctx).analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE LOCAL_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE OUTER_OK"), Truth::True);
}

#[test]
fn known_inactive_malformed_declarations_do_not_poison_reachable_state() {
    let source = "unit T; interface {$IF false}type Broken = record X: ; nope;{$ELSE}type R=Byte;{$ENDIF} \
        const N=SizeOf(R)+Length('abc')+Ord('A'); {$IF N=69}{$DEFINE MIXED_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE MIXED_OK"), Truth::True);
}

#[test]
fn unknown_branches_are_explicitly_incomplete_without_splicing_phase_a() {
    let source = "unit T; interface {$IF Mystery}type R=Byte;{$ELSE}type R=Int64;{$ENDIF} \
        {$IF SizeOf(R)=1}{$DEFINE WRONG}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(!a.root.complete);
    assert!(!a.reasons.is_empty());
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
}

#[test]
fn malformed_sizeof_arity_is_distinct_from_valid_unknown_operand() {
    for expr in [
        "SizeOf()",
        "SizeOf(Integer,Byte)",
        "SizeOf(Integer",
        "SizeOf(Integer,)",
    ] {
        let source = format!(
            "unit T; interface {{$IF {expr}=4}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
        );
        let a = Fixture::new(&source, context(LayoutPlatform::Win32))
            .analyze()
            .unwrap();
        assert!(!a.root.complete, "{expr}");
        assert!(
            a.reasons.contains(&IncompleteReason::MalformedSource),
            "{expr}: {:?}",
            a.reasons
        );
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::Unknown);
    }
    // No unknown IF is needed to check expression admission: unsupported
    // initializers remain unknown values, not malformed source admissions.
    for expr in ["SizeOf(65536)", "SizeOf(F())", "SizeOf(Missing)"] {
        let source = format!(
            "unit T; interface const N={expr}; {{$IF true}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
        );
        let a = Fixture::new(&source, context(LayoutPlatform::Win32))
            .analyze()
            .unwrap();
        assert!(a.root.complete, "{expr}: {:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    }
}

#[test]
fn root_occurrence_carriers_and_unavailable_resources_are_explicit() {
    let mut f = Fixture::new(
        "unit T; interface {$I child.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("child.inc", "type R=Byte;");
    let a = f.analyze().unwrap();
    assert_eq!(a.occurrences.len(), 1);
    assert_eq!(a.occurrences[0].id, OccurrenceId(0));
    assert_eq!(a.occurrences[0].source.id, f.root.id);
    assert_eq!(a.header, Fact::Unknown(UnknownReason::HeaderNotAnalyzed));
    assert!(matches!(
        &a.includes[0].target,
        pascal_core::Resolution::Unavailable { .. }
    ));
    assert_eq!(a.includes[0].child, None);
    assert!(matches!(
        &a.reasons[0],
        IncompleteReason::RequiredSource { .. }
    ));
    assert!(f.read_names().is_empty());
}

#[test]
fn completed_split_initializer_and_source_defines_are_visible_to_next_directive() {
    let source = "unit T; interface const N= {$IF SizeOf(Integer)=4} SizeOf(Byte)+3 {$ELSE} 0 {$ENDIF}; \
        {$IF N=4}{$DEFINE READY}{$ENDIF}{$IF Defined(READY)}{$DEFINE SEEN}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE READY"), Truth::True);
    assert_eq!(activity(&a, "DEFINE SEEN"), Truth::True);
}

#[test]
fn source_switch_stack_captures_string_layout_and_current_query_settings() {
    let source = "unit T; interface {$PUSHOPT}{$H-} type S=string; {$POPOPT} type L=string; \
        {$IF (SizeOf(S)=256) and (SizeOf(L)=4) and (SizeOf(String)=4)}{$DEFINE SWITCH_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE SWITCH_OK"), Truth::True);
}

#[test]
fn unsupported_sizeof_value_preserves_existing_conservative_boolean_semantics() {
    let source = "unit T; interface {$IF true or (SizeOf(65536)=1)}{$DEFINE PROVEN}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(!a.root.complete);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
    assert_eq!(activity(&a, "DEFINE PROVEN"), Truth::Unknown);
}

#[test]
fn captured_identity_keeps_the_physical_declaration_not_the_query_location() {
    let source = "unit T; interface type R=Word; var value:R; const N=SizeOf(value); \
        (*$IF (N=2) and (SizeOf( value )=2)*){$DEFINE MAPPED}{$ENDIF} implementation end.";
    let mut f = Fixture::new(source, context(LayoutPlatform::Win32));
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE MAPPED"), Truth::True);
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), source.len())
        .unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let operand = parse_sizeof_operand("value", &mut budget).known().unwrap();
    let known = a
        .facts
        .layout_operand(site, &operand, &mut budget)
        .known()
        .unwrap();
    let TypeIdentity::Declared(key) = known.identity else {
        panic!("lost variable declaration identity");
    };
    assert_eq!(key.anchor.source, f.root.id);
    assert_eq!(key.anchor.occurrence, OccurrenceId(0));
    let start = source.find("value:R").unwrap();
    assert_eq!(key.anchor.range, start..start + "value".len());
}

#[test]
fn root_session_limits_and_cancellation_never_publish_a_precise_result() {
    let mut f = Fixture::new(
        "unit T; interface type R=Word; {$IF SizeOf(R)=2}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_retained_bytes = 0;
    let error = f.analyze().unwrap_err();
    let pascal_core::ResolverError::LimitExceeded {
        observed,
        maximum,
        limit,
    } = error
    else {
        panic!("unexpected root admission error");
    };
    assert_eq!(limit, "max_retained_bytes");
    assert_eq!(maximum, 0);
    assert_eq!(
        observed,
        f.root.analysis_bytes().len() * 4
            + std::mem::size_of::<CompileTimeAnalysis>()
            + std::mem::size_of::<OccurrenceAnalysis>()
            + f.root.id.as_str().len()
            + f.root.path.as_os_str().len()
    );
    struct Cancel;
    impl pascal_core::CancellationToken for Cancel {
        fn is_cancelled(&self) -> bool {
            true
        }
    }
    assert_eq!(
        analyze_source(
            &f.root,
            &f.context,
            &mut support::compile_time::NoResources,
            CompileTimeLimits::default(),
            &Cancel
        )
        .unwrap_err(),
        pascal_core::ResolverError::Cancelled
    );
}

#[test]
fn completed_constant_captures_the_site_before_a_later_type_is_declared() {
    let source = "unit T; interface const Before=SizeOf(Later); type Later=Byte; const After=SizeOf(Later); \
        {$IF After=1}{$DEFINE AFTER_OK}{$ENDIF}{$IF Before=1}{$DEFINE RETROACTIVE}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE AFTER_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE RETROACTIVE"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}

#[test]
fn sizeof_is_static_and_does_not_admit_runtime_values_or_calls() {
    let source = "unit T; interface var v:Word; function GetValue:Word; \
        const N=SizeOf(v); Bad=SizeOf(GetValue()); {$IF N=2}{$DEFINE STATIC_OK}{$ENDIF} \
        {$IF v=0}{$DEFINE RUNTIME}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE STATIC_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE RUNTIME"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}

#[test]
fn existing_boolean_switch_lists_update_layout_facts_in_order() {
    let source = "unit T; interface {$A+,H-} type S=String; {$B+,H+} type L=String; \
        {$IF (SizeOf(S)=256) and (SizeOf(L)=4) and (SizeOf(String)=4)}{$DEFINE LIST_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE LIST_OK"), Truth::True);
}

#[test]
fn nested_sizeof_array_queries_share_the_structural_depth_budget() {
    let mut f = Fixture::new(
        "unit T; interface type R=array[1..SizeOf(R)] of Byte; {$IF SizeOf(R)>0}{$DEFINE RECURSIVE}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_type_depth = 4;
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert!(a.reasons.iter().any(|r| matches!(
        r,
        IncompleteReason::Limit {
            name: "max_type_depth",
            maximum: 4
        }
    )));
    assert_eq!(activity(&a, "DEFINE RECURSIVE"), Truth::Unknown);
}

#[test]
fn ordinal_initializer_uses_the_same_sizeof_constant_bridge() {
    let source = "unit T; interface {$Z1} type E=(e0=SizeOf(Byte), e1=e0+1); \
        {$IF SizeOf(E)=1}{$DEFINE ENUM_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE ENUM_OK"), Truth::True);
}

#[test]
fn lexical_compiler_version_constant_and_parameter_block_metadata_fallback() {
    let source = "unit T; interface const CompilerVersion=SizeOf(Integer); \
        {$IF CompilerVersion=4}{$DEFINE LEXICAL_VERSION}{$ENDIF} implementation \
        procedure P(CompilerVersion:Byte); begin {$IF SizeOf(CompilerVersion)=1}{$DEFINE PARAM_TYPE}{$ENDIF} \
        {$IF CompilerVersion=4}{$DEFINE PARAM_VALUE}{$ENDIF} end; end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE LEXICAL_VERSION"), Truth::True);
    assert_eq!(activity(&a, "DEFINE PARAM_TYPE"), Truth::True);
    assert_eq!(activity(&a, "DEFINE PARAM_VALUE"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}

#[test]
fn scalar_option_restore_without_complete_snapshots_is_conservative_until_phase_b() {
    let source = "unit T; interface {$B+}{$PUSHOPT}{$B-}{$POPOPT}{$IFOPT B+}{$DEFINE RESTORED}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE RESTORED"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}

#[test]
fn source_constant_preserves_explicit_compiler_version_value_semantics() {
    let source = "unit T; interface const CV=CompilerVersion; {$IF CV>=37}{$DEFINE VERSION_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE VERSION_OK"), Truth::True);
}

#[test]
fn declared_preserves_explicit_rtl_closed_world_context() {
    let mut ctx = context(LayoutPlatform::Win32);
    ctx.rtl_constants_known = true;
    let a = Fixture::new("unit T; interface {$IF Declared(RTLVersion113)}{$DEFINE ABSENT}{$ELSE}{$DEFINE CLOSED}{$ENDIF} implementation end.", ctx).analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE ABSENT"), Truth::False);
    assert_eq!(activity(&a, "DEFINE CLOSED"), Truth::True);
}

#[test]
fn scoped_qualifier_variable_blocks_explicit_constant_value_fallback() {
    let ctx = context(LayoutPlatform::Win32).with_constant(
        "System.RTLVersion113",
        pascal_core::conditional::ConstantValue::Boolean(true),
    );
    let a = Fixture::new("unit T; interface implementation procedure P; var System:Integer; begin {$IF System.RTLVersion113}{$DEFINE WRONG}{$ENDIF} end; end.", ctx).analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}

#[test]
fn earlier_enum_member_blocks_qualified_context_fallback_in_its_initializer() {
    let ctx = context(LayoutPlatform::Win32).with_constant(
        "System.K",
        pascal_core::conditional::ConstantValue::Integer(1),
    );
    let a = Fixture::new("unit T; interface {$Z1} type E=(System=0,e1=System.K); {$IF SizeOf(E)=1}{$DEFINE WRONG_ENUM}{$ENDIF} implementation end.", ctx).analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE WRONG_ENUM"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}

#[test]
fn source_routines_block_later_intrinsic_calls_without_rebinding_earlier_constants() {
    let source = "unit T; interface const Early=Length('x')+Ord('A')+SizeOf(Integer); \
        function Length(x:string):Integer; function Ord(x:Char):Integer; function SizeOf(x:Integer):Integer; \
        const Late=Length('x')+Ord('A')+SizeOf(Integer); \
        {$IF Early=70}{$DEFINE EARLY_OK}{$ENDIF}{$IF Late=70}{$DEFINE RUNTIME_CALLS}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE EARLY_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE RUNTIME_CALLS"), Truth::Unknown);
    assert_eq!(a.reasons, vec![IncompleteReason::BranchStateNotAnalyzed]);
}
