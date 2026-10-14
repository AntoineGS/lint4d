#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::{CompileTimeLimits, Fact, verify_unit_header};
use pascal_core::{NoCancellation, ResolverError};
use pascal_project::LayoutPlatform;
use support::{Fixture, context};

fn verify(
    f: &mut Fixture,
) -> Result<Fact<pascal_core::compile_time::VerifiedUnitHeader>, ResolverError> {
    let limits = f.limits;
    verify_unit_header(
        &f.root.clone(),
        &f.context.clone(),
        f,
        limits,
        &NoCancellation,
    )
}

#[test]
fn public_header_stops_at_semicolon_under_default_limits() {
    let text = format!(
        "unit Main; {{$IF UnsupportedFunction(T)}} {{$I forbidden.inc}} {}",
        "unsupported body ".repeat(1_000_000)
    );
    let mut f = Fixture::new(&text, context(LayoutPlatform::Win32));
    let h = verify(&mut f).unwrap().known().unwrap();
    assert_eq!(h.name, "Main");
    assert_eq!(h.declaration.range, 0..10);
    assert!(f.requests.is_empty());
    assert!(f.unit_requests.is_empty());
}

#[test]
fn public_header_preheader_include_stops_without_following_reads() {
    let mut f = Fixture::new(
        "{$I head.inc} {$I forbidden-root.inc}",
        context(LayoutPlatform::Win32),
    );
    f.include(
        "head.inc",
        "{$DEFINE READY} unit Included.Name; {$I forbidden-child.inc}",
    );
    let h = verify(&mut f).unwrap().known().unwrap();
    assert_eq!(h.name, "Included.Name");
    assert_eq!(h.declaration.source.as_str(), "/fixture/head.inc");
    assert_eq!(h.declaration.occurrence.0, 1);
    assert_eq!(h.declaration.range, 16..35);
    assert_eq!(h.name_parts[0].range, 21..29);
    assert_eq!(h.name_parts[1].range, 30..34);
    assert_eq!(f.requests.len(), 1);
    assert_eq!(f.requests[0].2, "head.inc");
}

#[test]
fn public_header_unknown_pascal_and_competing_headers_fail_closed() {
    for text in [
        "{$IF UnsupportedFunction(T)} unit A; {$ELSE} unit B; {$ENDIF}",
        "{$IF UnsupportedFunction(T)} type Fake=Byte; {$ENDIF} unit Main;",
        "{$IFDEF UNKNOWN} unit Main; {$ENDIF} unit Main;",
        "uses Main; unit Main;",
        "'unit Fake;' unit Main;",
        "{unit Fake;} uses Main;",
        "//unit Fake;\n uses Main;",
        "unit Main..Name;",
    ] {
        let mut f = Fixture::new(text, context(LayoutPlatform::Win32));
        assert!(verify(&mut f).unwrap().known().is_none(), "{text}");
        assert!(f.requests.is_empty());
    }
}

#[test]
fn rejected_prefix_never_opens_a_following_include() {
    let mut f = Fixture::new(
        "uses Fake {$I forbidden.inc} ; unit Main;",
        context(LayoutPlatform::Win32),
    );
    assert!(verify(&mut f).unwrap().known().is_none());
    assert!(f.requests.is_empty());
    assert!(f.unit_requests.is_empty());
}

#[test]
fn unknown_pascal_punctuation_is_not_a_definite_name_mismatch() {
    let mut f = Fixture::new(
        "{$IFDEF UNKNOWN}( {$ENDIF} unit Main;",
        context(LayoutPlatform::Win32),
    );
    assert!(matches!(
        verify(&mut f).unwrap(),
        Fact::Unknown(pascal_core::compile_time::UnknownReason::UnknownActivity)
            | Fact::Incomplete(_)
    ));
}

#[test]
fn public_header_ignores_inactive_header_but_not_uncertain_tokens() {
    let mut f = Fixture::new(
        "{$IF False} unit Fake; {$ENDIF} {unit Fake;} (* unit Fake; *) unit Main; {$I never.inc}",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(verify(&mut f).unwrap().known().unwrap().name, "Main");
    assert!(f.requests.is_empty());
}

#[test]
fn public_header_missing_or_cyclic_required_include_is_incomplete() {
    let mut missing = Fixture::new(
        "{$I missing.inc} unit Main;",
        context(LayoutPlatform::Win32),
    );
    assert!(matches!(verify(&mut missing).unwrap(), Fact::Incomplete(_)));
    let mut cycle = Fixture::new("{$I cycle.inc} unit Main;", context(LayoutPlatform::Win32));
    cycle.include("cycle.inc", "{$I cycle.inc}");
    assert!(matches!(verify(&mut cycle).unwrap(), Fact::Incomplete(_)));
    let mut ambiguous = Fixture::new(
        "{$I ambiguous.inc} unit Main;",
        context(LayoutPlatform::Win32),
    );
    ambiguous.fail_include(
        "ambiguous.inc",
        pascal_core::Resolution::Ambiguous { candidates: vec![] },
    );
    assert!(matches!(
        verify(&mut ambiguous).unwrap(),
        Fact::Incomplete(_)
    ));
    let mut denied = Fixture::new("{$I denied.inc} unit Main;", context(LayoutPlatform::Win32));
    denied.deny_include("denied.inc");
    assert!(matches!(verify(&mut denied).unwrap(), Fact::Incomplete(_)));
    assert_eq!(denied.requests.len(), 1); // Fixture logs attempts, not payload reads.
    let mut uncertain = Fixture::new(
        "{$IFDEF UNKNOWN}{$I no-authority.inc}{$ENDIF} unit Main;",
        context(LayoutPlatform::Win32),
    );
    assert!(verify(&mut uncertain).unwrap().known().is_none());
    assert!(uncertain.requests.is_empty());
}

#[test]
fn public_firedac_header_has_body_independent_work_bound() {
    for body in [String::new(), "not Pascal ".repeat(1_000_000)] {
        let text = format!(
            "{{$IF Defined(IOS) OR Defined(ANDROID)}} {{$HPPEMIT LINKUNIT}}{{$ELSE}}{{$HPPEMIT '#pragma link unit'}}{{$ENDIF}} unit FireDAC.Phys.IB; {{$I forbidden.inc}} {body}"
        );
        let mut f = Fixture::new(&text, context(LayoutPlatform::Win32));
        f.limits.max_work = 10_000;
        f.limits.max_byte_work = 100_000;
        assert_eq!(
            verify(&mut f).unwrap().known().unwrap().name,
            "FireDAC.Phys.IB"
        );
        assert!(f.requests.is_empty());
    }
}

#[test]
fn public_header_include_and_target_changes_are_not_cached_names() {
    let mut f = Fixture::new(
        "{$I target.inc} {$IFDEF READY} unit Main; {$ELSE} unit Other; {$ENDIF}",
        context(LayoutPlatform::Win32),
    );
    f.include("target.inc", "{$DEFINE READY}");
    assert_eq!(verify(&mut f).unwrap().known().unwrap().name, "Main");
    f.include("target.inc", "{$UNDEF READY}");
    assert_eq!(verify(&mut f).unwrap().known().unwrap().name, "Other");
    let mut target = Fixture::new(
        "{$IF SizeOf(Pointer)=4} unit Win32Name; {$ELSE} unit Win64Name; {$ENDIF}",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        verify(&mut target).unwrap().known().unwrap().name,
        "Win32Name"
    );
    target.context = context(LayoutPlatform::Win64);
    assert_eq!(
        verify(&mut target).unwrap().known().unwrap().name,
        "Win64Name"
    );
}

#[test]
fn public_header_cancellation_and_work_limits_fail_closed() {
    struct Cancel;
    impl pascal_core::CancellationToken for Cancel {
        fn is_cancelled(&self) -> bool {
            true
        }
    }
    let mut f = Fixture::new("unit Main; {$I never.inc}", context(LayoutPlatform::Win32));
    assert!(matches!(
        verify_unit_header(
            &f.root.clone(),
            &f.context.clone(),
            &mut f,
            CompileTimeLimits::default(),
            &Cancel
        ),
        Err(ResolverError::Cancelled)
    ));
    f.limits.max_work = 1;
    assert!(verify(&mut f).is_err());
    assert!(f.requests.is_empty());
}

#[test]
fn firedac_preamble_does_not_hide_actual_header() {
    let text = "{$IF Defined(IOS) OR Defined(ANDROID)} {$HPPEMIT LINKUNIT}{$ELSE}{$HPPEMIT '#pragma link unit'}{$ENDIF} unit FireDAC.Phys.IB; interface implementation end.";
    let mut f = Fixture::new(text, context(LayoutPlatform::Win32));
    let a = f.analyze().unwrap();
    let h = a.header.clone().known().expect("source-backed header");
    assert_eq!(h.name.to_ascii_lowercase(), "firedac.phys.ib");
    assert_eq!(h.name_parts.len(), 3);
    for (anchor, word) in h.name_parts.iter().zip(["FireDAC", "Phys", "IB"]) {
        assert_eq!(anchor.source, f.root.id);
        assert_eq!(&text[anchor.range.clone()], word);
    }
    assert_eq!(&text[h.declaration.range.clone()], "unit FireDAC.Phys.IB;");
}

#[test]
fn unsupported_body_does_not_destroy_unconditional_header() {
    let mut f = Fixture::new(
        "unit Main; interface {$IF UnsupportedFunction(T)} type X=Byte; {$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(a.header.clone().known().unwrap().name, "Main");
    assert!(!a.root.complete || !a.root.unknown_spans.is_empty());
}

#[test]
fn proven_header_does_not_hide_unterminated_body_directive() {
    let mut f = Fixture::new(
        "unit Main; interface implementation end. {$IF True",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(a.header.clone().known().unwrap().name, "Main");
    assert!(!a.root.complete);
}

#[test]
fn prefix_include_has_physical_header_anchors() {
    let mut f = Fixture::new(
        "{$I header.inc} interface implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("header.inc", "{ unit Fake; } unit Included.Name;");
    let a = f.analyze().unwrap();
    let h = a.header.clone().known().unwrap();
    assert_eq!(h.name, "Included.Name");
    assert_eq!(h.declaration.source.as_str(), "/fixture/header.inc");
    assert_eq!(h.name_parts[0].range, 20..28);
    assert_eq!(h.name_parts[1].range, 29..33);
}

#[test]
fn public_header_preserves_utf8_bom_and_comment_coordinates() {
    let mut f = Fixture::new(
        "\u{feff}{ café unit Fake; } unit CaféIgnored;",
        context(LayoutPlatform::Win32),
    );
    // Unsupported identifier syntax must not be truncated into a valid name.
    assert!(verify(&mut f).unwrap().known().is_none());
    let text = "\u{feff}{ café unit Fake; } unit Main;";
    let mut f = Fixture::new(text, context(LayoutPlatform::Win32));
    let h = verify(&mut f).unwrap().known().unwrap();
    assert_eq!(
        h.declaration.range,
        text.find("unit Main;").unwrap()..text.len()
    );
    assert_eq!(&text[h.name_parts[0].range.clone()], "Main");
}
