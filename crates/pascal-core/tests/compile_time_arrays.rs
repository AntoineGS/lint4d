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

#[test]
fn fix_i1_dynamic_elements_require_a_type_binding_not_a_stride() {
    for (platform, width) in [(LayoutPlatform::Win32, 4), (LayoutPlatform::Win64, 8)] {
        let h = Harness::plain(
            "unit T; interface type R=record X:Byte; end; Alias=R; Valid=array of Alias; MissingElement=array of Missing; implementation procedure P; var Byte:Integer; type Invalid=array of Byte; LocalValid=Valid; begin {inside} end; end.",
            context(platform),
        );
        assert_eq!(h.size("MissingElement"), None);
        assert_eq!(h.size_at("{inside}", "Invalid"), None);
        assert_eq!(h.size("Valid"), Some(width));
        assert_eq!(h.size_at("{inside}", "LocalValid"), Some(width));
        assert_eq!(h.size("R"), None);
    }
}

#[test]
fn fix_i2_enum_members_block_type_and_intrinsic_fallback() {
    let h = Harness::plain(
        "unit T; interface type E=(Byte,Other); A=array[Byte] of Word; B=array[0..1] of Byte; C=array of Byte; Valid=array[E] of Word; implementation end.",
        context(LayoutPlatform::Win64),
    );
    for name in ["Byte", "A", "B", "C"] {
        assert_eq!(h.size(name), None, "{name}");
    }
    assert_eq!(h.size("Valid"), Some(4));
}

#[test]
fn fix_i2_nearest_enum_member_shadow_keeps_earlier_element_binding() {
    let h = Harness::plain(
        "unit T; interface type Old=array[0..1] of Byte; implementation procedure P; type E=(Byte,Other); A=array[Byte] of Word; B=array[0..1] of Byte; C=Old; begin {inside} end; end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(h.size_at("{inside}", "A"), None);
    assert_eq!(h.size_at("{inside}", "B"), None);
    assert_eq!(h.size_at("{inside}", "C"), Some(2));
    assert_eq!(h.size("Byte"), Some(1));
}

#[test]
fn fix_i3_grouping_preserves_nominal_enum_bound_kind() {
    let h = Harness::plain(
        "unit T; interface type E=(Red,Blue); F=(Low,High); const Lower=(Red); Upper=((Blue)); type A=array[(Lower)..((Upper))] of Byte; Good=array[(Red)..(Blue)] of Word; Bad=array[(Red)..(High)] of Byte; Mixed=array[(Red)..1] of Byte; Arithmetic=array[(Red)+1..(Blue)] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(2));
    assert_eq!(h.size("Good"), Some(4));
    for name in ["Bad", "Mixed", "Arithmetic"] {
        assert_eq!(h.size(name), None, "{name}");
    }
}

#[test]
fn fix_i3_character_subranges_and_earlier_character_constants() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let h = Harness::plain(
            "unit T; interface const Lower='A'; Upper='Z'; type C='A'..'Z'; A=array[C] of Byte; B=array[(Lower)..((Upper))] of Word; Unicode=array['Ā'..'Ă'] of Byte; Mixed=array['A'..90] of Byte; Multi=array['AA'..'ZZ'] of Byte; Reversed=array['Z'..'A'] of Byte; Surrogate=array['😀'..'😁'] of Byte; implementation end.",
            context(platform),
        );
        assert_eq!(h.size("A"), Some(26));
        assert_eq!(h.size("B"), Some(52));
        assert_eq!(h.size("Unicode"), Some(3));
        for name in ["Mixed", "Multi", "Reversed", "Surrogate"] {
            assert_eq!(h.size(name), None, "{name}");
        }
    }
}

#[test]
fn fix_i3_character_profiles_and_code_unit_limits_are_explicit() {
    let source = "unit T; interface type A=array['A'..'Z'] of Byte; B=array['Ā'..'Ă'] of Byte; Edge=array['￿'..'￿'] of Byte; implementation end.";
    let mut legacy = context(LayoutPlatform::Win32);
    legacy.compiler_version = Some(pascal_project::CompilerVersion::new(18, 5));
    let old = Harness::plain(source, legacy);
    assert_eq!(old.size("A"), Some(26));
    assert_eq!(old.size("B"), None);
    assert_eq!(old.size("Edge"), None);
    let modern = Harness::plain(source, context(LayoutPlatform::Win64));
    assert_eq!(modern.size("B"), Some(3));
    assert_eq!(modern.size("Edge"), Some(1));
    let mut missing_target = context(LayoutPlatform::Win64);
    missing_target.layout.platform = None;
    let unverified = Harness::plain(source, missing_target);
    assert_eq!(unverified.size("A"), None);
    let mut missing_version = context(LayoutPlatform::Win64);
    missing_version.compiler_version = None;
    assert_eq!(Harness::plain(source, missing_version).size("A"), None);
}

#[test]
fn fix_i3_nominal_group_scratch_is_bounded_and_refunded_on_failure() {
    let h = Harness::plain(
        "unit T; interface type E=(Red,Blue); A=array[((Red))..((Blue))] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    let source_len = "unit T; interface type E=(Red,Blue); A=array[((Red))..((Blue))] of Byte; implementation end.".len();
    let site = h
        .state
        .site_at(&h.root.id, OccurrenceId(0), source_len)
        .unwrap();
    for work in [8, 30, 80, 120, 200] {
        let mut budget = CompileTimeBudget::new(
            CompileTimeLimits {
                max_work: work,
                ..CompileTimeLimits::default()
            },
            &NoCancellation,
        );
        let result = h.state.layout_name(site.clone(), "A", &mut budget);
        if matches!(result, Fact::Incomplete(_)) {
            assert_eq!(budget.snapshot().retained_bytes, 0, "work={work}");
        } else {
            assert_eq!(result.known().unwrap().storage.size, 2);
        }
    }
    assert_eq!(h.size("A"), Some(2));
}

#[test]
fn fix_i3_grouped_enum_constants_keep_their_definition_time_domain() {
    let h = Harness::plain(
        "unit T; interface type E=(Red,Blue); const Lower=((Red)); Upper=(Blue); implementation procedure P; type Local=(Red,Blue,Extra); A=array[(Lower)..((Upper))] of Byte; Mixed=array[(Red)..(Upper)] of Byte; begin {inside} end; end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size_at("{inside}", "A"), Some(2));
    assert_eq!(h.size_at("{inside}", "Mixed"), None);
}

#[test]
fn fix_i2_member_blockers_are_source_ordered_in_the_same_scope() {
    let h = Harness::plain(
        "unit T; interface type Before=array[0..1] of Byte; {before_enum} E=(Byte,Other); After=array[0..1] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size_at("{before_enum}", "Byte"), Some(1));
    assert_eq!(h.size("Byte"), None);
    assert_eq!(h.size("Before"), Some(2));
    assert_eq!(h.size("After"), None);
}

#[test]
fn fix_i3_grouped_members_do_not_prove_an_invalid_enum_domain() {
    let h = Harness::plain(
        "unit T; interface type E=(First=3,Last=2); A=array[(First)..(First)] of Byte; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), None);
}

#[test]
fn fix_i1_preserves_valid_nominal_reference_element_bindings() {
    let h = Harness::plain(
        "unit T; interface type C=class end; I=interface end; A=array of C; B=array of I; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), Some(8));
    assert_eq!(h.size("B"), Some(8));
    assert_eq!(h.size("C"), None);
    assert_eq!(h.size("I"), None);
}

#[test]
fn fix_i2_escaped_names_keep_the_same_lexical_binding() {
    let h = Harness::plain(
        "unit T; interface type &R=record X:Word; end; Valid=array of R; E=(&Byte=2,Other=Byte+1); Invalid=array of Byte; Index=array[Byte] of Word; Values=array[(&Byte)..(Other)] of Word; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("Valid"), Some(8));
    assert_eq!(h.size("Invalid"), None);
    assert_eq!(h.size("Index"), None);
    assert_eq!(h.size("Values"), Some(4));
}

#[test]
fn fix_i1_invalid_element_cardinality_is_not_a_valid_type_shape() {
    let h = Harness::plain(
        "unit T; interface type Huge=array[0..9223372036854775807] of Byte; A=array of Huge; implementation end.",
        context(LayoutPlatform::Win64),
    );
    assert_eq!(h.size("A"), None);
}

#[test]
fn record_element_stride_includes_nested_and_final_padding() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let mut c = context(platform);
        c.layout.defaults.record_alignment = Some(8);
        c.layout.defaults.old_type_layout = pascal_project::ConditionalFact::False;
        let h = Harness::plain(
            "unit T; interface type R=record I:LongWord; B:Byte; end; N=record B:Byte; R1:R; end; A=array[1..3] of R; B=array[-1..0] of N; P=packed record X:Byte; I:LongWord; end; PA=array[0..2] of P; implementation end.",
            c,
        );
        assert_eq!(h.size("A"), Some(24));
        assert_eq!(h.size("B"), Some(24));
        assert_eq!(h.size("PA"), Some(15));
    }
}
