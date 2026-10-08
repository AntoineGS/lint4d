#[path = "support/compile_time.rs"]
mod support;
use pascal_core::NoCancellation;
use pascal_core::compile_time::*;
use pascal_project::{ConditionalContext, ConditionalFact, LayoutPlatform};
use support::*;

fn i1_context(platform: LayoutPlatform) -> ConditionalContext {
    let mut context = context(platform);
    context.layout.defaults.record_alignment = Some(8);
    context.layout.defaults.old_type_layout = ConditionalFact::False;
    context
}

fn i1_grouped_record(platform: LayoutPlatform) {
    let h = Harness::plain(
        "unit T; interface var Integer, other: record V: Integer; end; \
        implementation begin {HERE} end.",
        i1_context(platform.clone()),
    );
    let operands = ["Integer", "other", "Integer.V", "other.V"];
    let actual: Vec<_> = operands
        .map(|operand| {
            (
                operand,
                h.query("HERE", operand).known().map(|v| v.storage.size),
            )
        })
        .into();
    let expected: Vec<_> = operands.map(|operand| (operand, Some(4))).into();
    assert_eq!(actual, expected, "{platform:?}");
}

fn i1_grouped_nested_record(platform: LayoutPlatform) {
    let h = Harness::plain(
        "unit T; interface var Integer, other: record V: Integer; \
        Nested: record W: Integer; end; Items: array[0..1] of record Z: Integer; end; end; \
        implementation begin {HERE} end.",
        i1_context(platform.clone()),
    );
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for name in ["Integer", "other"] {
        actual.push((
            name.to_owned(),
            h.query("HERE", name).known().map(|v| v.storage.size),
        ));
        expected.push((name.to_owned(), Some(16)));
        for (suffix, size) in [
            ("V", 4),
            ("Nested", 4),
            ("Nested.W", 4),
            ("Items", 8),
            ("Items[0].Z", 4),
        ] {
            let operand = format!("{name}.{suffix}");
            actual.push((
                operand.clone(),
                h.query("HERE", &operand).known().map(|v| v.storage.size),
            ));
            expected.push((operand, Some(size)));
        }
    }
    assert_eq!(actual, expected, "{platform:?}");
}

fn i1_grouped_record_parameters(platform: LayoutPlatform) {
    let h = Harness::plain(
        "unit T; interface implementation procedure Test(var Integer, other: \
        record V: Integer; Nested: record W: Integer; end; end); begin {HERE} end; end.",
        i1_context(platform.clone()),
    );
    for name in ["Integer", "other"] {
        assert_eq!(
            h.query("HERE", name).known().map(|v| v.storage.size),
            Some(8),
            "{platform:?} {name}"
        );
        for suffix in ["V", "Nested", "Nested.W"] {
            let operand = format!("{name}.{suffix}");
            assert_eq!(
                h.query("HERE", &operand).known().map(|v| v.storage.size),
                Some(4),
                "{platform:?} {operand}"
            );
        }
    }
}

#[test]
fn i1_grouped_record_win32() {
    i1_grouped_record(LayoutPlatform::Win32);
}
#[test]
fn i1_grouped_record_win64() {
    i1_grouped_record(LayoutPlatform::Win64);
}
#[test]
fn i1_grouped_nested_record_win32() {
    i1_grouped_nested_record(LayoutPlatform::Win32);
}
#[test]
fn i1_grouped_nested_record_win64() {
    i1_grouped_nested_record(LayoutPlatform::Win64);
}
#[test]
fn i1_grouped_record_parameters_win32() {
    i1_grouped_record_parameters(LayoutPlatform::Win32);
}
#[test]
fn i1_grouped_record_parameters_win64() {
    i1_grouped_record_parameters(LayoutPlatform::Win64);
}

#[test]
fn i1_capture_keeps_named_cycles_anchors_and_bounded_recovery() {
    use pascal_core::conditional::Truth;
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let cycle = Harness::plain(
            "unit T; interface type R=record Next:R; end; \
            implementation begin {HERE} end.",
            i1_context(platform.clone()),
        );
        assert_eq!(
            cycle.query("HERE", "R"),
            Fact::Unknown(UnknownReason::CyclicType)
        );
        let text = "unit T; interface var Integer,other:record V:Integer; Nested:record W:Integer; end; end; implementation begin {HERE} end.";
        let root = loaded("T.pas", text);
        // V, Nested and W are each retained once per grouped descriptor.
        for max_fields in [5, 6] {
            let mut budget = CompileTimeBudget::new(
                CompileTimeLimits {
                    max_fields,
                    ..CompileTimeLimits::default()
                },
                &NoCancellation,
            );
            let mut state =
                CompileTimeState::new(&i1_context(platform.clone()), &root, &mut budget).unwrap();
            let admitted = state.feed(
                SourceFragment {
                    source: &root.id,
                    occurrence: OccurrenceId(0),
                    physical_start: 0,
                    text,
                },
                Truth::True,
                &mut budget,
            );
            if max_fields == 5 {
                assert!(matches!(
                    admitted,
                    Err(IncompleteReason::Limit {
                        name: "max_fields",
                        ..
                    })
                ));
                continue;
            }
            admitted.unwrap();
            state.finish_root(&mut budget).unwrap();
            let mut payload = 0;
            state
                .visit_recovery_payload(&mut |n| {
                    payload += n;
                    Ok(())
                })
                .unwrap();
            assert!(budget.snapshot().retained_bytes >= payload);
            let site = state
                .site_at(&root.id, OccurrenceId(0), text.find("HERE").unwrap())
                .unwrap();
            for name in ["Integer", "other"] {
                let operand = parse_sizeof_operand(name, &mut budget).known().unwrap();
                let known = state
                    .layout_operand(site.clone(), &operand, &mut budget)
                    .known()
                    .unwrap();
                assert_eq!(known.storage.size, 8);
                let TypeIdentity::Declared(key) = known.identity else {
                    panic!("group member lost declaration identity");
                };
                assert_eq!(key.anchor.source, root.id);
                assert_eq!(key.anchor.occurrence, OccurrenceId(0));
                let start = text.find(name).unwrap();
                assert_eq!(key.anchor.range, start..start + name.len());
            }
        }
    }
}

#[test]
fn sizeof_access_uses_static_type_without_execution() {
    let h = Harness::plain(
        "unit T; interface type TRecord=packed record V:Int64; end; \
        TPtr=^TRecord; TArray=array[-1..1] of TRecord; var p:TPtr; a:TArray; i:Integer; \
        implementation procedure Test(var x:TRecord); begin {HERE} end; end.",
        context(LayoutPlatform::Win32),
    );
    for operand in ["x", "x.V", "p^", "p^.V", "a[i]", "a[i].V"] {
        let fact = h.query("HERE", operand);
        assert_eq!(
            fact.clone().known().map(|v| v.storage.size),
            Some(8),
            "{operand}: {fact:?}"
        );
    }
    assert_eq!(h.query("HERE", "p").known().unwrap().storage.size, 4);
}

#[test]
fn parameters_shadows_and_invalid_access() {
    let h = Harness::plain(
        "unit T; interface type R=record V:Int64; end; A=array[Byte] of R; \
        var a1:A; i:Integer; s:String; p:Pointer; implementation \
        procedure Test(var x,y:R; out z:Int64; const c:R; oa:array of Byte); \
        var Extended:Byte; begin {HERE} end; end.",
        context(LayoutPlatform::Win64),
    );
    for operand in ["x.V", "y.V", "z", "c.V"] {
        assert_eq!(
            h.query("HERE", operand).known().unwrap().storage.size,
            8,
            "{operand}"
        );
    }
    assert_eq!(h.query("HERE", "Extended").known().unwrap().storage.size, 1);
    for operand in [
        "",
        "x,y",
        "x.NoField",
        "a1[i,i]",
        "a1[s]",
        "p^",
        "oa",
        "x()",
        "x.V()",
        "R<Integer>",
        "System.Extended",
        "65536",
    ] {
        assert!(h.query("HERE", operand).known().is_none(), "{operand}");
    }
}

#[test]
fn selectors_keep_declaration_sites_and_anonymous_fields() {
    let h = Harness::plain(
        "unit T; interface type N=Int64; R=record V:N; Inner:record B:Byte; end; end; \
        A=array[Boolean] of R; var r1:R; a1:A; b:Boolean; implementation \
        procedure Test; type N=Byte; begin {HERE} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.query("HERE", "r1.V").known().unwrap().storage.size, 8);
    assert_eq!(
        h.query("HERE", "r1.Inner.B").known().unwrap().storage.size,
        1
    );
    assert_eq!(h.query("HERE", "a1[b].V").known().unwrap().storage.size, 8);
    assert!(h.query("HERE", "a1[1]").known().is_none());
}

#[test]
fn shared_index_expression_language_is_type_only() {
    let h = Harness::plain(
        "unit T; interface type E=(First,Last); F=(Other); A=array[E] of Int64; \
        C=array[AnsiChar] of Byte; var a1:A; e1:E; f1:F; c1:C; ch:AnsiChar; i,j:Integer; \
        ints:array[-2..2] of Byte; implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    for operand in [
        "ints[(i+j) div (i-i)]",
        "ints[i shl j]",
        "ints[$FFFF]",
        "a1[(e1)]",
        "a1[First]",
        "c1[ch]",
    ] {
        assert!(
            h.query("HERE", operand).known().is_some(),
            "{operand}: {:?}",
            h.query("HERE", operand)
        );
    }
    for operand in [
        "a1[f1]",
        "a1[Other]",
        "a1[e1+1]",
        "ints[i/j]",
        "ints[Ord(e1)]",
        "ints[Unknown()]",
        "ints[i as Integer]",
    ] {
        assert!(h.query("HERE", operand).known().is_none(), "{operand}");
    }
}

#[test]
fn variable_type_is_bound_before_its_own_name() {
    let h = Harness::plain(
        "unit T; interface var Integer:Integer; implementation \
        procedure Test(x,Byte:Byte); begin {HERE} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.query("HERE", "Integer").known().unwrap().storage.size, 4);
    assert_eq!(h.query("HERE", "x").known().unwrap().storage.size, 1);
    assert_eq!(h.query("HERE", "Byte").known().unwrap().storage.size, 1);
}

#[test]
fn current_unit_qualifier_requires_header_proof_and_ignores_local_shadow() {
    let h = Harness::plain(
        "unit T; interface var Value:Int64; implementation \
        procedure Test; var Value:Byte; begin {HERE} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.query("HERE", "Value").known().unwrap().storage.size, 1);
    assert_eq!(h.query("HERE", "T.Value").known().unwrap().storage.size, 8);
    assert!(h.query("HERE", "Missing.Value").known().is_none());
}

#[test]
fn qualified_declared_types_and_forward_pointees_have_captured_scope() {
    let h = Harness::plain(
        "unit N.T; interface type P=^R; R=record V:Int64; end; Alias=N.T.R; \
        var p1:P; a1:Alias; implementation procedure Test; type R=Byte; \
        begin {HERE} end; end.",
        context(LayoutPlatform::Win32),
    );
    for operand in ["N.T.a1.V", "a1.V", "p1^.V"] {
        assert_eq!(
            h.query("HERE", operand).known().map(|v| v.storage.size),
            Some(8),
            "{operand}: {:?}",
            h.query("HERE", operand)
        );
    }
}

#[test]
fn variant_fields_and_lexical_blockers_are_not_guesses() {
    let h = Harness::plain(
        "unit T; interface type E=(Extended,Last); R=record \
        case Boolean of False:(B:Byte); True:(V:Int64); end; \
        var r1:R; implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.query("HERE", "r1.B").known().unwrap().storage.size, 1);
    assert_eq!(h.query("HERE", "r1.V").known().unwrap().storage.size, 8);
    assert!(h.query("HERE", "Extended").known().is_none());
    let duplicate = Harness::plain(
        "unit T; interface type R=record case Boolean of \
        False:(V:Byte); True:(V:Int64); end; var r1:R; implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    assert!(duplicate.query("HERE", "r1.V").known().is_none());
}

#[test]
fn parse_and_failed_query_release_scratch_and_charge_limits() {
    for text in ["", "x,y", "a[i,,j]", "x(", "x."] {
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        assert!(parse_sizeof_operand(text, &mut budget).known().is_none());
        assert_eq!(budget.snapshot().retained_bytes, 0, "{text}");
    }
    for text in ["x".repeat(4097), format!("x{}", ".x".repeat(130))] {
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        assert!(matches!(
            parse_sizeof_operand(&text, &mut budget),
            Fact::Incomplete(_)
        ));
        assert_eq!(budget.snapshot().retained_bytes, 0);
    }
    let h = Harness::plain(
        "unit T; interface type E=(First); F=(Other); A=array[E] of Byte; \
         var x:A; j:F; implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    let site = h
        .state
        .site_at(
            &h.root.id,
            OccurrenceId(0),
            h.root.decoded_text.as_ref().unwrap().find("HERE").unwrap(),
        )
        .unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let operand = parse_sizeof_operand("x.NoField", &mut budget)
        .known()
        .unwrap();
    let baseline = budget.snapshot().retained_bytes;
    assert!(
        h.state
            .layout_operand(site.clone(), &operand, &mut budget)
            .known()
            .is_none()
    );
    assert_eq!(budget.snapshot().retained_bytes, baseline);
    let indexed = parse_sizeof_operand("x[j]", &mut budget).known().unwrap();
    let baseline = budget.snapshot().retained_bytes;
    assert!(
        h.state
            .layout_operand(site.clone(), &indexed, &mut budget)
            .known()
            .is_none()
    );
    assert_eq!(budget.snapshot().retained_bytes, baseline);
    let mut tiny = CompileTimeBudget::new(
        CompileTimeLimits {
            max_work: 0,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    assert!(matches!(
        h.state.layout_operand(site, &operand, &mut tiny),
        Fact::Incomplete(_)
    ));
    assert_eq!(tiny.snapshot().retained_bytes, 0);
}

#[test]
fn dynamic_open_array_and_incomplete_pointer_storage_stay_distinct() {
    let h = Harness::plain(
        "unit T; interface type D=array of Byte; P=^Missing; \
        var d1:D; p1:P; implementation procedure Test(const d2:D; oa:array of Byte; const untyped); \
        begin {HERE} end; end.",
        context(LayoutPlatform::Win64),
    );
    for operand in ["d1", "d2", "p1"] {
        assert_eq!(
            h.query("HERE", operand).known().unwrap().storage.size,
            8,
            "{operand}"
        );
    }
    assert_eq!(h.query("HERE", "d1[100]").known().unwrap().storage.size, 1);
    for operand in ["oa", "untyped", "p1^", "d1[True]", "d1[1,2]"] {
        assert!(h.query("HERE", operand).known().is_none(), "{operand}");
    }
}

#[test]
fn field_query_does_not_skip_invalid_admission_or_execute_properties() {
    let h = Harness::plain(
        "unit T; interface type R=record V:Byte; Bad:Missing; end; \
        C=class property Value:Integer read GetValue; end; var r1:R; c1:C; \
        implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    for operand in ["r1.V", "c1.Value", "c1.GetValue()"] {
        assert!(h.query("HERE", operand).known().is_none(), "{operand}");
    }
}

#[test]
fn routine_header_return_type_is_not_a_variable_binding() {
    let h = Harness::plain(
        "unit T; interface function GetIndex:Integer; \
        var a1:array[1..2] of Byte; implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    for operand in ["GetIndex", "GetIndex()", "a1[GetIndex]", "a1[GetIndex()]"] {
        assert!(h.query("HERE", operand).known().is_none(), "{operand}");
    }
}

#[test]
fn strong_pointer_and_qualified_pointee_use_parser_type_spans() {
    let h = Harness::plain(
        "unit T; interface type R=record V:Int64; end; \
        P=type ^R; Q=^T . R; var p1:P; q1:Q; implementation begin {HERE} end.",
        context(LayoutPlatform::Win32),
    );
    for operand in ["p1^.V", "q1^.V"] {
        assert_eq!(
            h.query("HERE", operand).known().map(|v| v.storage.size),
            Some(8),
            "{operand}"
        );
    }
}
