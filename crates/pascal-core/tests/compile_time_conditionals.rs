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

#[test]
fn fix2_i4_ord_of_proven_enum_preserves_independent_later_proof() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let source = "unit T; interface {$DEFINE FLAG} type E=(e0=Ord(Defined(FLAG)),e1=e0+1); \
            {$IF Ord(e0)=1}{$DEFINE HIT}{$ENDIF} \
            {$IF SizeOf(Integer)=4}{$DEFINE INDEPENDENT}{$ENDIF} implementation end.";
        let a = Fixture::new(source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
        assert_eq!(activity(&a, "DEFINE INDEPENDENT"), Truth::True);
        assert_eq!(a.facts.reachable_states().count(), 1);
    }
}

#[test]
fn fix2_i4_ord_initializers_retain_opposing_enum_capture_and_branch_correlation() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let source = "unit T; interface {$IF Mystery}{$DEFINE LEFT}{$DEFINE FLAG} \
            type E=(e0=Ord(Defined(FLAG)),e1=e0+1);{$ELSE}{$UNDEF LEFT}{$UNDEF FLAG} \
            type E=(e0=Ord(Defined(FLAG)),e1=e0+1);{$ENDIF}{$DEFINE FLAG} \
            const N=Ord(e0); {$IF Defined(LEFT)} \
            {$IF (N=1) and (Ord(e1)=2)}{$DEFINE LEFT_OK}{$ELSE}{$DEFINE LEFT_WRONG}{$ENDIF} \
            {$ELSE}{$IF (N=0) and (Ord(e1)=1)}{$DEFINE RIGHT_OK}{$ELSE}{$DEFINE RIGHT_WRONG}{$ENDIF}{$ENDIF} \
            {$IF SizeOf(Integer)=4}{$DEFINE INDEPENDENT}{$ENDIF} implementation end.";
        let a = Fixture::new(source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        for body in ["DEFINE LEFT_OK", "DEFINE RIGHT_OK"] {
            assert_eq!(activity(&a, body), Truth::Unknown, "{body}");
        }
        for body in ["DEFINE LEFT_WRONG", "DEFINE RIGHT_WRONG"] {
            assert_eq!(activity(&a, body), Truth::False, "{body}");
        }
        assert_eq!(activity(&a, "DEFINE INDEPENDENT"), Truth::True);
        assert_eq!(a.facts.reachable_states().count(), 2);
    }
}

#[test]
fn fix2_i4_valid_unknown_ord_does_not_poison_independent_proof() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let source = "unit T; interface type E=(e0=Ord(Defined(UNKNOWN)),e1=e0+1); \
            {$IF Ord(e0)=1}{$DEFINE UNKNOWN_ORD}{$ENDIF} \
            {$IF SizeOf(Integer)=4}{$DEFINE INDEPENDENT}{$ENDIF} implementation end.";
        let a = Fixture::new(source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE UNKNOWN_ORD"), Truth::Unknown);
        assert_eq!(activity(&a, "DEFINE INDEPENDENT"), Truth::True);
    }
}

#[test]
fn fix2_i4_ord_empty_and_multiple_arguments_remain_malformed() {
    for expression in ["Ord()", "Ord(e0,e1)", "Ord(e0,UNKNOWN)"] {
        let source = format!(
            "unit T; interface {{$DEFINE FLAG}} type E=(e0=Ord(Defined(FLAG)),e1=e0+1); \
            {{$IF {expression}=1}}{{$DEFINE INVALID}}{{$ENDIF}} \
            {{$IF SizeOf(Integer)=4}}{{$DEFINE INDEPENDENT}}{{$ENDIF}} implementation end."
        );
        let a = Fixture::new(&source, context(LayoutPlatform::Win32))
            .analyze()
            .unwrap();
        assert!(!a.root.complete, "{expression}");
        assert!(
            a.reasons.contains(&IncompleteReason::MalformedSource),
            "{expression}"
        );
        assert_eq!(activity(&a, "DEFINE INDEPENDENT"), Truth::Unknown);
    }
}

#[test]
fn fix1_i1_interrupted_initializers_capture_each_sizeof_and_scalar_query_site() {
    for (platform, pointer) in [(LayoutPlatform::Win32, 4), (LayoutPlatform::Win64, 8)] {
        let source = format!(
            "unit T; interface {{$H+}} const N=SizeOf(String){{$H-}}; \
            {{$H+}} const Mixed=SizeOf(String){{$H-}}+SizeOf(String); \
            {{$DEFINE FLAG}} const Enabled=Defined(FLAG){{$UNDEF FLAG}}; \
            {{$DEFINE FLAG}} const Count=Ord(Defined(FLAG)){{$UNDEF FLAG}}+Ord(Defined(FLAG)); \
            {{$IF (N={pointer}) and (Mixed={}) and Enabled and (Count=1)}}{{$DEFINE CAPTURED}}{{$ENDIF}} implementation end.",
            pointer + 256
        );
        let a = Fixture::new(&source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE CAPTURED"), Truth::True);
        assert_eq!(a.facts.reachable_states().count(), 1);
    }
}

#[test]
fn fix1_i1_scalar_history_is_captured_before_initializer_completion() {
    let source = "unit T; interface {$DEFINE FLAG} const Enabled=Defined(FLAG){$UNDEF FLAG}; \
        {$DEFINE FLAG} const Count=Ord(Defined(FLAG)){$UNDEF FLAG}+Ord(Defined(FLAG)); \
        {$IF Enabled and (Count=1)}{$DEFINE SCALAR_CAPTURED}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE SCALAR_CAPTURED"), Truth::True);
    assert_eq!(a.facts.reachable_states().count(), 1);
}

#[test]
fn fix1_i2_enum_initializers_capture_defines_and_interrupted_member_sites() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let source = "unit T; interface {$DEFINE FLAG}{$Z1} type E=(e0=Ord(Defined(FLAG)){$UNDEF FLAG}, e1=3+Ord(Defined(FLAG))); \
            A=array[E] of Byte; {$DEFINE FLAG}{$IF (SizeOf(A)=3) and (SizeOf(E)=1)}{$DEFINE ENUM_CAPTURED}{$ENDIF} implementation end.";
        let a = Fixture::new(source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE ENUM_CAPTURED"), Truth::True);
    }
}

#[test]
fn fix1_i2_enum_sizeof_uses_the_members_physical_operand_site() {
    for (platform, pointer) in [(LayoutPlatform::Win32, 4), (LayoutPlatform::Win64, 8)] {
        let source = format!(
            "unit T; interface {{$H+}} type E=(low=SizeOf(String){{$H-}},high=300); \
            A=array[E] of Byte; {{$IF SizeOf(A)={}}}{{$DEFINE ENUM_SITE}}{{$ENDIF}} implementation end.",
            301 - pointer
        );
        let a = Fixture::new(&source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE ENUM_SITE"), Truth::True);
    }
}

#[test]
fn fix1_i2_enum_snapshots_remain_correlated_after_a_common_define() {
    let source = "unit T; interface {$Z1}{$IF Mystery}{$DEFINE FLAG}type E=(e0=Ord(Defined(FLAG)),e1=3); \
        {$ELSE}{$UNDEF FLAG}type E=(e0=Ord(Defined(FLAG)),e1=3);{$ENDIF}type A=array[E] of Byte;{$DEFINE FLAG} \
        {$IF SizeOf(A)=3}{$DEFINE ONE}{$ELSE}{$DEFINE ZERO}{$ENDIF} \
        {$IF SizeOf(E)=1}{$DEFINE ENUM_SIZE}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE ONE"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE ZERO"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE ENUM_SIZE"), Truth::True);
    assert_eq!(a.facts.reachable_states().count(), 2);
}

#[test]
fn fix1_i3_direct_and_initializer_declared_and_bare_defines_share_policy() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let mut ctx = context(platform).with_constant(
            "Known",
            pascal_core::conditional::ConstantValue::Boolean(true),
        );
        ctx.defines.insert("EXPLICIT".into(), Truth::True);
        ctx.rtl_constants_known = true;
        let source = "unit T; interface {$DEFINE SOURCE} \
            {$IF Declared(Known) and EXPLICIT and SOURCE}{$DEFINE DIRECT}{$ENDIF} \
            const Has=Declared(Known); Enabled=EXPLICIT; Local=SOURCE; Absent=Declared(RTLVersion113); \
            {$IF Has and Enabled and Local and (Absent=false)}{$DEFINE INITIALIZERS}{$ENDIF} \
            {$IF Declared(RTLVersion113)=false}{$DEFINE DIRECT_ABSENCE}{$ENDIF} implementation end.";
        let a = Fixture::new(source, ctx).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        for body in [
            "DEFINE DIRECT",
            "DEFINE INITIALIZERS",
            "DEFINE DIRECT_ABSENCE",
        ] {
            assert_eq!(activity(&a, body), Truth::True, "{body}");
        }
    }
}

#[test]
fn fix1_i3_lexical_and_previous_enum_members_block_context_fallbacks() {
    let mut ctx = context(LayoutPlatform::Win32).with_constant(
        "System.K",
        pascal_core::conditional::ConstantValue::Integer(7),
    );
    ctx.defines.insert("FLAG".into(), Truth::True);
    let source = "unit T; interface var FLAG:Integer; System:Integer; \
        const Enabled=FLAG; Has=Declared(System.K); Value=System.K; \
        {$IF FLAG}{$DEFINE DIRECT_BLOCKED}{$ENDIF}{$IF Enabled}{$DEFINE CONST_BLOCKED}{$ENDIF} \
        {$IF Has or (Value=7)}{$DEFINE QUALIFIED_BLOCKED}{$ENDIF} \
        {$Z1} type E=(previous=0,next=Ord(Declared(previous))+2); A=array[E] of Byte; \
        {$IF SizeOf(A)=4}{$DEFINE PREVIOUS_DECLARED}{$ENDIF} implementation end.";
    let a = Fixture::new(source, ctx).analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    for body in [
        "DEFINE DIRECT_BLOCKED",
        "DEFINE CONST_BLOCKED",
        "DEFINE QUALIFIED_BLOCKED",
    ] {
        assert_eq!(activity(&a, body), Truth::Unknown, "{body}");
    }
    assert_eq!(activity(&a, "DEFINE PREVIOUS_DECLARED"), Truth::True);
}

#[test]
fn phase_b_reachable_branches_keep_correlated_declarations_and_switches() {
    let source = "unit T; interface {$IF Mystery}{$DEFINE SMALL}type R=Byte;{$ELSE}{$UNDEF SMALL}type R=Int64;{$ENDIF} \
        {$IF Defined(SMALL)}{$IF SizeOf(R)=1}{$DEFINE SMALL_OK}{$ELSE}{$DEFINE WRONG_SMALL}{$ENDIF}{$ELSE}\
        {$IF SizeOf(R)=8}{$DEFINE LARGE_OK}{$ELSE}{$DEFINE WRONG_LARGE}{$ENDIF}{$ENDIF} \
        {$IF SizeOf(R)=1}{$DEFINE MAYBE}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE SMALL_OK"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE LARGE_OK"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE WRONG_SMALL"), Truth::False);
    assert_eq!(activity(&a, "DEFINE WRONG_LARGE"), Truth::False);
    assert_eq!(activity(&a, "DEFINE MAYBE"), Truth::Unknown);
}

#[test]
fn phase_b_equal_size_partial_records_and_initializers_prove_all_paths() {
    let source = "unit T; interface type R=packed record X:{$IF Mystery}Byte{$ELSE}ShortInt{$ENDIF}; end; \
        const N=SizeOf({$IF Mystery}R{$ELSE}Byte{$ENDIF}); type A=array[1..N] of Word; \
        {$IF (N=1) and (SizeOf(A)=2)}{$DEFINE ALL_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE ALL_OK"), Truth::True);
}

#[test]
fn phase_b_missing_else_retains_the_unchanged_reachable_state() {
    let source = "unit T; interface {$IF Mystery}type R=Byte;{$ENDIF} \
        {$IF SizeOf(R)=1}{$DEFINE MAYBE}{$ENDIF} type Later=Word; \
        {$IF SizeOf(Later)=2}{$DEFINE LATER_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE MAYBE"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE LATER_OK"), Truth::True);
}

#[test]
fn phase_b_sequential_splits_are_capped_not_only_nested_depth() {
    for (splits, complete) in [(6, true), (7, false)] {
        let mut source = String::from("unit T; interface ");
        for i in 0..splits {
            source.push_str(&format!(
                "{{$IF Mystery{i}}}{{$DEFINE X{i}}}{{$ELSE}}{{$UNDEF X{i}}}{{$ENDIF}}"
            ));
        }
        source.push_str("implementation end.");
        let a = Fixture::new(&source, context(LayoutPlatform::Win32))
            .analyze()
            .unwrap();
        assert_eq!(a.root.complete, complete, "{:?}", a.reasons);
        if !complete {
            assert!(a.reasons.iter().any(|r| matches!(
                r,
                IncompleteReason::Limit {
                    name: "max_branch_states",
                    maximum: 64
                }
            )));
        }
    }
}

#[test]
fn phase_b_caller_branch_budget_is_shared_with_sequential_forks() {
    let mut f = Fixture::new(
        "unit T; interface {$IF Mystery}type R=Byte;{$ELSE}type R=Word;{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_branch_states = 1;
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert!(a.reasons.contains(&IncompleteReason::Limit {
        name: "max_branch_states",
        maximum: 1
    }));
}

#[test]
fn phase_b_whole_restore_covers_documented_scalar_options_without_restoring_defines() {
    for option in ["R", "O", "Q", "C", "B", "I", "X", "D", "H", "T", "Y"] {
        let source = format!(
            "unit T; interface {{$UNDEF FLAG}}{{${option}+}}{{$PUSHOPT}}{{${option}-}}{{$DEFINE FLAG}}{{$POPOPT}}{{$IFOPT {option}+}}{{$DEFINE RESTORED}}{{$ENDIF}}{{$IF Defined(FLAG)}}{{$DEFINE DEFINE_KEPT}}{{$ENDIF}} implementation end."
        );
        let a = Fixture::new(&source, context(LayoutPlatform::Win32))
            .analyze()
            .unwrap();
        assert!(a.root.complete, "{option}: {:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE RESTORED"), Truth::True, "{option}");
        assert_eq!(activity(&a, "DEFINE DEFINE_KEPT"), Truth::True, "{option}");
    }
}

#[test]
fn option_evidence_t_y_whole_restore_keeps_saved_facts_and_source_constants() {
    let source = "unit T; interface {$T-}{$Y+}{$PUSHOPT}{$TYPEDADDRESS ON}{$REFERENCEINFO OFF} \
        const Width=SizeOf(Word); {$DEFINE DURING_SAVE}{$POPOPT} \
        {$IFOPT T-}{$DEFINE T_RESTORED}{$ENDIF}{$IFOPT Y+}{$DEFINE Y_RESTORED}{$ENDIF} \
        {$IF (Width=2) and Defined(DURING_SAVE)}{$DEFINE SOURCE_FACTS_KEPT}{$ENDIF} implementation end.";
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let a = Fixture::new(source, context(platform)).analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        for body in [
            "DEFINE T_RESTORED",
            "DEFINE Y_RESTORED",
            "DEFINE SOURCE_FACTS_KEPT",
        ] {
            assert_eq!(activity(&a, body), Truth::True, "{body}");
        }
    }
}

#[test]
fn option_evidence_t_y_nested_saves_preserve_unknown_outer_facts() {
    let source = "unit T; interface {$PUSHOPT}{$T+}{$Y-}{$PUSHOPT}{$T-}{$Y+}{$POPOPT} \
        {$IFOPT T+}{$DEFINE INNER_T}{$ENDIF}{$IFOPT Y-}{$DEFINE INNER_Y}{$ENDIF}{$POPOPT} \
        {$IFOPT T+}{$DEFINE UNKNOWN_T}{$ENDIF}{$IFOPT Y-}{$DEFINE UNKNOWN_Y}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE INNER_T"), Truth::True);
    assert_eq!(activity(&a, "DEFINE INNER_Y"), Truth::True);
    assert_eq!(activity(&a, "DEFINE UNKNOWN_T"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE UNKNOWN_Y"), Truth::Unknown);
}

#[test]
fn option_evidence_t_y_restore_remains_correlated_with_each_branch() {
    let source = "unit T; interface {$IF Mystery}{$DEFINE LEFT}{$T-}{$Y+}{$ELSE}{$UNDEF LEFT}{$T+}{$Y-}{$ENDIF} \
        {$PUSHOPT}{$T+}{$Y+}{$POPOPT} {$IF Defined(LEFT)} \
        {$IFOPT T-}{$DEFINE LEFT_T}{$ELSE}{$DEFINE WRONG_LEFT_T}{$ENDIF} \
        {$IFOPT Y+}{$DEFINE LEFT_Y}{$ELSE}{$DEFINE WRONG_LEFT_Y}{$ENDIF} {$ELSE} \
        {$IFOPT T+}{$DEFINE RIGHT_T}{$ELSE}{$DEFINE WRONG_RIGHT_T}{$ENDIF} \
        {$IFOPT Y-}{$DEFINE RIGHT_Y}{$ELSE}{$DEFINE WRONG_RIGHT_Y}{$ENDIF} {$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    for body in [
        "DEFINE LEFT_T",
        "DEFINE LEFT_Y",
        "DEFINE RIGHT_T",
        "DEFINE RIGHT_Y",
    ] {
        assert_eq!(activity(&a, body), Truth::Unknown, "{body}");
    }
    for body in [
        "DEFINE WRONG_LEFT_T",
        "DEFINE WRONG_LEFT_Y",
        "DEFINE WRONG_RIGHT_T",
        "DEFINE WRONG_RIGHT_Y",
    ] {
        assert_eq!(activity(&a, body), Truth::False, "{body}");
    }
    assert_eq!(a.facts.reachable_states().count(), 2);
}

#[test]
fn option_evidence_runtime_checks_changed_restore_stays_unverified() {
    let source = "unit T; interface {$RUNTIME_CHECKS ON}{$PUSHOPT}{$RUNTIME_CHECKS OFF}{$POPOPT} \
        {$IFOPT RUNTIME_CHECKS+}{$DEFINE UNVERIFIED}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE UNVERIFIED"), Truth::Unknown);
}

#[test]
fn phase_b_reachable_failures_pending_and_depth_do_not_publish_precise_facts() {
    let mut f = Fixture::new(
        "unit T; interface {$IF Mystery}type R=packed record X:Byte;{$ELSE}type R=Byte;{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_pending_bytes = 8;
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert!(a.reasons.iter().any(|r| matches!(
        r,
        IncompleteReason::Limit {
            name: "max_pending_bytes",
            ..
        }
    )));
    assert_eq!(a.root.directives[0].activity, Truth::Unknown);
    let mut source = String::from("unit T; interface ");
    for _ in 0..257 {
        source.push_str("{$IF false}");
    }
    for _ in 0..257 {
        source.push_str("{$ENDIF}");
    }
    source.push_str("implementation end.");
    let a = Fixture::new(&source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(!a.root.complete);
    assert!(a.reasons.iter().any(|r| matches!(
        r,
        IncompleteReason::Limit {
            name: "max_conditional_depth",
            maximum: 256
        }
    )));
}

#[test]
fn phase_b_equal_scalar_sizes_do_not_invent_one_layout_identity() {
    let source = "unit T; interface {$IF Mystery}type R=Byte;{$ELSE}type R=ShortInt;{$ENDIF} \
        {$IF SizeOf(R)=1}{$DEFINE SIZE_OK}{$ENDIF} implementation end.";
    let mut f = Fixture::new(source, context(LayoutPlatform::Win32));
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE SIZE_OK"), Truth::True);
    assert_eq!(a.facts.reachable_states().count(), 2);
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), source.len())
        .unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    assert!(matches!(
        a.facts.layout_name(site.clone(), "R", &mut budget),
        Fact::Unknown(_)
    ));
    let operand = parse_sizeof_operand("R", &mut budget).known().unwrap();
    assert_eq!(
        a.facts.sizeof_operand(site, &operand, &mut budget),
        Fact::Known(1)
    );
}

#[test]
fn phase_b_returned_graphs_evaluate_captured_bounds_in_their_own_path() {
    let source = "unit T; interface {$IF Mystery}type R=Byte;{$ELSE}type R=Word;{$ENDIF} \
        type A=array[0..SizeOf(R)-SizeOf(R)] of Word; {$IF SizeOf(A)=2}{$DEFINE ARRAY_OK}{$ENDIF} implementation end.";
    let mut f = Fixture::new(source, context(LayoutPlatform::Win32));
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE ARRAY_OK"), Truth::True);
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), source.len())
        .unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let operand = parse_sizeof_operand("A", &mut budget).known().unwrap();
    assert_eq!(
        a.facts.sizeof_operand(site, &operand, &mut budget),
        Fact::Known(2)
    );
}

#[test]
fn phase_b_failed_fork_never_returns_a_healthy_selected_graph() {
    let mut source = String::from("unit T; interface type R=Byte;");
    for i in 0..7 {
        source.push_str(&format!("{{$IF Mystery{i}}}{{$DEFINE X{i}}}{{$ENDIF}}"));
    }
    source.push_str("implementation end.");
    let mut f = Fixture::new(&source, context(LayoutPlatform::Win32));
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    let site = a
        .facts
        .site_at(
            &f.root.id,
            OccurrenceId(0),
            source.find("{$IF Mystery0}").unwrap(),
        )
        .unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    assert!(matches!(
        a.facts.layout_name(site, "R", &mut budget),
        Fact::Incomplete(_)
    ));
}

#[test]
fn phase_b_scope_and_later_shadow_queries_use_each_paths_physical_site() {
    let source = "unit T; interface type R=Word; implementation procedure P(p:Byte); \
        {$IF Mystery}type R=Byte; const N=SizeOf(p);{$ELSE}type R=ShortInt; const N=SizeOf(R); Extra=9;{$ENDIF} \
        begin {$IF (N=1) and (SizeOf(R)=1)}{$DEFINE LOCAL_OK}{$ENDIF} end; \
        {$IF SizeOf(R)=2}{$DEFINE OUTER_OK}{$ENDIF} end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE LOCAL_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE OUTER_OK"), Truth::True);
}

#[test]
fn phase_b_elseifs_pending_tokens_and_nested_option_stacks_keep_independent_paths() {
    let source = "unit T; interface {$B+}{$H-}{$PUSHOPT} {$IF Mystery}{$B-}{$H+}{$ELSEIF false}type Bad=record Nope: ;{$ELSE}{$B-}{$H+}{$ENDIF} \
        {$PUSHOPT}{$B+}{$H-}{$POPOPT}{$IFOPT B-}{$DEFINE INNER_OK}{$ENDIF}{$POPOPT} \
        type A=By{$IF Mystery}te{$ELSE}te{$ENDIF}; const N=SizeOf(A); \
        {$IFOPT B+}{$DEFINE OUTER_OK}{$ENDIF}{$IFOPT H-}{$DEFINE H_OK}{$ENDIF}{$IF N=1}{$DEFINE TOKEN_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    for body in [
        "DEFINE INNER_OK",
        "DEFINE OUTER_OK",
        "DEFINE H_OK",
        "DEFINE TOKEN_OK",
    ] {
        assert_eq!(activity(&a, body), Truth::True, "{body}");
    }
}

#[test]
fn phase_b_unknown_saved_option_is_not_defaulted_and_subset_restore_poisoned() {
    let source = "unit T; interface {$PUSHOPT}{$B+}{$POPOPT}{$IFOPT B+}{$DEFINE SAVED_UNKNOWN}{$ENDIF} \
        {$B+}{$PUSHOPT B}{$B-}{$POPOPT}{$IFOPT B+}{$DEFINE SUBSET_UNKNOWN}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE SAVED_UNKNOWN"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE SUBSET_UNKNOWN"), Truth::Unknown);
}

#[test]
fn phase_b_initializers_capture_each_paths_scalar_environment_without_retroactive_define_changes() {
    let source = "unit T; interface {$IF Mystery}{$DEFINE FLAG}{$ELSE}{$UNDEF FLAG}{$ENDIF} \
        const N=Ord(Defined(FLAG)); {$DEFINE FLAG} \
        {$IF N=0}{$DEFINE ZERO}{$ELSE}{$DEFINE ONE}{$ENDIF} \
        {$IF Defined(FLAG)}{$DEFINE FLAG_OK}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE ZERO"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE ONE"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE FLAG_OK"), Truth::True);
    assert_eq!(
        a.facts.reachable_states().count(),
        2,
        "unknown initializers must not cause extra spurious forks"
    );
}

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
    // Scalar transport remains lossless, but the legacy callback has no shared
    // source-backed declaration continuation and must not claim completeness.
    assert!(!analysis.complete);
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
    assert!(!analysis.complete);
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
    // The unresolved early query forks without rebinding the earlier decision.
    assert_eq!(activity(&a, "DEFINE ORDER_OK"), Truth::True);
    assert_eq!(activity(&a, "DEFINE TOO_EARLY"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE LATER_OK"), Truth::True);
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
fn unknown_branches_remain_complete_without_splicing() {
    let source = "unit T; interface {$IF Mystery}type R=Byte;{$ELSE}type R=Int64;{$ENDIF} \
        {$IF SizeOf(R)=1}{$DEFINE WRONG}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert!(a.reasons.is_empty());
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
    f.deny_include("child.inc");
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
    assert_eq!(f.read_names(), vec!["child.inc"]);
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
    assert!(a.root.complete);
    assert!(a.reasons.is_empty());
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
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
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
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
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
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
}

#[test]
fn scalar_option_restore_restores_complete_option_snapshot() {
    let source = "unit T; interface {$B+}{$PUSHOPT}{$B-}{$POPOPT}{$IFOPT B+}{$DEFINE RESTORED}{$ENDIF} implementation end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE RESTORED"), Truth::True);
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
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
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
}

#[test]
fn earlier_enum_member_blocks_qualified_context_fallback_in_its_initializer() {
    let ctx = context(LayoutPlatform::Win32).with_constant(
        "System.K",
        pascal_core::conditional::ConstantValue::Integer(1),
    );
    let a = Fixture::new("unit T; interface {$Z1} type E=(System=0,e1=System.K); {$IF SizeOf(E)=1}{$DEFINE WRONG_ENUM}{$ENDIF} implementation end.", ctx).analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE WRONG_ENUM"), Truth::Unknown);
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
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
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
}
