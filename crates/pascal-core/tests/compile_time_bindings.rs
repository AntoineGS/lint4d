#[path = "support/compile_time.rs"]
mod support;
use pascal_core::NoCancellation;
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::*;

#[test]
fn rf_shadowed_intrinsic() {
    let h = Harness::plain(
        "unit T; interface type E = Extended; procedure P; implementation procedure P; type Extended = Byte; var x: Extended; begin {LOCAL} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.size("E"), Some(10));
    assert_eq!(h.size_at("LOCAL", "Extended"), Some(1));
    assert_eq!(h.size("Extended"), Some(10));
}
#[test]
fn alias_chains_and_pointer_recursion_keep_identity() {
    let h = Harness::plain(
        "unit T; interface type A=LongWord; B=type A; P=^Node; Node=record Next:P; end; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("B"), Some(4));
    assert_eq!(h.size("P"), Some(8));
    assert_eq!(h.size("Node"), None);
}
#[test]
fn by_value_cycles_and_forward_aliases_are_not_repaired_later() {
    let h = Harness::plain(
        "unit T; interface type A=A; B=C; {BEFORE} C=Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.size("A"), None);
    assert_eq!(h.size_at("BEFORE", "C"), None);
    assert_eq!(h.size("B"), None);
    assert_eq!(h.size("C"), Some(1));
}
#[test]
fn adjacent_fragments_continue_a_token() {
    let mut h = Harness::stream(
        "unit T; interface type A = ",
        context(LayoutPlatform::Win64),
    );
    h.feed("Long");
    assert_eq!(h.size("A"), None);
    h.feed("Word;");
    assert_eq!(h.size("A"), Some(4));
    h.feed(" implementation end.");
    h.finish();
    assert_eq!(h.size("A"), Some(4));
}
#[test]
fn comments_strings_and_hidden_locals_do_not_leak_bindings() {
    let h = Harness::plain(
        "unit T; interface type E=Extended; implementation procedure P; type Hidden=Byte; var Extended: Byte; begin {type end} (*end type*) writeln('type end'); {PLOCAL} end; procedure Q; begin {QLOCAL} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.size_at("PLOCAL", "Extended"), None);
    assert_eq!(h.size_at("QLOCAL", "Hidden"), None);
    assert_eq!(h.size_at("QLOCAL", "Extended"), Some(10));
    assert_eq!(h.size("E"), Some(10));
}
#[test]
fn missing_recovered_and_qualified_types_are_not_intrinsic_facts() {
    let h = Harness::plain(
        "unit T; interface type Bad=; Fake=Other.Byte; A=array[0..2] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Bad"), None);
    assert_eq!(h.size("Fake"), None);
    assert_eq!(h.size("A"), None);
}

#[test]
fn nested_scopes_parameters_and_grouped_variables_block_intrinsics() {
    let h = Harness::plain(
        "unit T; interface type E=Extended; implementation procedure P(Extended: Byte); type Local=Word; procedure Q; type Local=LongWord; var Byte, Integer: Word; begin {INNER} end; begin {OUTER} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.size_at("INNER", "Local"), Some(4));
    assert_eq!(h.size_at("INNER", "Extended"), None);
    assert_eq!(h.size_at("INNER", "Byte"), None);
    assert_eq!(h.size_at("INNER", "Integer"), None);
    assert_eq!(h.size_at("OUTER", "Local"), Some(2));
    assert_eq!(h.size("Local"), None);
    assert_eq!(h.size("E"), Some(10));
}

#[test]
fn comments_and_escaped_strings_continue_across_fragments() {
    let mut h = Harness::stream(
        "unit T; interface type A=Long(",
        context(LayoutPlatform::Win64),
    );
    h.feed("* type end *");
    h.feed(")Word;");
    // A comment separates tokens; unlike truly adjacent Long/Word chunks this
    // is not a valid LongWord alias and must not synthesize an intrinsic.
    assert_eq!(h.size("A"), None);
    h.feed(" implementation procedure P; type B=Byte; begin writeln('type '");
    h.feed("' end'); {HERE} end; end.");
    h.finish();
    assert_eq!(h.size_at("HERE", "B"), Some(1));
}

#[test]
fn include_fragments_preserve_physical_occurrences_and_declared_identity() {
    let root = loaded("T.pas", "");
    let child = loaded("type.inc", "Byte");
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 0,
                text: "unit T; interface type A=",
            },
            Truth::True,
            &mut budget,
        )
        .unwrap();
    let pending_hash = state.fingerprint();
    state
        .feed(
            SourceFragment {
                source: &child.id,
                occurrence: OccurrenceId(1),
                physical_start: 0,
                text: "Byte",
            },
            Truth::True,
            &mut budget,
        )
        .unwrap();
    assert_ne!(pending_hash, state.fingerprint());
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 24,
                text: "; B=A; implementation end.",
            },
            Truth::True,
            &mut budget,
        )
        .unwrap();
    state.finish_root(&mut budget).unwrap();
    let site = state.site_at(&root.id, OccurrenceId(0), 49).unwrap();
    let Fact::Known(a) = state.layout_name(site.clone(), "A", &mut budget) else {
        panic!("A must be known");
    };
    let Fact::Known(b) = state.layout_name(site, "B", &mut budget) else {
        panic!("B must be known");
    };
    assert_eq!(a.storage.size, 1);
    assert_eq!(a.storage, b.storage);
    assert_ne!(a.identity, b.identity);
    assert!(
        matches!(a.identity, TypeIdentity::Declared(ref key) if key.anchor.source == root.id && key.anchor.occurrence == OccurrenceId(0))
    );
    assert!(state.site_at(&child.id, OccurrenceId(1), 1).is_some());
    assert!(state.site_at(&child.id, OccurrenceId(2), 1).is_none());
    let mut payload = 0;
    state
        .visit_recovery_payload(&mut |n| {
            payload += n;
            Ok(())
        })
        .unwrap();
    assert!(payload > 0);
    assert!(
        state
            .visit_recovery_payload(&mut |_| Err("refused".into()))
            .is_err()
    );
}

#[test]
fn repeated_include_occurrences_in_different_scopes_are_distinct() {
    let root = loaded("T.pas", "");
    let child = loaded("local.inc", "type Local=Byte;");
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win32), &root, &mut budget).unwrap();
    for (source, occurrence, physical_start, text) in [
        (
            &root.id,
            0,
            0,
            "unit T; interface implementation procedure P; ",
        ),
        (&child.id, 1, 0, "type Local=Byte; "),
        (&root.id, 0, 45, "begin end; procedure Q; "),
        (&child.id, 2, 0, "type Local=Byte; "),
        (&root.id, 0, 69, "begin end; end."),
    ] {
        state
            .feed(
                SourceFragment {
                    source,
                    occurrence: OccurrenceId(occurrence),
                    physical_start,
                    text,
                },
                Truth::True,
                &mut budget,
            )
            .unwrap();
    }
    state.finish_root(&mut budget).unwrap();
    let first = state.site_at(&child.id, OccurrenceId(1), 17).unwrap();
    let second = state.site_at(&child.id, OccurrenceId(2), 17).unwrap();
    assert_ne!(first.scope, second.scope);
    let a = state
        .layout_name(first, "Local", &mut budget)
        .known()
        .unwrap();
    let b = state
        .layout_name(second, "Local", &mut budget)
        .known()
        .unwrap();
    assert_ne!(a.identity, b.identity);
}

#[test]
fn recovered_intrinsic_shadow_and_unterminated_declaration_never_fall_back() {
    let h = Harness::plain(
        "unit T; interface type Byte=; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Byte"), None);
    let mut h = Harness::stream(
        "unit T; interface type Byte=",
        context(LayoutPlatform::Win64),
    );
    h.finish();
    assert_eq!(h.size("Byte"), None);
}

#[test]
fn unsupported_reference_forms_do_not_stall_following_scalar_declarations() {
    let h = Harness::plain(
        "unit T; interface type C=class; M=class of C; E=Extended; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("C"), None);
    assert_eq!(h.size("M"), None);
    assert_eq!(h.size("E"), Some(8));
}

#[test]
fn unsupported_forward_composites_and_modifiers_retain_following_aliases() {
    let h = Harness::plain(
        "unit T; interface type C=class abstract; I=interface; O=object; E=Extended; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("C"), None);
    assert_eq!(h.size("I"), None);
    assert_eq!(h.size("O"), None);
    assert_eq!(h.size("E"), Some(8));
}

#[test]
fn budget_limits_and_unknown_activity_prevent_known_queries() {
    let root = loaded("T.pas", "");
    for (limits, text, expected) in [
        (
            CompileTimeLimits {
                max_pending_bytes: 8,
                ..CompileTimeLimits::default()
            },
            "abcdefghij",
            "max_pending_bytes",
        ),
        (
            CompileTimeLimits {
                max_definitions: 0,
                ..CompileTimeLimits::default()
            },
            "type A=Byte;",
            "max_definitions",
        ),
        (
            CompileTimeLimits {
                max_scope_depth: 1,
                ..CompileTimeLimits::default()
            },
            "implementation procedure P;",
            "max_scope_depth",
        ),
        (
            CompileTimeLimits {
                max_type_depth: 1,
                ..CompileTimeLimits::default()
            },
            "type A=record B:record X:Byte; end; end;",
            "max_type_depth",
        ),
    ] {
        let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
        let mut state =
            CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
        assert!(
            matches!(state.feed(SourceFragment { source: &root.id, occurrence: OccurrenceId(0), physical_start: 0, text }, Truth::True, &mut budget), Err(IncompleteReason::Limit { name, .. }) if name == expected)
        );
        assert!(matches!(
            state.layout_name(
                BindingSite {
                    scope: ScopeId(0),
                    sequence: Sequence(0)
                },
                "Byte",
                &mut budget
            ),
            Fact::Incomplete(_)
        ));
    }
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 0,
                text: "type Byte=Word;",
            },
            Truth::False,
            &mut budget,
        )
        .unwrap();
    let site = BindingSite {
        scope: ScopeId(0),
        sequence: Sequence(0),
    };
    assert_eq!(
        state
            .layout_name(site.clone(), "Byte", &mut budget)
            .known()
            .unwrap()
            .storage
            .size,
        1
    );
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 20,
                text: "type Byte=Word;",
            },
            Truth::Unknown,
            &mut budget,
        )
        .unwrap();
    assert_eq!(
        state.layout_name(site, "Byte", &mut budget),
        Fact::Unknown(UnknownReason::UnknownActivity)
    );
}

#[test]
fn inline_record_variables_keep_field_names_out_of_lexical_bindings() {
    let h = Harness::plain(
        "unit T; interface type E=Extended; var R:record Extended: Byte; Integer: Word; end; type A=LongWord; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("R"), None);
    assert_eq!(h.size("Extended"), Some(8));
    assert_eq!(h.size("Integer"), Some(4));
    assert_eq!(h.size("A"), Some(4));
    assert_eq!(h.size("E"), Some(8));
}

#[test]
fn finishing_a_trailing_scope_keyword_updates_the_physical_endpoint() {
    let mut h = Harness::stream(
        "unit T; interface implementation procedure P; type Local=Byte; begin end",
        context(LayoutPlatform::Win64),
    );
    h.finish();
    assert_eq!(h.size("Local"), None);
    assert_eq!(h.size("Byte"), Some(1));
}

#[test]
fn unmatched_scope_end_does_not_promote_following_locals_to_globals() {
    let h = Harness::plain(
        "unit T; interface type E=Extended; implementation procedure P; begin end; end; type Escaped=Byte; end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Escaped"), None);
}

#[test]
fn finalization_admission_failure_is_sticky() {
    let root = loaded("T.pas", "");
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 0,
                text: "type A=Byte; end",
            },
            Truth::True,
            &mut budget,
        )
        .unwrap();
    let mut denied = CompileTimeBudget::new(
        CompileTimeLimits {
            max_work: 0,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    assert!(state.finish_root(&mut denied).is_err());
    let site = state.site_at(&root.id, OccurrenceId(0), 16).unwrap();
    assert!(matches!(
        state.layout_name(site, "A", &mut budget),
        Fact::Incomplete(_)
    ));
}

#[test]
fn query_temporaries_refund_only_their_own_admitted_payloads() {
    let h = Harness::plain(
        "unit T; interface type A=Missing; B=Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    let site = h.state.site_at(&h.root.id, OccurrenceId(0), 59).unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    budget.charge(0, 0, 100).unwrap();
    assert!(matches!(
        h.state.layout_name(site.clone(), "A", &mut budget),
        Fact::Unknown(_)
    ));
    assert_eq!(budget.snapshot().retained_bytes, 100);
    let fact = h.state.layout_name(site, "B", &mut budget);
    assert!(matches!(fact, Fact::Known(_)));
    assert!(budget.snapshot().retained_bytes > 100);
}

#[test]
fn program_routine_locals_do_not_leak_into_the_root_scope() {
    let h = Harness::plain(
        "program T; type E=Extended; procedure P; type Extended=Byte; begin {LOCAL} end; begin {ROOT} end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size_at("LOCAL", "Extended"), Some(1));
    assert_eq!(h.size_at("ROOT", "Extended"), Some(8));
    assert_eq!(h.size("E"), Some(8));
}

#[test]
fn routine_names_block_the_unqualified_intrinsic_namespace() {
    let h = Harness::plain(
        "unit T; interface procedure Extended; type E=Extended; implementation procedure Extended; begin end; end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Extended"), None);
    assert_eq!(h.size("E"), None);
}

#[test]
fn retained_constant_expressions_participate_in_recovery_fingerprints() {
    let a = Harness::plain(
        "unit T; interface const N=7; implementation end.",
        context(LayoutPlatform::Win64),
    );
    let b = Harness::plain(
        "unit T; interface const N=8; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_ne!(a.state.fingerprint(), b.state.fingerprint());
}

#[test]
fn record_and_qualified_routine_sites_have_distinct_lexical_scopes() {
    let h = Harness::plain(
        "unit T; interface type E=Extended; R=record {FIELD} Next:^R; end; implementation procedure T.P; type Local=Byte; begin {METHOD} end; end.",
        context(LayoutPlatform::Win64),
    );
    let field = h
        .state
        .site_at(
            &h.root.id,
            OccurrenceId(0),
            h.root.decoded_text.as_ref().unwrap().find("FIELD").unwrap(),
        )
        .unwrap();
    let root = h
        .state
        .site_at(
            &h.root.id,
            OccurrenceId(0),
            h.root.decoded_text.as_ref().unwrap().len(),
        )
        .unwrap();
    assert_ne!(field.scope, root.scope);
    assert_eq!(h.size("E"), Some(8));
    assert_eq!(h.size_at("METHOD", "Local"), Some(1));
    assert_eq!(h.size("Local"), None);
}

#[test]
fn pending_and_type_limits_measure_source_payload_not_synthetic_wrappers() {
    let root = loaded("T.pas", "");
    for limits in [
        CompileTimeLimits {
            max_pending_bytes: 8,
            ..CompileTimeLimits::default()
        },
        CompileTimeLimits {
            max_type_depth: 1,
            ..CompileTimeLimits::default()
        },
    ] {
        let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
        let mut state =
            CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
        state
            .feed(
                SourceFragment {
                    source: &root.id,
                    occurrence: OccurrenceId(0),
                    physical_start: 0,
                    text: "type A=Byte;",
                },
                Truth::True,
                &mut budget,
            )
            .unwrap();
        let site = state.site_at(&root.id, OccurrenceId(0), 12).unwrap();
        assert_eq!(
            state
                .layout_name(site, "A", &mut budget)
                .known()
                .unwrap()
                .storage
                .size,
            1
        );
    }
}

#[test]
fn construction_rejects_root_depth_and_refunds_unallocated_context_payload() {
    let root = loaded("T.pas", "");
    let mut denied = CompileTimeBudget::new(
        CompileTimeLimits {
            max_scope_depth: 0,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    assert!(matches!(
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut denied),
        Err(IncompleteReason::Limit {
            name: "max_scope_depth",
            ..
        })
    ));
    let mut ctx = context(LayoutPlatform::Win64);
    ctx.defines.insert("x".repeat(512), Truth::True);
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_retained_bytes: std::mem::size_of::<CompileTimeState>() + 200,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    budget.charge(0, 0, 10).unwrap();
    assert!(matches!(
        CompileTimeState::new(&ctx, &root, &mut budget),
        Err(IncompleteReason::Limit {
            name: "max_retained_bytes",
            ..
        })
    ));
    assert_eq!(budget.snapshot().retained_bytes, 10);
    assert!(budget.snapshot().work > 0);
}

#[test]
fn uncertain_queries_still_poll_the_shared_cancellation_budget() {
    struct Cancelled;
    impl pascal_core::CancellationToken for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }
    let root = loaded("T.pas", "");
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 0,
                text: "type A=Byte;",
            },
            Truth::Unknown,
            &mut budget,
        )
        .unwrap();
    let mut denied = CompileTimeBudget::new(CompileTimeLimits::default(), &Cancelled);
    assert_eq!(
        state.layout_name(
            BindingSite {
                scope: ScopeId(0),
                sequence: Sequence(0)
            },
            "Byte",
            &mut denied
        ),
        Fact::Incomplete(IncompleteReason::Cancelled)
    );
    assert_eq!(denied.snapshot(), BudgetSnapshot::default());
}
