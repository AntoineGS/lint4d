#[path = "support/compile_time.rs"]
mod support;
use pascal_core::NoCancellation;
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::{Harness, context};

#[test]
fn rf_ordinal_bounds() {
    let h = Harness::plain(
        "unit T; interface type Color=(Red,Green,Blue); A=array[-2..2,1..3] of Word; B=array[Color] of Byte; C=array of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(30));
    assert_eq!(h.size("B"), Some(3));
    assert_eq!(h.size("C"), Some(8));
}

#[test]
fn boolean_subrange_and_nested_dimensions() {
    let h = Harness::plain(
        "unit T; interface type Index=-3..1; Alias=Index; A=array[Boolean,Alias,2..4] of Word; B=array[1..2] of array[4..6] of Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.size("A"), Some(60));
    assert_eq!(h.size("B"), Some(6));
}

#[test]
fn earlier_constants_use_existing_typed_arithmetic() {
    let h = Harness::plain(
        "unit T; interface const L=-2; N=(3 shl 1) div 2; U=L+N*2; type A=array[L..U] of Word; B=array[Ord(False)..Ord(True)] of Byte; C=array[1..Length('abc')] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(14));
    assert_eq!(h.size("B"), Some(2));
    assert_eq!(h.size("C"), Some(3));
}

#[test]
fn bound_sites_survive_later_shadowing_and_forward_names_stay_unknown() {
    let h = Harness::plain(
        "unit T; interface const N=3; type A=array[1..N] of Byte; F=array[1..Later] of Byte; implementation const Later=4; procedure P; const N=9; type B=A; C=array[1..N] of Byte; begin {inside} end; end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size_at("{inside}", "B"), Some(3));
    assert_eq!(h.size_at("{inside}", "C"), Some(9));
    assert_eq!(h.size("F"), None);
}

#[test]
fn array_size_does_not_wrap() {
    let h = Harness::plain(
        "unit T; interface type Good=array[1..2] of Byte; A=array[0..9223372036854775807] of Int64; B=array[-9223372036854775807..9223372036854775807] of Byte; C=array[0..4294967295,0..4294967295] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Good"), Some(2));
    for name in ["A", "B", "C"] {
        assert_eq!(h.size(name), None, "{name}");
    }
}

#[test]
fn reversed_unknown_and_overflowing_bounds_remain_unknown() {
    let h = Harness::plain(
        "unit T; interface const Huge=9223372036854775807+1; type Good=array[3..3] of Byte; A=array[3..2] of Byte; B=array[Missing..4] of Byte; C=array[0..Huge] of Byte; R=record X:Byte; end; D=array[1..2] of R; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Good"), Some(1));
    for name in ["A", "B", "C", "D"] {
        assert_eq!(h.size(name), None, "{name}");
    }
}

#[test]
fn dynamic_reference_does_not_require_element_layout_or_host_width() {
    for (platform, width) in [(LayoutPlatform::Win32, 4), (LayoutPlatform::Win64, 8)] {
        let h = Harness::plain(
            "unit T; interface type R=record X:Byte; end; A=array of R; B=array[1..2] of A; implementation end.",
            context(platform),
        );
        assert_eq!(h.size("A"), Some(width));
        assert_eq!(h.size("B"), Some(2 * width));
    }
}

#[test]
fn open_array_parameter_blocks_type_lookup() {
    let h = Harness::plain(
        "unit T; interface type A=array of Byte; implementation procedure P(A:array of Byte); begin {inside} end; end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(8));
    assert_eq!(h.size_at("{inside}", "A"), None);
}

#[test]
fn enum_domain_is_independent_of_storage_width_and_members_bind_earlier() {
    let source = "unit T; interface type Color=(Red,Green,Blue); A=array[Color] of Byte; B=array[Red..Blue] of Word; implementation end.";
    let unknown = Harness::plain(source, context(LayoutPlatform::Win64));
    assert_eq!(unknown.size("Color"), None);
    assert_eq!(unknown.size("A"), Some(3));
    assert_eq!(unknown.size("B"), Some(6));
    let mut explicit = context(LayoutPlatform::Win64);
    explicit.layout.defaults.minimum_enum_size = Some(4);
    let explicit = Harness::plain(source, explicit);
    assert_eq!(explicit.size("Color"), Some(4));
    assert_eq!(explicit.size("A"), Some(3));
    assert_eq!(explicit.size("B"), Some(6));
}

#[test]
fn constants_with_strings_and_lexical_blockers() {
    let h = Harness::plain(
        "unit T; interface const Text='abc'; N=Length(Text); type A=array[1..N] of Byte; implementation procedure P; var N:Integer; type B=array[1..N] of Byte; C=A; begin {inside} end; end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(3));
    assert_eq!(h.size_at("{inside}", "B"), None);
    assert_eq!(h.size_at("{inside}", "C"), Some(3));
}

#[test]
fn array_descriptor_payload_fingerprint_and_shared_limits() {
    let source = "unit T; interface type E=(A,B,C); R=-2..2; X=array[R,Boolean] of Word; implementation end.";
    let root = support::loaded("T.pas", source);
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(7),
                physical_start: 0,
                text: source,
            },
            Truth::True,
            &mut budget,
        )
        .unwrap();
    state.finish_root(&mut budget).unwrap();
    let mut payload = 0;
    state
        .visit_recovery_payload(&mut |n| {
            payload += n;
            Ok(())
        })
        .unwrap();
    assert!(budget.snapshot().retained_bytes >= payload);
    let fingerprint = state.fingerprint();
    let site = state
        .site_at(&root.id, OccurrenceId(7), source.len())
        .unwrap();
    let mut limited = CompileTimeBudget::new(
        CompileTimeLimits {
            max_work: 8,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    assert!(matches!(
        state.layout_name(site.clone(), "X", &mut limited),
        Fact::Incomplete(_)
    ));
    assert_eq!(limited.snapshot().retained_bytes, 0);
    assert_eq!(state.fingerprint(), fingerprint);
    assert_eq!(
        state
            .layout_name(site, "X", &mut budget)
            .known()
            .unwrap()
            .storage,
        StorageLayout {
            size: 20,
            alignment: 2
        }
    );
    let other = Harness::plain(
        &source.replace("-2..2", "-2..3"),
        context(LayoutPlatform::Win64),
    );
    assert_ne!(fingerprint, other.state.fingerprint());

    let mut dimensions = CompileTimeBudget::new(
        CompileTimeLimits {
            max_array_dimensions: 1,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut dimensions).unwrap();
    assert_eq!(
        state.feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 0,
                text: source
            },
            Truth::True,
            &mut dimensions
        ),
        Err(IncompleteReason::Limit {
            name: "max_array_dimensions",
            maximum: 1
        })
    );
}

#[test]
fn array_identity_and_physical_bound_continuation_are_preserved() {
    let mut h = Harness::stream(
        "unit T; interface const N=3; type A=array[1..",
        context(LayoutPlatform::Win64),
    );
    h.feed("N] of Word; B=A; implementation end.");
    h.finish();
    assert_eq!(h.size("B"), Some(6));
    let site = h
        .state
        .site_at(
            &h.root.id,
            OccurrenceId(0),
            "unit T; interface const N=3; type A=array[1..N] of Word; B=A; implementation end."
                .len(),
        )
        .unwrap();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let identity = h
        .state
        .layout_name(site, "B", &mut budget)
        .known()
        .unwrap()
        .identity;
    let TypeIdentity::Declared(key) = identity else {
        panic!("declared alias identity required")
    };
    assert_eq!(key.anchor.source, h.root.id);
    assert_eq!(key.anchor.occurrence, OccurrenceId(0));
    assert_eq!(key.anchor.range, 57..58);
}

#[test]
fn bound_shift_overflow_and_unproven_typed_constants_are_not_counts() {
    let h = Harness::plain(
        "unit T; interface const Shifted=4611686018427387904 shl 2; Typed:Byte=300; type A=array[0..Shifted] of Byte; B=array[0..Typed] of Byte; Good=array[0..(2 shl 2)] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), None);
    assert_eq!(h.size("B"), None);
    assert_eq!(h.size("Good"), Some(9));
}

#[test]
fn referenced_array_abi_fixtures() {
    let fixtures: toml::Value =
        toml::from_str(include_str!("fixtures/compile_time/abi.toml")).unwrap();
    let cases = fixtures["array_fixture"].as_array().unwrap();
    assert_eq!(cases.len(), 6);
    for case in cases {
        assert_eq!(case["compiler"].as_str(), Some("37.0"));
        let mut context = context(match case["platform"].as_str().unwrap() {
            "Win32" => LayoutPlatform::Win32,
            "Win64" => LayoutPlatform::Win64,
            _ => panic!("unverified fixture target"),
        });
        if case["switches"].as_str() == Some("Z4") {
            context.layout.defaults.minimum_enum_size = Some(4);
        }
        let source = format!(
            "unit T; interface type {}; implementation end.",
            case["declaration"].as_str().unwrap()
        );
        let h = Harness::plain(&source, context);
        let site = h
            .state
            .site_at(&h.root.id, OccurrenceId(0), source.len())
            .unwrap();
        let layout = h
            .state
            .layout_name(
                site,
                case["name"].as_str().unwrap(),
                &mut CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation),
            )
            .known()
            .unwrap()
            .storage;
        assert_eq!(
            layout.size,
            u64::try_from(case["expected_size"].as_integer().unwrap()).unwrap()
        );
        assert_eq!(
            layout.alignment,
            u32::try_from(case["expected_alignment"].as_integer().unwrap()).unwrap()
        );
        let reference = fixtures["references"][case["reference"].as_str().unwrap()]
            .as_str()
            .unwrap();
        assert!(reference.contains("https://"));
    }
}

#[test]
fn incompatible_ordinal_bounds_are_not_valid_array_domains() {
    let h = Harness::plain(
        "unit T; interface type E=(Red,Blue); F=(Low,High); A=array[False..1] of Byte; B=array[Red..High] of Byte; C=array[Red+1..Blue] of Byte; Good=array[False..True] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    for name in ["A", "B", "C"] {
        assert_eq!(h.size(name), None, "{name}");
    }
    assert_eq!(h.size("Good"), Some(2));
}

#[test]
fn conflicting_explicit_bound_context_is_not_a_proven_constant() {
    let mut context = context(LayoutPlatform::Win64);
    context.constants.insert(
        "N".into(),
        pascal_core::conditional::ConstantValue::Integer(2),
    );
    context.constants.insert(
        "n".into(),
        pascal_core::conditional::ConstantValue::Integer(3),
    );
    let h = Harness::plain(
        "unit T; interface type A=array[1..N] of Byte; implementation end.",
        context,
    );
    assert_eq!(h.size("A"), None);
}

#[test]
fn explicit_enum_values_use_domain_span_and_earlier_constants() {
    let h = Harness::plain(
        "unit T; interface const Start=2; type E=(First=Start,Middle=4,Last=Middle+2); A=array[E] of Byte; Bad=(One=3,Two=2); B=array[Bad] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(5));
    assert_eq!(h.size("B"), None);
}

#[test]
fn unverified_packed_array_alignment_is_not_an_unpacked_fact() {
    let h = Harness::plain(
        "unit T; interface type A=packed array[1..2] of Word; B=array[1..2] of Word; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), None);
    assert_eq!(h.size("B"), Some(4));
}

#[test]
fn enum_members_share_the_declaration_definition_limit() {
    let source = "unit T; interface type E=(Only); implementation end.";
    let root = support::loaded("T.pas", source);
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_definitions: 1,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win64), &root, &mut budget).unwrap();
    assert_eq!(
        state.feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
                physical_start: 0,
                text: source
            },
            Truth::True,
            &mut budget
        ),
        Err(IncompleteReason::Limit {
            name: "max_definitions",
            maximum: 1
        })
    );
}
