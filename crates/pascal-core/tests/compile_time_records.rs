#[path = "support/compile_time.rs"]
mod support;
use pascal_core::NoCancellation;
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_project::{ConditionalFact, LayoutPlatform};
use support::{Harness, context};

fn records(text: &str, platform: LayoutPlatform) -> Harness {
    let mut c = context(platform);
    c.layout.defaults.record_alignment = Some(8);
    c.layout.defaults.old_type_layout = ConditionalFact::False;
    Harness::plain(
        &format!("unit T; interface type {text} implementation end."),
        c,
    )
}

#[test]
fn ordinary_packed_grouped_and_nested() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        let h = records(
            "R=record B:Byte; I:LongWord; end; P=packed record B:Byte; I:LongWord; end; G=packed record X,Y:Byte; Z:LongWord; end; N=record B:Byte; R1:R; C:Byte; end;",
            platform,
        );
        assert_eq!(h.size("R"), Some(8));
        assert_eq!(h.size("P"), Some(5));
        assert_eq!(h.size("G"), Some(6));
        assert_eq!(h.size("N"), Some(16));
    }
}

#[test]
fn named_unnamed_and_nested_variants() {
    let h = records(
        "V=packed record case Tag:Byte of 0:(A:Word);1:(B:Int64); end; U=packed record case Byte of 0:(A:Word);1:(B:Int64); end; N=packed record B:Byte; case Byte of 0:(A:Word; case T:Byte of 0:(C:Byte);1:(D:Int64));1:(E:LongWord); end; O=record B:Byte; case Tag:Byte of 0:(A:Word);1:(C:Int64); end;",
        LayoutPlatform::Win32,
    );
    assert_eq!(h.size("V"), Some(9));
    assert_eq!(h.size("U"), Some(8));
    assert_eq!(h.size("N"), Some(12));
    assert_eq!(h.size("O"), Some(16));
}

#[test]
fn invalid_fields_are_not_skipped() {
    for body in [
        "B:Byte; X:Missing;",
        "B:Byte; b:LongWord;",
        "B:Byte; X:;",
        "case Byte of 0:(A:Byte);1:(a:Int64);",
    ] {
        assert_eq!(
            records(&format!("R=record {body} end;"), LayoutPlatform::Win32).size("R"),
            None
        );
    }
}

#[test]
fn padded_record_array_stride() {
    let h = records(
        "R=record I:LongWord; B:Byte; end; A=array[1..3] of R; N=record B:Byte; R1:R; end; NA=array[0..1] of N;",
        LayoutPlatform::Win32,
    );
    assert_eq!(h.size("A"), Some(24));
    assert_eq!(h.size("NA"), Some(24));
}

#[test]
fn rf_declaration_settings() {
    let mut c = context(LayoutPlatform::Win32);
    c.layout.defaults.old_type_layout = ConditionalFact::False;
    let mut h = Harness::stream("unit T; interface ", c);
    h.with_layout("A1");
    h.feed("type Early=record B:Byte; I:LongWord; end; S=String; E=(e0,e1); {early_settings} ");
    h.with_layout("ALIGN 8");
    h.with_layout("H-");
    h.with_layout("Z4");
    h.feed("type Late=record B:Byte; I:LongWord; end; Alias=Early; S2=String; E2=(f0,f1); ");
    h.feed("implementation end.");
    h.finish();
    assert_eq!(h.size("Early"), Some(5));
    assert_eq!(h.size("Late"), Some(8));
    assert_eq!(h.size("Alias"), Some(5));
    assert_eq!(h.size("S"), Some(4));
    assert_eq!(h.size("S2"), Some(256));
    assert_eq!(h.size_at("{early_settings}", "String"), Some(4));
    assert_eq!(h.size("String"), Some(256));
    assert_eq!(h.size("E"), None);
    assert_eq!(h.size("E2"), Some(4));
}

#[test]
fn advanced_record_noninstance_members() {
    let h = records(
        "R=record B:Byte; class var Shared:Int64; procedure Run; property Value:Byte read B; end;",
        LayoutPlatform::Win32,
    );
    assert_eq!(h.size("R"), Some(1));
}

#[test]
fn uncertainty_affects_only_dependent_layouts() {
    let mut c = context(LayoutPlatform::Win32);
    c.layout.defaults.record_alignment = Some(8);
    let h = Harness::plain(
        "unit T; interface type R=record B:Byte; I:LongWord; end; G=record A,B:LongWord; end; P=packed record A,B:LongWord; end; S=String; implementation end.",
        c,
    );
    assert_eq!(h.size("R"), Some(8));
    assert_eq!(h.size("G"), None);
    assert_eq!(h.size("P"), Some(8));
    assert_eq!(h.size("S"), Some(4));
    assert_eq!(
        records(
            "R=record {$IFDEF X} X:Int64; {$ENDIF} B:Byte; end;",
            LayoutPlatform::Win32
        )
        .size("R"),
        None
    );
}

#[test]
fn supported_save_restore_does_not_restore_ineligible_h() {
    let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
    h.with_layout("A1");
    h.with_layout("PUSHOPT");
    h.with_layout("A8");
    h.with_layout("H-");
    h.feed("type R=record B:Byte; I:LongWord; end; ");
    h.with_layout("POPOPT");
    h.feed("type P=record B:Byte; I:LongWord; end; S=String; ");
    h.with_layout("POPOPT");
    h.feed("type U=record B:Byte; I:LongWord; end; B=Byte; ");
    h.feed("implementation end.");
    h.finish();
    assert_eq!(h.size("R"), Some(8));
    assert_eq!(h.size("P"), Some(5));
    assert_eq!(h.size("S"), Some(256));
    assert_eq!(h.size("U"), None);
    assert_eq!(h.size("B"), Some(1));
}

#[test]
fn definition_sites_freeze_field_bindings_and_inline_settings() {
    let mut c = context(LayoutPlatform::Win32);
    c.layout.defaults.record_alignment = Some(8);
    let mut h = Harness::stream(
        "unit T; interface type T=Word; R=record X:T; end; Inner=record B:Byte; I:LongWord; end; N=record X:Inner; end; Mix=packed record First:String; ",
        c,
    );
    h.with_layout("H-");
    h.feed("Second:String; end; T=Int64; implementation end.");
    h.finish();
    assert_eq!(h.size("R"), Some(2));
    assert_eq!(h.size("N"), Some(8));
    assert_eq!(h.size("Mix"), Some(260));
}

#[test]
fn malformed_unknown_and_unsupported_restores_never_fabricate_settings() {
    for directive in ["POPOPT junk", "POP", "POPOPT", "A16", "ALIGN invalid"] {
        let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
        h.with_layout("A8");
        h.with_layout(directive);
        h.feed("type R=record B:Byte; I:LongWord; end; P=packed record B:Byte; I:LongWord; end; implementation end.");
        h.finish();
        assert_eq!(h.size("R"), None, "{directive}");
        assert_eq!(h.size("P"), Some(5));
    }
    let mut c = context(LayoutPlatform::Win32);
    c.compiler_version = Some(pascal_project::CompilerVersion::new(36, 0));
    let mut h = Harness::stream("unit T; interface ", c);
    h.with_layout("A8");
    h.with_layout("PUSHOPT");
    h.with_layout("A1");
    h.with_layout("POPOPT");
    h.feed("type R=record B:Byte; I:LongWord; end; implementation end.");
    h.finish();
    assert_eq!(h.size("R"), None);
}

#[test]
fn uncertain_directives_and_fields_are_conservative_state_feeds() {
    let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
    h.with_layout("A8");
    h.with_layout_activity("H-", Truth::Unknown);
    h.feed("type R=record B:Byte; I:LongWord; end; S=record Text:String; end; implementation end.");
    h.finish();
    assert_eq!(h.size("R"), Some(8));
    assert_eq!(h.size("S"), None);
    let mut h = Harness::stream(
        "unit T; interface type R=record B:Byte; ",
        context(LayoutPlatform::Win32),
    );
    h.feed_activity("X:Int64;", Truth::Unknown);
    h.feed("end; implementation end.");
    h.finish();
    assert_eq!(h.size("R"), None);
}

#[test]
fn graph_payload_and_dimension_limits_cover_records_and_settings() {
    let source = "unit T; interface type R=record A,B:Word; case T:Byte of 0:(C:Byte);1:(D:Int64); end; implementation end.";
    let root = support::loaded("R.pas", source);
    let mut c = context(LayoutPlatform::Win32);
    c.layout.defaults.record_alignment = Some(8);
    c.layout.defaults.old_type_layout = ConditionalFact::False;
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let mut state = CompileTimeState::new(&c, &root, &mut budget).unwrap();
    state
        .apply_layout_directive("PUSHOPT", Truth::True, &mut budget)
        .unwrap();
    state
        .feed(
            SourceFragment {
                source: &root.id,
                occurrence: OccurrenceId(0),
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
    let other = records(
        "R=record A,B:Word; case T:Byte of 0:(C:Byte);2:(D:Int64); end;",
        LayoutPlatform::Win32,
    );
    assert_ne!(state.fingerprint(), other.state.fingerprint());
    for limits in [
        CompileTimeLimits {
            max_fields: 1,
            ..CompileTimeLimits::default()
        },
        CompileTimeLimits {
            max_variant_alternatives: 1,
            ..CompileTimeLimits::default()
        },
    ] {
        let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
        let mut state = CompileTimeState::new(&c, &root, &mut budget).unwrap();
        assert!(matches!(
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
            Err(IncompleteReason::Limit { .. })
        ));
    }
}

#[test]
fn invalid_variant_discriminators_do_not_prove_storage() {
    for ty in ["Missing", "String", "Double"] {
        assert_eq!(
            records(
                &format!("R=packed record case {ty} of 0:(B:Byte); end;"),
                LayoutPlatform::Win32
            )
            .size("R"),
            None
        );
    }
}

#[test]
fn unverified_alignment_changes_inside_a_record_are_not_fabricated() {
    let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
    h.with_layout("A1");
    h.feed("type R=record B:Byte; ");
    h.with_layout("A8");
    h.feed("I:LongWord; end; implementation end.");
    h.finish();
    assert_eq!(h.size("R"), None);
}

#[test]
fn uncertain_restore_cannot_expose_a_false_older_snapshot() {
    let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
    h.with_layout("A1");
    h.with_layout("PUSHOPT");
    h.with_layout("A8");
    h.with_layout("PUSHOPT");
    h.with_layout_activity("POPOPT", Truth::Unknown);
    h.with_layout("POPOPT");
    h.feed("type R=record B:Byte; I:LongWord; end; implementation end.");
    h.finish();
    assert_eq!(h.size("R"), None);
}

#[test]
fn referenced_record_abi_fixtures() {
    let fixtures: toml::Value =
        toml::from_str(include_str!("fixtures/compile_time/abi.toml")).unwrap();
    for case in fixtures["record_fixture"].as_array().unwrap() {
        assert_eq!(case["compiler"].as_str(), Some("37.0"));
        assert_eq!(case["switches"].as_str(), Some("A8 OLDTYPELAYOUT OFF"));
        let platform = match case["platform"].as_str().unwrap() {
            "Win32" => LayoutPlatform::Win32,
            "Win64" => LayoutPlatform::Win64,
            _ => panic!("unverified target"),
        };
        let h = records(
            &format!("{};", case["declaration"].as_str().unwrap()),
            platform,
        );
        let source = format!(
            "unit T; interface type {}; implementation end.",
            case["declaration"].as_str().unwrap()
        );
        let site = h
            .state
            .site_at(&h.root.id, OccurrenceId(0), source.len())
            .unwrap();
        let storage = h
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
            storage.size,
            u64::try_from(case["expected_size"].as_integer().unwrap()).unwrap()
        );
        assert_eq!(
            storage.alignment,
            u32::try_from(case["expected_alignment"].as_integer().unwrap()).unwrap()
        );
        assert!(
            fixtures["references"][case["reference"].as_str().unwrap()]
                .as_str()
                .unwrap()
                .contains("https://")
        );
    }
}

#[test]
fn record_arithmetic_never_wraps() {
    let h = records(
        "Huge=array[1..9223372036854775807] of Word; Add=packed record X:Huge; B:LongWord; end; Pad=record X:Huge; B:Byte; end; Union=record B:Byte; case Byte of 0:(X:Huge);1:(Y:Word); end;",
        LayoutPlatform::Win32,
    );
    for name in ["Add", "Pad", "Union"] {
        assert_eq!(h.size(name), None, "{name}");
    }
}

#[test]
fn switch_uncertainty_is_dependency_scoped_and_saved_types_are_stable() {
    let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
    h.with_layout("A8");
    h.with_layout("Z1");
    h.feed("type E=(e0,e1); Early=record B:E; R:Real; end; ");
    h.with_layout_activity("Z4", Truth::Unknown);
    h.with_layout_activity("REALCOMPATIBILITY ON", Truth::Unknown);
    h.with_layout_activity("OLDTYPELAYOUT OFF", Truth::Unknown);
    h.feed("type F=(f0,f1); Scalar=record I:LongWord; end; OldEnum=record X:E; end; NewEnum=record X:F; end; Reals=record R:Real; end; Grouped=record A,B:LongWord; end; implementation end.");
    h.finish();
    assert_eq!(h.size("Early"), Some(16));
    assert_eq!(h.size("Scalar"), Some(4));
    assert_eq!(h.size("OldEnum"), Some(1));
    assert_eq!(h.size("NewEnum"), None);
    assert_eq!(h.size("Reals"), None);
    assert_eq!(h.size("Grouped"), None);
}

#[test]
fn missing_or_unsupported_profiles_do_not_use_host_record_layouts() {
    for platform in [None, Some(LayoutPlatform::Other("Linux64".into()))] {
        let mut c = context(LayoutPlatform::Win32);
        c.layout.platform = platform;
        assert_eq!(
            Harness::plain(
                "unit T; interface type R=packed record B:Byte; end; implementation end.",
                c
            )
            .size("R"),
            None
        );
    }
}

#[test]
fn settings_stack_admission_is_bounded_and_failure_sticky() {
    let root = support::loaded("T.pas", "unit T; interface ");
    let mut budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_scope_depth: 1,
            ..CompileTimeLimits::default()
        },
        &NoCancellation,
    );
    let mut state =
        CompileTimeState::new(&context(LayoutPlatform::Win32), &root, &mut budget).unwrap();
    state
        .apply_layout_directive("PUSHOPT", Truth::True, &mut budget)
        .unwrap();
    let reason = state
        .apply_layout_directive("PUSHOPT", Truth::True, &mut budget)
        .unwrap_err();
    assert_eq!(
        reason,
        IncompleteReason::Limit {
            name: "max_scope_depth",
            maximum: 1
        }
    );
    assert_eq!(
        state.apply_layout_directive("POPOPT", Truth::True, &mut budget),
        Err(reason)
    );
}

#[test]
fn undocumented_boolean_spellings_do_not_publish_supported_facts() {
    for (body, ty) in [
        ("REALCOMPATIBILITY -", "Real"),
        ("REALCOMPATIBILITY +", "Real"),
        ("LONGSTRINGS +", "String"),
        ("H ON", "String"),
    ] {
        let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
        h.with_layout(body);
        h.feed(&format!("type R={ty}; implementation end."));
        h.finish();
        assert_eq!(h.size("R"), None, "{body}");
    }
}

#[test]
fn packing_caps_do_not_rewrite_scalar_inherent_alignment() {
    for (cap, expected) in [(1, 5), (2, 6), (4, 8), (8, 8)] {
        let mut h = Harness::stream("unit T; interface ", context(LayoutPlatform::Win32));
        h.with_layout(&format!("ALIGN {cap}"));
        let source = "type R=record B:Byte; I:LongWord; end; implementation end.";
        h.feed(source);
        h.finish();
        assert_eq!(h.size("R"), Some(expected));
        let site = h
            .state
            .site_at(
                &h.root.id,
                OccurrenceId(0),
                "unit T; interface ".len() + source.len(),
            )
            .unwrap();
        let storage = h
            .state
            .layout_name(
                site,
                "LongWord",
                &mut CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation),
            )
            .known()
            .unwrap()
            .storage;
        assert_eq!(
            storage,
            StorageLayout {
                size: 4,
                alignment: 4
            }
        );
    }
}
