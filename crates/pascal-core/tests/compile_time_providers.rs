#[path = "support/compile_time.rs"]
mod support;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

fn edit_context<R>(
    context: &mut std::sync::Arc<pascal_project::ProjectContext>,
    edit: impl FnOnce(&mut pascal_project::ProjectContextParts) -> R,
) {
    let mut parts = std::sync::Arc::unwrap_or_clone(std::mem::take(context)).into_parts();
    edit(&mut parts);
    *context = pascal_project::ProjectContext::from_parts(parts).into();
}

fn assert_qualified_export_fact(
    fixture: &Fixture,
    analysis: &pascal_core::compile_time::CompileTimeAnalysis,
    marker: &str,
    operand: &str,
    provider: &str,
    provider_text: &str,
) -> pascal_core::compile_time::KnownLayout {
    use pascal_core::compile_time::{
        CompileTimeBudget, CompileTimeLimits, OccurrenceId, TypeIdentity,
    };
    let text = fixture.root.decoded_text.as_ref().unwrap();
    assert_eq!(text.matches(marker).count(), 1);
    let offset = text.find(marker).unwrap();
    let site = analysis
        .facts
        .site_at(&fixture.root.id, OccurrenceId(0), offset)
        .unwrap();
    let mut budget =
        CompileTimeBudget::new(CompileTimeLimits::default(), &pascal_core::NoCancellation);
    let fact = analysis.facts.layout_name(site, operand, &mut budget);
    let pascal_core::compile_time::Fact::Known(layout) = fact else {
        panic!("qualified operand {operand} at physical offset {offset}: {fact:?}");
    };
    assert_eq!(layout.storage.size, 8);
    let TypeIdentity::Declared(key) = &layout.identity else {
        panic!(
            "expected imported declaration identity: {:?}",
            layout.identity
        );
    };
    assert_eq!(
        key.anchor.source,
        pascal_core::SourceId::new(format!("/fixture/{provider}.pas"))
    );
    assert_eq!(provider_text.get(key.anchor.range.clone()), Some("Export"));
    layout
}

#[test]
fn provider_root_identity_is_shared_only_for_the_same_authorized_capability() {
    use pascal_core::compile_time::*;
    use pascal_core::*;
    struct ChangingProvider {
        fixture: Fixture,
        mode: &'static str,
        calls: usize,
    }
    const PROVIDER: &str =
        "unit Types; interface type Export=Int64; const Value=8; implementation end.";
    impl CompileTimeResolver for ChangingProvider {
        fn unit_report_capability(
            &mut self,
            request: UnitResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn CancellationToken,
        ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError> {
            assert_eq!(request.requested_name, "Types");
            if self.calls != 0 {
                match self.mode {
                    "revision" => self.fixture.unit_revision("Types", 2),
                    "context" => {
                        self.fixture
                            .unit("Types", PROVIDER, context(LayoutPlatform::Win64))
                    }
                    "qualifiers" => self
                        .fixture
                        .unit_qualifiers("Types", &["Types", "Alternate"]),
                    "same" => (),
                    _ => unreachable!(),
                }
            }
            self.calls += 1;
            self.fixture.unit_report_capability(request, budget, cancel)
        }
        fn include(
            &mut self,
            request: IncludeResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn CancellationToken,
        ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
            CompileTimeResolver::include(&mut self.fixture, request, budget, cancel)
        }
        fn unit(
            &mut self,
            request: UnitResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn CancellationToken,
        ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
            CompileTimeResolver::unit(&mut self.fixture, request, budget, cancel)
        }
    }
    for mode in ["same", "revision", "context", "qualifiers"] {
        let mut fixture = Fixture::new(
            "unit Main; interface uses Types; {$IFDEF OPEN} uses Other; {$ENDIF} {layout-site} {$IF Types.Value=8}{$DEFINE CONSTANT_HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        fixture.unit("Types", PROVIDER, context(LayoutPlatform::Win32));
        let root = fixture.root.clone();
        let cx = fixture.context.clone();
        let mut resolver = ChangingProvider {
            fixture,
            mode,
            calls: 0,
        };
        let a = analyze_source(
            &root,
            &cx,
            &mut resolver,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
        .unwrap();
        assert_eq!(resolver.calls, 2);
        assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
        if mode == "same" {
            assert_qualified_export_fact(
                &resolver.fixture,
                &a,
                "{layout-site}",
                "Types.Export",
                "Types",
                PROVIDER,
            );
        } else {
            let site = a
                .facts
                .site_at(
                    &root.id,
                    OccurrenceId(0),
                    root.decoded_text
                        .as_ref()
                        .unwrap()
                        .find("{layout-site}")
                        .unwrap(),
                )
                .unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            assert_eq!(
                a.facts.layout_name(site, "Types.Export", &mut budget),
                Fact::Unknown(UnknownReason::AmbiguousBinding),
                "{mode}"
            );
        }
        assert!(
            !resolver
                .fixture
                .unit_requests
                .iter()
                .any(|(_, name)| name == "Other")
        );
    }
}

#[test]
fn imported_enum_array_indices_keep_nominal_identity() {
    let mut f = Fixture::new(
        "unit Main; interface uses Other; type Different=(One,Two,Three); A=array[Other.EA] of Int64; var Values:A; Right:Other.E; Wrong:Different; {$IF SizeOf(Values[Right])=8}{$DEFINE HIT}{$ENDIF} {$IF SizeOf(Values[Wrong])=8}{$DEFINE WRONG}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type E=(One,Two,Three); EA=E; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
}

#[test]
fn imported_captured_intrinsics_observe_provider_declaration_order() {
    let mut f = Fixture::new(
        "unit Main; interface uses Other; {$IF Other.Early}{$DEFINE EARLY}{$ENDIF} {$IF Other.Late}{$I chosen.inc}{$DEFINE LATE}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Other", "unit Other; interface const Early=SizeOf(Integer)=4; SizeOf=1; Late=SizeOf(Integer)=4; implementation end.", context(LayoutPlatform::Win64));
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE EARLY"), Truth::True);
    assert_eq!(activity(&a, "DEFINE LATE"), Truth::Unknown);
    assert!(!f.read_names().iter().any(|n| n == "chosen.inc"));
}

#[test]
fn imported_ordinal_domains_and_aliases_use_provider_definition_sites() {
    for name in [
        "E", "Other.E", "EA", "Other.EA", "R", "Other.R", "RA", "Other.RA",
    ] {
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface uses Other; const Lower=90; type A=array[{name}] of Byte; {{$IF SizeOf(A)=3}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit("Other", "unit Other; interface const Lower=-2; Upper=0; type E=(One,Two,Three); EA=E; R=Lower..Upper; RA=R; implementation end.", context(LayoutPlatform::Win64));
        let a = f.analyze().unwrap();
        assert_eq!(
            activity(&a, "DEFINE HIT"),
            Truth::True,
            "{name}: {:?}",
            a.reasons
        );
        assert!(f.read_names().iter().any(|n| n == "Other"));
        let mut budget = pascal_core::compile_time::CompileTimeBudget::new(
            pascal_core::compile_time::CompileTimeLimits::default(),
            &pascal_core::NoCancellation,
        );
        let site = a
            .facts
            .site_at(
                &f.root.id,
                pascal_core::compile_time::OccurrenceId(0),
                f.root
                    .decoded_text
                    .as_ref()
                    .unwrap()
                    .find("{$IF SizeOf(A)")
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            a.facts
                .layout_name(site, "A", &mut budget)
                .known()
                .map(|layout| layout.storage.size),
            Some(3)
        );
    }
}

#[test]
fn imported_ordinal_domains_preserve_shadow_ambiguity_and_unknown_controls() {
    for (setup, domain) in [
        ("uses Other, Second;", "E"),
        ("uses Other; var Other:Integer;", "Other.E"),
        ("{$IFDEF OPEN} uses Other; {$ENDIF}", "Other.E"),
        ("uses Other;", "Other.Cycle"),
    ] {
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface {setup} type A=array[{domain}] of Byte; {{$IF SizeOf(A)=3}}{{$I chosen.inc}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Other",
            "unit Other; interface type E=(One,Two,Three); Cycle=Cycle; implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Second",
            "unit Second; interface type E=(One,Two,Three); implementation end.",
            context(LayoutPlatform::Win32),
        );
        let a = f.analyze().unwrap();
        assert_eq!(
            activity(&a, "DEFINE HIT"),
            Truth::Unknown,
            "{setup} {domain}"
        );
        assert!(!f.read_names().iter().any(|n| n == "chosen.inc"));
        if setup.contains("OPEN") {
            assert!(f.unit_requests.is_empty());
        }
    }
    let mut f = Fixture::new(
        "unit Main; interface uses Other; type E=0..0; A=array[E] of Byte; {$IF SizeOf(A)=1}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type E=(One,Two,Three); implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn filesystem_resolver_imported_ordinal_arrays_are_known() {
    use pascal_core::{FilesystemSourceStore, NoCancellation, ResolverLimits, UnitResolver};
    use pascal_project::{ProjectContext, ProjectOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Main.pas");
    let text = "unit Main; interface uses Other; type A=array[Other.E] of Byte; B=array[RA] of Byte; {$IF SizeOf(A)=3}{$DEFINE ENUM_HIT}{$ENDIF} {$IF SizeOf(B)=3}{$DEFINE RANGE_HIT}{$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    std::fs::write(
        dir.path().join("Other.pas"),
        "unit Other; interface type E=(One,Two,Three); R=-2..0; RA=R; implementation end.",
    )
    .unwrap();
    let mut cx = ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
        .unwrap()
        .context;
    edit_context(&mut cx, |p| {
        p.conditional_context = context(LayoutPlatform::Win32)
    });
    let mut resolver = UnitResolver::new(
        cx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut source = support::loaded("Main.pas", text);
    source.path = path;
    let a = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE ENUM_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE RANGE_HIT"), Truth::True);
}

#[test]
fn enum_members_block_intrinsic_callees_direct_and_captured() {
    for (name, call) in [
        ("SizeOf", "SizeOf(Integer)=4"),
        ("Length", "Length('ab')=2"),
        ("Ord", "Ord(True)=1"),
        ("Declared", "Declared(CompilerVersion)"),
    ] {
        for imported in [false, true] {
            for captured in [false, true] {
                let declarations = if imported {
                    "uses Other;".to_owned()
                } else {
                    format!("type E=({name},Next);")
                };
                let query = if captured {
                    format!("const Captured={call}; {{$IF Captured}}")
                } else {
                    format!("{{$IF {call}}}")
                };
                let mut f = Fixture::new(
                    &format!(
                        "unit Main; interface {declarations} {query}{{$I chosen.inc}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
                    ),
                    context(LayoutPlatform::Win32),
                );
                f.unit(
                    "Other",
                    &format!("unit Other; interface type E=({name},Next); implementation end."),
                    context(LayoutPlatform::Win32),
                );
                f.include("chosen.inc", " ");
                let a = f.analyze().unwrap();
                assert_eq!(
                    activity(&a, "DEFINE HIT"),
                    Truth::Unknown,
                    "{name} imported={imported} captured={captured}"
                );
                assert!(
                    !a.root.complete,
                    "unknown selected include remains required"
                );
                assert_eq!(
                    f.read_names(),
                    if imported { vec!["Other"] } else { vec![] }
                );
                assert!(f.requests.is_empty());
            }
        }
    }
}

#[test]
fn imported_enum_member_blocks_define_and_context_scalar_fallback() {
    for expression in ["FLAG", "Other.FLAG"] {
        for captured in [false, true] {
            let query = if captured {
                format!("const Captured={expression}; {{$IF Captured}}")
            } else {
                format!("{{$IF {expression}}}")
            };
            let mut ctx = context(LayoutPlatform::Win32);
            ctx.constants
                .insert("FLAG".into(), pascal_project::ConstantValue::Boolean(true));
            let mut f = Fixture::new(
                &format!(
                    "unit Main; interface uses Other; {{$DEFINE FLAG}} {query}{{$I chosen.inc}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
                ),
                ctx,
            );
            f.unit(
                "Other",
                "unit Other; interface type E=(FLAG,Next); implementation end.",
                context(LayoutPlatform::Win32),
            );
            f.include("chosen.inc", " ");
            let a = f.analyze().unwrap();
            assert_eq!(
                activity(&a, "DEFINE HIT"),
                Truth::Unknown,
                "{expression} captured={captured}"
            );
            assert_eq!(f.read_names(), ["Other"]);
            assert!(f.requests.is_empty());
        }
    }
}

#[test]
fn enum_parent_export_and_import_site_order_preserve_intrinsic_absence() {
    for provider in [
        "unit Other; interface implementation type E=(SizeOf,Next); end.",
        "unit Other; interface type E=(Unrelated,Next); implementation end.",
    ] {
        let mut f = Fixture::new(
            "unit Main; interface uses Other; {$IF SizeOf(Integer)=4}{$I chosen.inc}{$DEFINE HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit("Other", provider, context(LayoutPlatform::Win32));
        f.include("chosen.inc", " ");
        let a = f.analyze().unwrap();
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
        assert!(a.root.complete);
        assert_eq!(f.read_names(), ["Other", "chosen.inc"]);
    }
    let mut f = Fixture::new(
        "unit Main; interface const Earlier=SizeOf(Integer)=4; {$IF SizeOf(Integer)=4}{$DEFINE BEFORE}{$ENDIF} uses Other; {$IF Earlier}{$I earlier.inc}{$DEFINE CAPTURED}{$ENDIF} {$IF SizeOf(Integer)=4}{$I chosen.inc}{$DEFINE AFTER}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type E=(SizeOf,Next); implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("earlier.inc", " ");
    f.include("chosen.inc", " ");
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE BEFORE"), Truth::True);
    assert_eq!(activity(&a, "DEFINE CAPTURED"), Truth::True);
    assert_eq!(activity(&a, "DEFINE AFTER"), Truth::Unknown);
    assert_eq!(f.read_names(), ["earlier.inc", "Other"]);
}

#[test]
fn imported_intrinsic_blockers_apply_to_direct_and_captured_calls() {
    for (name, call) in [
        ("SizeOf", "SizeOf(Integer)=4"),
        ("Length", "Length('ab')=2"),
        ("Ord", "Ord(True)=1"),
        ("Declared", "Declared(CompilerVersion)"),
    ] {
        for declaration in [
            format!("const {name}=1;"),
            format!("var {name}:Integer;"),
            format!("function {name}:Integer;"),
        ] {
            for captured in [false, true] {
                let query = if captured {
                    format!("const Captured={call}; {{$IF Captured}}")
                } else {
                    format!("{{$IF {call}}}")
                };
                let mut f = Fixture::new(
                    &format!(
                        "unit Main; interface uses Other; {query}{{$I selected.inc}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
                    ),
                    context(LayoutPlatform::Win32),
                );
                f.unit(
                    "Other",
                    &format!("unit Other; interface {declaration} implementation end."),
                    context(LayoutPlatform::Win32),
                );
                f.include("selected.inc", " ");
                assert_eq!(
                    activity(&f.analyze().unwrap(), "DEFINE HIT"),
                    Truth::Unknown,
                    "{declaration} captured={captured}"
                );
                assert!(f.read_names().iter().any(|n| n == "Other"));
                assert!(!f.read_names().iter().any(|n| n == "selected.inc"));
            }
        }
    }
}

#[test]
fn uncertain_import_blocks_intrinsics_without_reading_hypothetical_provider() {
    for call in [
        "SizeOf(Integer)=4",
        "Length('ab')=2",
        "Ord(True)=1",
        "Declared(CompilerVersion)",
    ] {
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface {{$IFDEF OPEN}} uses Other; {{$ENDIF}} const Captured={call}; {{$IF {call}}}{{$I direct.inc}}{{$DEFINE DIRECT}}{{$ENDIF}} {{$IF Captured}}{{$I captured.inc}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Other",
            "unit Other; interface type T=Byte; implementation end.",
            context(LayoutPlatform::Win32),
        );
        let a = f.analyze().unwrap();
        assert_eq!(activity(&a, "DEFINE DIRECT"), Truth::Unknown);
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::Unknown);
        assert!(f.unit_requests.is_empty());
        assert!(f.read_names().is_empty());
    }
}

#[test]
fn intrinsic_absence_and_source_order_controls() {
    let mut f = Fixture::new(
        "unit Main; interface {$IF SizeOf(Integer)=4}{$DEFINE BEFORE}{$ENDIF} const Earlier=SizeOf(Integer)=4; uses Other; {$IF Earlier}{$DEFINE CAPTURED_BEFORE}{$ENDIF} {$IF SizeOf(Integer)=4}{$DEFINE AFTER}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface const SizeOf=1; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE BEFORE"), Truth::True);
    assert_eq!(activity(&a, "DEFINE CAPTURED_BEFORE"), Truth::True);
    assert_eq!(activity(&a, "DEFINE AFTER"), Truth::Unknown);
    let mut f = Fixture::new(
        "unit Main; interface uses Empty; {$IF SizeOf(Integer)=4}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Empty",
        "unit Empty; interface type T=Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn unavailable_imports_do_not_fall_through_to_define_or_context() {
    for mode in ["unknown", "missing", "denied", "ambiguous", "two"] {
        for captured in [false, true] {
            for explicit_context in [false, true] {
                let imports = match mode {
                    "unknown" => "{$IFDEF OPEN} uses Other; {$ENDIF}",
                    "two" => "uses Other, Second;",
                    _ => "uses Other;",
                };
                let expression = if captured {
                    "const Captured=Value; {$IF Captured}"
                } else {
                    "{$IF Value}"
                };
                let mut cx = context(LayoutPlatform::Win32);
                if explicit_context {
                    cx.constants.insert(
                        "Value".into(),
                        pascal_core::conditional::ConstantValue::Boolean(true),
                    );
                }
                let mut f = Fixture::new(
                    &format!(
                        "unit Main; interface {imports} {{$DEFINE Value}} {expression}{{$I chosen.inc}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
                    ),
                    cx,
                );
                if mode != "missing" {
                    f.unit(
                        "Other",
                        "unit Other; interface const Value=False; implementation end.",
                        context(LayoutPlatform::Win32),
                    );
                }
                if mode == "denied" {
                    f.deny_unit("Other");
                }
                if mode == "ambiguous" {
                    f.ambiguous_unit("Other");
                }
                if mode == "two" {
                    f.unit(
                        "Second",
                        "unit Second; interface const Value=False; implementation end.",
                        context(LayoutPlatform::Win32),
                    );
                }
                f.include("chosen.inc", " ");
                let a = f.analyze().unwrap();
                assert_eq!(
                    activity(&a, "DEFINE HIT"),
                    Truth::Unknown,
                    "{mode} captured={captured} context={explicit_context}"
                );
                assert!(!f.read_names().iter().any(|n| n == "chosen.inc"));
                if mode == "unknown" {
                    assert!(f.unit_requests.is_empty());
                }
            }
        }
    }
}

#[test]
fn proven_absence_allows_fallback_but_known_false_pascal_binding_wins() {
    for imports in ["", "{$IF false} uses Other; {$ENDIF}", "uses Empty;"] {
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface {imports} {{$DEFINE Value}} const Captured=Value; {{$IF Captured}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Empty",
            "unit Empty; interface type T=Byte; implementation end.",
            context(LayoutPlatform::Win32),
        );
        assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
        assert!(!f.unit_requests.iter().any(|(_, n)| n == "Other"));
    }
    let mut f = Fixture::new(
        "unit Main; interface uses Other; {$DEFINE Value} const Captured=Value; {$IF Value}{$DEFINE DIRECT}{$ENDIF} {$IF Captured}{$DEFINE HIT}{$ENDIF} {$IF Defined(Value)}{$DEFINE EXPLICIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface const Value=False; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE DIRECT"), Truth::False);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::False);
    assert_eq!(activity(&a, "DEFINE EXPLICIT"), Truth::True);
}

#[test]
fn unknown_import_prevents_an_unqualified_layout_proof_without_provider_reads() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation {$IFDEF OPEN} uses Other; {$ENDIF} {$IF SizeOf(Export)=8}{$DEFINE HIT}{$ENDIF} end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type Export=Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::Unknown);
    assert!(f.read_names().iter().any(|name| name == "Types"));
    assert!(!f.unit_requests.iter().any(|(_, name)| name == "Other"));
    assert!(!f.read_names().iter().any(|name| name == "Other"));
}

#[test]
fn unknown_import_prevents_an_unqualified_constant_proof_without_provider_reads() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation {$IFDEF OPEN} uses Other; {$ENDIF} {$IF Value=8}{$DEFINE HIT}{$ENDIF} end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface const Value=8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface const Value=1; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::Unknown);
    assert!(f.read_names().iter().any(|name| name == "Types"));
    assert!(!f.unit_requests.iter().any(|(_, name)| name == "Other"));
    assert!(!f.read_names().iter().any(|name| name == "Other"));
}

#[test]
fn unknown_import_prevents_an_unqualified_dotted_record_operand_proof() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation {$IFDEF OPEN} uses Other; {$ENDIF} {$IF SizeOf(Value.Field[0]^)=8}{$DEFINE HIT}{$ENDIF} end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types", "unit Types; interface type P=^Int64; R=record Field:array[0..1] of P; end; var Value:R; implementation end.", context(LayoutPlatform::Win32));
    f.unit("Other", "unit Other; interface type P=^Byte; R=record Field:array[0..1] of P; end; var Value:R; implementation end.", context(LayoutPlatform::Win32));
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
    assert!(!f.unit_requests.iter().any(|(_, name)| name == "Other"));
    assert!(!f.read_names().iter().any(|name| name == "Other"));
}

#[test]
fn false_import_is_absent_for_unqualified_layout_and_constant_proofs() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation {$IF false} uses Other; {$ENDIF} {$IF SizeOf(Export)=8}{$DEFINE LAYOUT_HIT}{$ENDIF} {$IF Value=8}{$DEFINE CONSTANT_HIT}{$ENDIF} end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type Export=Byte; const Value=1; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE LAYOUT_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
    assert!(!f.unit_requests.iter().any(|(_, name)| name == "Other"));
    assert!(!f.read_names().iter().any(|name| name == "Other"));
}

#[test]
fn source_future_import_does_not_change_earlier_unqualified_proofs() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation {$IF SizeOf(Export)=8}{$DEFINE LAYOUT_HIT}{$ENDIF} {$IF Value=8}{$DEFINE CONSTANT_HIT}{$ENDIF} {$IFDEF OPEN} uses Other; {$ENDIF} end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type Export=Byte; const Value=1; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE LAYOUT_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
    assert!(!f.unit_requests.iter().any(|(_, name)| name == "Other"));
    assert!(!f.read_names().iter().any(|name| name == "Other"));
}

#[test]
fn qualified_proofs_stay_isolated_from_an_unknown_unqualified_import() {
    for selected_include in ["", "{$I hit.inc}"] {
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface uses Types; implementation {{$IFDEF OPEN}} uses Other; {{$ENDIF}} {{$IF Types.Value=8}}{{$DEFINE CONSTANT_HIT}}{{$ENDIF}} {{layout-site}} {{$IF SizeOf(Types.Export)=8}}{selected_include}{{$DEFINE LAYOUT_HIT}}{{$ENDIF}} end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Types",
            "unit Types; interface type Export=Int64; const Value=8; implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Other",
            "unit Other; interface type Export=Byte; const Value=1; implementation end.",
            context(LayoutPlatform::Win32),
        );
        let a = f.analyze().unwrap();
        if selected_include.is_empty() {
            assert_qualified_export_fact(
                &f,
                &a,
                "{layout-site}",
                "Types.Export",
                "Types",
                "unit Types; interface type Export=Int64; const Value=8; implementation end.",
            );
        } else {
            assert!(
                !a.root.complete,
                "opaque unknown include retains the existing incomplete-analysis policy"
            );
        }
        assert_eq!(activity(&a, "DEFINE LAYOUT_HIT"), Truth::Unknown);
        assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
        assert!(!f.unit_requests.iter().any(|(_, name)| name == "Other"));
        assert!(!f.read_names().iter().any(|name| name == "Other"));
        assert!(!f.read_names().iter().any(|name| name == "hit.inc"));
    }
}

/// An uncertain interface that could declare `SizeOf`. One that never
/// spells the name could not shadow the intrinsic (l4d-onnm, D46).
const UNCERTAIN_OTHER: &str = "unit Other; interface {$IFDEF OPEN} type Export=Int64; {$ELSE} type Export=UInt64; {$ENDIF} {$IFDEF SHADOW} function SizeOf(X: Byte): Byte; {$ENDIF} implementation end.";

#[test]
fn qualified_layout_keeps_its_proof_after_an_uncertain_provider_query() {
    for selected_include in ["", "{$I hit.inc}"] {
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface uses Types, Other; {{$IF SizeOf(Other.Export)=8}}{{$DEFINE OTHER_HIT}}{{$ENDIF}} {{$IF Types.Value=8}}{{$DEFINE CONSTANT_HIT}}{{$ENDIF}} {{layout-site}} {{$IF SizeOf(Types.Export)=8}}{{$DEFINE HIT}}{selected_include}{{$ENDIF}} {{$IFDEF INCLUDED}}{{$DEFINE INCLUDED_HIT}}{{$ENDIF}} implementation end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit(
        "Types",
        "unit Types; interface type Export=Int64; const Value=8; implementation {$I forbidden.inc} end.",
        context(LayoutPlatform::Win32),
    );
        f.unit("Other", UNCERTAIN_OTHER, context(LayoutPlatform::Win32));
        f.include("hit.inc", "{$DEFINE INCLUDED}");
        f.deny_include("forbidden.inc");
        let a = f.analyze().unwrap();
        if selected_include.is_empty() {
            assert_qualified_export_fact(
                &f,
                &a,
                "{layout-site}",
                "Types.Export",
                "Types",
                "unit Types; interface type Export=Int64; const Value=8; implementation {$I forbidden.inc} end.",
            );
        } else {
            assert!(
                !a.root.complete,
                "opaque unknown include retains the existing incomplete-analysis policy"
            );
        }
        assert_eq!(activity(&a, "DEFINE OTHER_HIT"), Truth::Unknown);
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::Unknown);
        assert_eq!(activity(&a, "DEFINE INCLUDED_HIT"), Truth::Unknown);
        assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
        assert!(!f.read_names().iter().any(|name| name == "hit.inc"));
        assert!(!f.read_names().iter().any(|name| name == "forbidden.inc"));
        assert!(f.read_names().iter().any(|name| name == "Types"));
        assert!(f.read_names().iter().any(|name| name == "Other"));
    }
}

#[test]
fn qualified_constants_keep_their_value_after_an_uncertain_provider_query() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types, Other; {$IF Other.Value=8}{$DEFINE OTHER_HIT}{$ENDIF} const Captured=Types.Value; {$IF Types.Value=8}{$DEFINE HIT}{$ENDIF} {$IF Captured=8}{$DEFINE CAPTURED_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface const Value=8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Other", "unit Other; interface {$IFDEF OPEN} const Value=8; {$ELSE} const Value=9; {$ENDIF} implementation end.", context(LayoutPlatform::Win32));
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE OTHER_HIT"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE CAPTURED_HIT"), Truth::True);
}

#[test]
fn authorized_alias_and_namespace_proofs_are_independent_of_provider_query_order() {
    for imports in ["Alias, Other", "Other, Alias"] {
        for other_first in [true, false] {
            for selected_include in ["", "{$I hit.inc}"] {
                let other = "{$IF SizeOf(Other.Export)=8}{$DEFINE OTHER_HIT}{$ENDIF}";
                let proven = format!(
                    "{{$IF Vendor.Types.Value=8}}{{$DEFINE NAMESPACE_HIT}}{{$ENDIF}} {{layout-site}} {{$IF SizeOf(aLiAs.Export)=8}}{selected_include}{{$DEFINE ALIAS_HIT}}{{$ENDIF}}"
                );
                let queries = if other_first {
                    format!("{other} {proven}")
                } else {
                    format!("{proven} {other}")
                };
                let mut f = Fixture::new(
                    &format!("unit Main; interface uses {imports}; {queries} implementation end."),
                    context(LayoutPlatform::Win32),
                );
                f.unit(
                    "Alias",
                    "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                    context(LayoutPlatform::Win32),
                );
                f.unit_qualifiers("Alias", &["Alias", "Vendor.Types"]);
                f.unit("Other", UNCERTAIN_OTHER, context(LayoutPlatform::Win32));
                f.include("hit.inc", " ");
                let a = f.analyze().unwrap();
                if selected_include.is_empty() {
                    let alias = assert_qualified_export_fact(
                        &f,
                        &a,
                        "{layout-site}",
                        "aLiAs.Export",
                        "Alias",
                        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                    );
                    let namespace = assert_qualified_export_fact(
                        &f,
                        &a,
                        "{layout-site}",
                        "Vendor.Types.Export",
                        "Alias",
                        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                    );
                    assert_eq!(alias.identity, namespace.identity);
                } else {
                    assert!(
                        !a.root.complete,
                        "opaque unknown include retains the existing incomplete-analysis policy"
                    );
                }
                assert_eq!(
                    activity(&a, "DEFINE OTHER_HIT"),
                    Truth::Unknown,
                    "{imports}, other_first={other_first}"
                );
                assert_eq!(
                    activity(&a, "DEFINE ALIAS_HIT"),
                    Truth::Unknown,
                    "{imports}, other_first={other_first}"
                );
                assert_eq!(
                    activity(&a, "DEFINE NAMESPACE_HIT"),
                    Truth::True,
                    "{imports}, other_first={other_first}"
                );
                assert!(!f.read_names().iter().any(|name| name == "hit.inc"));
            }
        }
    }
}

#[test]
fn dotted_unqualified_imported_record_operands_retain_member_index_and_deref_layout() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Value.Field[0]^)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types", "unit Types; interface type P=^Int64; R=record Field:array[0..1] of P; end; var Value:R; implementation end.", context(LayoutPlatform::Win32));
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn participating_shared_qualifiers_remain_uncertain_or_ambiguous_despite_equal_sizes() {
    for uncertain in [true, false] {
        let mut f = Fixture::new(
            "unit Main; interface uses Types, Other; {$IF SizeOf(Shared.Export)=8}{$DEFINE HIT}{$ENDIF} {$IF Shared.Value=8}{$DEFINE CONSTANT_HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Types",
            "unit Types; interface type Export=Int64; const Value=8; implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit_qualifiers("Types", &["Types", "Shared"]);
        f.unit("Other", if uncertain { "unit Other; interface {$IFDEF OPEN} type Export=Int64; const Value=8; {$ELSE} type Export=UInt64; const Value=8; {$ENDIF} implementation end." } else { "unit Other; interface type Export=Int64; const Value=8; implementation end." }, context(LayoutPlatform::Win32));
        f.unit_qualifiers("Other", &["Other", "Shared"]);
        let a = f.analyze().unwrap();
        assert_eq!(
            activity(&a, "DEFINE HIT"),
            Truth::Unknown,
            "uncertain={uncertain}"
        );
        assert_eq!(
            activity(&a, "DEFINE CONSTANT_HIT"),
            Truth::Unknown,
            "uncertain={uncertain}"
        );
    }
}

#[test]
fn local_root_shadow_blocks_qualified_constants_and_layout_after_provider_queries() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE BEFORE}{$ENDIF} var Types:Byte; {$IF SizeOf(Types.Export)=8}{$DEFINE LAYOUT_HIT}{$ENDIF} {$IF Types.Value=8}{$DEFINE CONSTANT_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE BEFORE"), Truth::True);
    assert_eq!(activity(&a, "DEFINE LAYOUT_HIT"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::Unknown);
}

#[test]
fn qualified_layout_stays_proven_without_an_earlier_uncertain_provider_query() {
    for prior in ["", "{$IF MISSING}{$DEFINE OTHER_HIT}{$ENDIF}"] {
        for selected_include in ["", "{$I hit.inc}"] {
            let imports = if prior.is_empty() {
                "Types, Other"
            } else {
                "Types"
            };
            let mut f = Fixture::new(
                &format!(
                    "unit Main; interface uses {imports}; {prior} {{$IF Types.Value=8}}{{$DEFINE CONSTANT_HIT}}{{$ENDIF}} {{layout-site}} {{$IF SizeOf(Types.Export)=8}}{selected_include}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
                ),
                context(LayoutPlatform::Win32),
            );
            f.unit(
                "Types",
                "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                context(LayoutPlatform::Win32),
            );
            f.unit("Other", UNCERTAIN_OTHER, context(LayoutPlatform::Win32));
            f.include("hit.inc", " ");
            let a = f.analyze().unwrap();
            if selected_include.is_empty() || !prior.is_empty() {
                assert_qualified_export_fact(
                    &f,
                    &a,
                    "{layout-site}",
                    "Types.Export",
                    "Types",
                    "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                );
            } else {
                assert!(
                    !a.root.complete,
                    "opaque unknown include retains the existing incomplete-analysis policy"
                );
            }
            assert_eq!(
                activity(&a, "DEFINE HIT"),
                if prior.is_empty() {
                    Truth::Unknown
                } else {
                    Truth::True
                }
            );
            assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
            assert_eq!(
                f.read_names()
                    .iter()
                    .filter(|name| *name == "hit.inc")
                    .count(),
                usize::from(!prior.is_empty() && !selected_include.is_empty())
            );
            assert_eq!(
                f.unit_requests
                    .iter()
                    .filter(|(_, name)| name == "Other")
                    .count(),
                usize::from(prior.is_empty())
            );
        }
    }
}

#[test]
fn imported_alias_layout_uses_provider_scope_and_context() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$DEFINE PRIVATE} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types", "unit Types; interface {$IFDEF PRIVATE} type Export=Byte; {$ELSE} type Export=Int64; {$ENDIF} implementation type Hidden=Byte; end.", context(LayoutPlatform::Win32).with_absent_define(Truth::False));
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

#[test]
fn imported_variable_member_index_and_pointer_use_captured_types() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; type Base=Byte; {$IF SizeOf(Types.Value.Field[0]^)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types", "unit Types; interface type Base=Int64; P=^Base; R=record Field:array[0..1] of P; end; var Value:R; implementation end.", context(LayoutPlatform::Win32));
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn implementation_private_and_unimported_qualifiers_are_unproven() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Hidden)=1}{$DEFINE PRIVATE_HIT}{$ENDIF} {$IF SizeOf(System.Byte)=1}{$DEFINE SYSTEM_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Byte; implementation type Hidden=Byte; end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE PRIVATE_HIT"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE SYSTEM_HIT"), Truth::Unknown);
}

#[test]
fn local_qualifier_shadow_blocks_imported_access() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; var Types:Byte; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn unknown_import_activity_never_reads_provider() {
    let mut f = Fixture::new(
        "unit Main; interface {$IFDEF OPEN} uses Types; {$ENDIF} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
    assert!(f.read_names().is_empty());
}

#[test]
fn imported_record_alias_member_preserves_provider_graph() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; type Alias=Types.R; var V:Alias; {$IF SizeOf(V.Field)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type R=record Field:Int64; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn imported_array_selector_uses_importer_index_type() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; var Index:Integer; {$IF SizeOf(Types.V[Index])=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type A=array[0..1] of Int64; var V:A; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn provider_interface_does_not_read_implementation_includes() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation {$I forbidden.inc} end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    assert!(!f.read_names().contains(&"forbidden.inc".to_string()));
}

#[test]
fn fresh_provider_revision_changes_exported_layout() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    f.unit(
        "Types",
        "unit Types; interface type Export=Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::False);
}

#[test]
fn recursive_providers_keep_pointer_storage_distinct_from_by_value() {
    let mut f = Fixture::new(
        "unit Main; interface uses A; {$IF SizeOf(A.P)=4}{$DEFINE POINTER_HIT}{$ENDIF} {$IF SizeOf(A.R)=4}{$DEFINE VALUE_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "A",
        "unit A; interface uses B; type P=^B.R; R=record Field:B.R; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "B",
        "unit B; interface uses A; type R=record Field:A.R; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE POINTER_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE VALUE_HIT"), Truth::Unknown);
}

#[test]
fn open_provider_condition_does_not_export_matching_size_identity() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types","unit Types; interface {$IFDEF OPEN} type Export=Int64; {$ELSE} type Export=UInt64; {$ENDIF} implementation end.",context(LayoutPlatform::Win32));
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn shared_provider_query_limit_precedes_provider_reads() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_provider_queries = 0;
    assert!(f.analyze().is_err());
    assert!(f.read_names().is_empty());
}

#[test]
fn authorized_namespace_aliases_expose_types_and_variables() {
    let mut f = Fixture::new(
        "unit Main; interface uses Alias; {$IF SizeOf(Alias.Export)=8}{$DEFINE TYPE_HIT}{$ENDIF} {$IF SizeOf(Vendor.Types.V)=8}{$DEFINE VAR_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Alias",
        "unit Vendor.Types; interface type Export=Int64; var V:Export; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit_qualifiers("Alias", &["Alias", "Vendor.Types"]);
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE TYPE_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE VAR_HIT"), Truth::True);
}

#[test]
fn unavailable_and_ambiguous_providers_do_not_read_or_prove_layout() {
    for ambiguous in [false, true] {
        let mut f = Fixture::new(
            "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Types",
            "unit Types; interface type Export=Int64; implementation end.",
            context(LayoutPlatform::Win32),
        );
        if ambiguous {
            f.ambiguous_unit("Types");
        } else {
            f.deny_unit("Types");
        }
        assert_eq!(
            activity(&f.analyze().unwrap(), "DEFINE HIT"),
            Truth::Unknown
        );
        assert!(f.read_names().is_empty());
    }
}

#[test]
fn provider_includes_inherit_provider_defines_and_layout_context() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$DEFINE PRIVATE} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface {$DEFINE PROVIDER}{$I types.inc} implementation end.",
        context(LayoutPlatform::Win32).with_absent_define(Truth::False),
    );
    f.include("types.inc","{$IFDEF PROVIDER} {$IFNDEF PRIVATE} type Export=NativeInt; {$ELSE} type Export=Byte; {$ENDIF} {$ENDIF}");
    // Separately compiled providers select their own platform too.
    f.unit(
        "Types",
        "unit Types; interface {$DEFINE PROVIDER}{$I types.inc} implementation end.",
        context(LayoutPlatform::Win64).with_absent_define(Truth::False),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn captured_constant_and_enum_array_bounds_use_shared_expression_language() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; const Limit=0; {$IF SizeOf(Types.Export)=24}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types","unit Types; interface const Limit=(1 shl 1); type E=(First, Second, Third); Export=array[First..Third] of array[0..Limit-1] of Integer; implementation end.",context(LayoutPlatform::Win32));
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn independently_selected_same_named_exports_are_ambiguous() {
    let mut f = Fixture::new(
        "unit Main; interface uses A,B; {$IF SizeOf(Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    for name in ["A", "B"] {
        f.unit(
            name,
            &format!("unit {name}; interface type Export=Int64; implementation end."),
            context(LayoutPlatform::Win32),
        );
    }
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn qualified_alias_binding_respects_local_value_blocker() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; var Types:Byte; type Alias=Types.Export; {$IF SizeOf(Alias)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn captured_interface_fingerprint_retains_physical_revision() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let first = f.analyze().unwrap().facts.fingerprint();
    f.unit_revision("Types", 2);
    assert_ne!(first, f.analyze().unwrap().facts.fingerprint());
}

#[test]
fn imported_layout_returns_provider_revision_dependency() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, Fact, OccurrenceId};
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), f.root.bytes.len())
        .unwrap();
    let mut budget =
        CompileTimeBudget::new(CompileTimeLimits::default(), &pascal_core::NoCancellation);
    let Fact::Known(layout) = a.facts.layout_name(site, "Types.Export", &mut budget) else {
        panic!("proven export layout");
    };
    assert!(layout.dependencies.iter().any(|observation|matches!(observation,pascal_core::ResolutionObservation::Payload { source_id,revision:pascal_core::SourceRevision::Overlay { version:1,.. },.. } if source_id.as_str()=="/fixture/Types.pas")));
}

#[test]
fn unused_active_import_does_not_compile_provider_interface() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface {$I unused.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("unused.inc", "type Export=Int64;");
    f.analyze().unwrap();
    assert!(!f.read_names().contains(&"unused.inc".to_string()));
}

#[test]
fn declaration_limit_is_aggregate_across_root_and_provider_graphs() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; type Local=Byte; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_definitions = 1;
    assert!(f.analyze().is_err());
}

#[test]
fn field_and_variant_caps_are_shared_by_root_and_provider() {
    for variants in [false, true] {
        let ty = if variants {
            "packed record case Byte of 0:(X:Byte); end"
        } else {
            "packed record X:Byte; end"
        };
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface uses Types; type Local={ty}; {{$IF SizeOf(Types.Export)=1}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
            ),
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Types",
            &format!("unit Types; interface type Export={ty}; implementation end."),
            context(LayoutPlatform::Win32),
        );
        if variants {
            f.limits.max_variant_alternatives = 1;
        } else {
            f.limits.max_fields = 1;
        }
        assert!(
            f.analyze().is_err(),
            "aggregate dimension variants={variants}"
        );
    }
}

#[test]
fn nested_provider_definition_cap_is_not_reset() {
    let mut f = Fixture::new(
        "unit Main; interface uses A; type Local=Byte; {$IF SizeOf(A.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "A",
        "unit A; interface uses B; type Export=B.Export; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "B",
        "unit B; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_definitions = 2;
    assert!(f.analyze().is_err());
    f.limits.max_definitions = 3;
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn conditional_snapshots_do_not_readmit_existing_syntax() {
    let mut f = Fixture::new(
        "unit Main; interface type Local=packed record case Byte of 0:(X:Byte); end; {$IFDEF OPEN}{$DEFINE SIDE}{$ENDIF} {$IF SizeOf(Local)=1}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_definitions = 1;
    f.limits.max_fields = 1;
    f.limits.max_variant_alternatives = 1;
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn bound_constant_queries_compile_only_when_used_and_respect_blockers() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; const C=Types.Limit; {$IF C=2}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface {$I constant.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("constant.inc", "const Limit=(1 shl 1);");
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    let mut blocked = Fixture::new(
        "unit Main; interface uses Types; const Types=1; {$IF Types.Limit=2}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    blocked.unit(
        "Types",
        "unit Types; interface {$I forbidden.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    blocked.analyze().unwrap();
    assert!(blocked.read_names().is_empty());
}

#[test]
fn split_import_uses_physical_name_opener_not_completing_file() {
    let mut f = Fixture::new(
        "unit Main; interface uses Ty{$I suffix.inc} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("suffix.inc", "pes;");
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    assert_eq!(f.unit_requests[0].0, f.root.path);
}

#[test]
fn include_import_opener_survives_parent_completion() {
    let mut f = Fixture::new(
        "unit Main; interface {$I sub/imports.inc}pes; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("sub/imports.inc", "uses Ty");
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    assert_eq!(
        f.unit_requests[0].0,
        std::path::PathBuf::from("/fixture/sub/imports.inc")
    );
}

#[test]
fn qualified_query_does_not_compile_unrelated_import() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types, Unused; {$IF Types.Value=8}{$DEFINE HIT}{$ENDIF} {layout-site} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Unused",
        "unit Unused; interface {$I unused.inc} type Export=Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("unused.inc", "type Ignored=Byte;");
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    let reads_before_operand = f.read_names();
    let requests_before_operand = f.unit_requests.clone();
    assert_qualified_export_fact(
        &f,
        &a,
        "{layout-site}",
        "Types.Export",
        "Types",
        "unit Types; interface type Export=Int64; const Value=8; implementation end.",
    );
    // Provider capability discovery learns authorized qualifier spellings. The
    // operand-only query must not compile Unused's interface or add any read.
    assert_eq!(reads_before_operand, vec!["Types", "Unused"]);
    assert_eq!(f.read_names(), reads_before_operand);
    assert_eq!(f.unit_requests, requests_before_operand);
    assert_eq!(f.unit_requests.len(), 2);
    assert_eq!(f.unit_requests[0].1, "Types");
    assert_eq!(f.unit_requests[1].1, "Unused");
    assert!(!f.read_names().iter().any(|name| name == "unused.inc"));
}

#[test]
fn callee_absence_is_a_separate_authorized_provider_proof() {
    for mode in ["absent", "blocker", "unknown"] {
        for selected_include in ["", "{$I hit.inc}"] {
            let imports = if mode == "unknown" {
                "uses Types; {$IFDEF OPEN} uses Unused; {$ENDIF}"
            } else {
                "uses Types, Unused;"
            };
            let mut f = Fixture::new(
                &format!(
                    "unit Main; interface {imports} {{$IF Types.Value=8}}{{$DEFINE CONSTANT_HIT}}{{$ENDIF}} {{layout-site}} {{$IF SizeOf(Types.Export)=8}}{selected_include}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
                ),
                context(LayoutPlatform::Win32),
            );
            f.unit(
                "Types",
                "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                context(LayoutPlatform::Win32),
            );
            f.unit(
                "Unused",
                "unit Unused; interface {$I unused.inc} type Export=Byte; implementation end.",
                context(LayoutPlatform::Win32),
            );
            f.include(
                "unused.inc",
                if mode == "blocker" {
                    "const SizeOf=1;"
                } else {
                    "type Ignored=Byte;"
                },
            );
            f.include("hit.inc", " ");
            let a = f.analyze().unwrap();
            if selected_include.is_empty() || mode == "absent" {
                assert_qualified_export_fact(
                    &f,
                    &a,
                    "{layout-site}",
                    "Types.Export",
                    "Types",
                    "unit Types; interface type Export=Int64; const Value=8; implementation end.",
                );
            } else {
                assert!(
                    !a.root.complete,
                    "opaque unknown include retains the existing incomplete-analysis policy"
                );
            }
            assert_eq!(activity(&a, "DEFINE CONSTANT_HIT"), Truth::True);
            assert_eq!(
                activity(&a, "DEFINE HIT"),
                if mode == "absent" {
                    Truth::True
                } else {
                    Truth::Unknown
                }
            );
            match mode {
                "absent" => assert_eq!(
                    f.read_names(),
                    if selected_include.is_empty() {
                        vec!["Types", "Unused", "unused.inc"]
                    } else {
                        vec!["Types", "Unused", "unused.inc", "hit.inc"]
                    }
                ),
                "blocker" => assert_eq!(f.read_names(), vec!["Types", "Unused", "unused.inc"]),
                "unknown" => {
                    assert_eq!(f.read_names(), vec!["Types", "Types"]);
                    assert_eq!(f.unit_requests.len(), 2);
                    assert!(!f.unit_requests.iter().any(|(_, name)| name == "Unused"));
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn by_value_provider_cycle_has_explicit_provenance_and_does_not_poison_pointer() {
    use pascal_core::compile_time::{
        CompileTimeBudget, CompileTimeLimits, Fact, OccurrenceId, UnknownReason,
    };
    let mut f = Fixture::new(
        "unit Main; interface uses A; {$IF SizeOf(A.R)=4}{$DEFINE VALUE}{$ENDIF} {$IF SizeOf(A.P)=4}{$DEFINE POINTER}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "A",
        "unit A; interface uses B; type R=packed record Field:B.R; end; P=^B.R; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "B",
        "unit B; interface uses A; type R=packed record Field:A.R; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE POINTER"), Truth::True);
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), f.root.bytes.len())
        .unwrap();
    let mut b = CompileTimeBudget::new(CompileTimeLimits::default(), &pascal_core::NoCancellation);
    let fact = a.facts.layout_name(site, "A.R", &mut b);
    assert!(
        matches!(fact, Fact::Unknown(UnknownReason::CyclicProvider)),
        "{fact:?}"
    );
}

/// l4d-4nbs R9: a provider imported by two units of one session (guiutils
/// pulls Windows.pas through RzCommon and again through Classes) is analysed
/// once; later imports prepare their names on a copy of the session's
/// interface graph. Re-analysing it (1.3 MB) had no matching grant, since
/// the session credits each byte once, and exceeded `max_byte_work`.
fn big_provider() -> String {
    let comment = "documentation of a provider constant ".repeat(6);
    let mut big = String::from("unit W; interface type TW = Int64;\nconst\n");
    let mut n = 0;
    while big.len() < 2_000_000 {
        big.push_str(&format!("  C{n} = {n}; {{ {comment} }}\n"));
        n += 1;
    }
    big.push_str("implementation end.");
    big
}

fn provider_fixture(root: &str, units: &[(&str, &str)]) -> Fixture {
    let mut f = Fixture::new(root, context(LayoutPlatform::Win32));
    for (name, text) in units {
        f.unit(name, text, context(LayoutPlatform::Win32));
    }
    f.unit("W", &big_provider(), context(LayoutPlatform::Win32));
    f.limits.max_retained_bytes = 64 * 1024 * 1024;
    f
}

#[test]
fn a_provider_imported_twice_in_one_session_is_analysed_once() {
    let mut alone = provider_fixture(
        "unit Main; interface uses W; {$IF SizeOf(W.TW)=8}{$DEFINE HW}{$ENDIF} implementation end.",
        &[],
    );
    let alone = alone.analyze().unwrap();
    assert_eq!(activity(&alone, "DEFINE HW"), Truth::True);
    let one_analysis = alone.budget.budget_snapshot().work;
    let mut twice = provider_fixture(
        "unit Main; interface uses A, B; {$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF}{$IF SizeOf(B.TB)=8}{$DEFINE HB}{$ENDIF} implementation end.",
        &[
            (
                "A",
                "unit A; interface uses W; type TA = W.TW; implementation end.",
            ),
            (
                "B",
                "unit B; interface uses W; type TB = W.TW; implementation end.",
            ),
        ],
    );
    let analysis = twice.analyze().unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE HA"), Truth::True);
    assert_eq!(activity(&analysis, "DEFINE HB"), Truth::True);
    let work = analysis.budget.budget_snapshot().work;
    // One scan of W plus copies of its graph, not a second scan.
    assert!(
        work < one_analysis * 3 / 2,
        "{work} steps; one analysis of W takes {one_analysis}"
    );
}

/// l4d-4nbs R9: in the LSP the same Windows.pas is found through different
/// search entries by different importers: its revisions differ (a distinct
/// provider identity) while its content does not. The session credited its
/// bytes once per content, so the second analysis had no grant and Log.pas
/// failed at `max_byte_work`; each distinct revision is funded once.
#[test]
fn a_provider_found_through_different_entries_is_funded_once_per_revision() {
    let tight = |f: &mut Fixture| {
        // Close to the scan's real cost, so an unfunded repeat cannot fit.
        f.limits.max_byte_work = 4 * 1024 * 1024;
        f.limits.byte_work_per_source_byte = 16;
    };
    let mut alone = provider_fixture(
        "unit Main; interface uses W; {$IF SizeOf(W.TW)=8}{$DEFINE HW}{$ENDIF} implementation end.",
        &[],
    );
    tight(&mut alone);
    let alone = alone.analyze().unwrap();
    assert_eq!(
        activity(&alone, "DEFINE HW"),
        Truth::True,
        "{:?}",
        alone.reasons
    );
    let mut twice = provider_fixture(
        "unit Main; interface uses A, B; {$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF}{$IF SizeOf(B.TB)=8}{$DEFINE HB}{$ENDIF} implementation end.",
        &[
            (
                "A",
                "unit A; interface uses W; type TA = W.TW; implementation end.",
            ),
            (
                "B",
                "unit B; interface uses W; type TB = W.TW; implementation end.",
            ),
        ],
    );
    tight(&mut twice);
    twice.importer_revisions = true;
    let analysis = twice.analyze();
    let analysis = analysis
        .as_ref()
        .map(|a| (&a.reasons, a.budget.budget_snapshot()));
    assert!(
        analysis
            .as_ref()
            .is_ok_and(|(reasons, _)| reasons.is_empty()),
        "{analysis:?}"
    );
}

/// l4d-4nbs R13: a later import of a provider whose session graph already
/// prepared the same name shares that graph instead of copying it. Each
/// copy of Windows.pas's graph cost 12.5M of byte work (75M per Log.pas
/// session).
#[test]
fn a_provider_graph_prepared_for_the_same_name_is_shared_not_copied() {
    let query = "{$IF Declared(Missing)}{$DEFINE HIT}{$ENDIF}";
    let mut one = provider_fixture(
        &format!("unit Main; interface uses A; {query} implementation end."),
        &[("A", "unit A; interface uses W; implementation end.")],
    );
    let one = one.analyze().unwrap().budget.budget_snapshot().byte_work;
    let units = ["A", "B", "C", "D"];
    let texts = units
        .iter()
        .map(|u| format!("unit {u}; interface uses W; implementation end."))
        .collect::<Vec<_>>();
    let mut four = provider_fixture(
        &format!("unit Main; interface uses A, B, C, D; {query} implementation end."),
        &units
            .iter()
            .zip(&texts)
            .map(|(u, t)| (*u, t.as_str()))
            .collect::<Vec<_>>(),
    );
    let analysis = four.analyze().unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    let four = analysis.budget.budget_snapshot().byte_work;
    assert!(
        four < one + one / 4,
        "{four} byte work for four importers of W, {one} for one"
    );
}

/// l4d-4nbs R13: one file state reached through different search entries
/// (Windows.pas was analysed three times in one Log.pas session and its
/// declarations exceeded `max_definitions`) is one provider identity.
#[test]
fn a_disk_provider_reached_through_different_entries_is_one_identity() {
    let mut alone = provider_fixture(
        "unit Main; interface uses W; {$IF SizeOf(W.TW)=8}{$DEFINE HW}{$ENDIF} implementation end.",
        &[],
    );
    let one_analysis = alone.analyze().unwrap().budget.budget_snapshot().work;
    let mut twice = provider_fixture(
        "unit Main; interface uses A, B; {$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF}{$IF SizeOf(B.TB)=8}{$DEFINE HB}{$ENDIF} implementation end.",
        &[
            (
                "A",
                "unit A; interface uses W; type TA = W.TW; implementation end.",
            ),
            (
                "B",
                "unit B; interface uses W; type TB = W.TW; implementation end.",
            ),
        ],
    );
    twice.importer_entries = true;
    let analysis = twice.analyze().unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE HB"), Truth::True);
    let work = analysis.budget.budget_snapshot().work;
    assert!(
        work < one_analysis * 3 / 2,
        "{work} steps; one analysis of W takes {one_analysis}"
    );
}

/// l4d-4nbs R13: SysUtils' 25 `{$IF CompilerVersion …}` each copied the
/// Windows.pas graph to prepare the same name again (65M of byte work).
/// A graph already prepared for a name is shared.
#[test]
fn repeated_queries_of_one_name_do_not_copy_provider_graphs() {
    let usage = |queries: usize| {
        let text = (0..queries)
            .map(|n| format!("{{$IF CompilerVersion > {n}}}{{$DEFINE Q{n}}}{{$IFEND}}\n"))
            .collect::<String>();
        let mut f = provider_fixture(
            &format!("unit Main; interface uses W; {text} implementation end."),
            &[],
        );
        let analysis = f.analyze().unwrap();
        assert!(analysis.root.complete, "{:?}", analysis.reasons);
        analysis.budget.budget_snapshot().byte_work
    };
    let (one, many) = (usage(1), usage(25));
    assert!(
        many < one + one / 4,
        "{many} byte work for 25 queries, {one} for one"
    );
}

/// l4d-4nbs D19: `uses classes` and `uses Classes` name one unit: one
/// provider identity, analysed once (Classes.pas was analysed for each
/// spelling in BaseRECORDLOCK's session).
#[test]
fn qualifier_spellings_of_one_provider_are_one_identity() {
    let mut alone = provider_fixture(
        "unit Main; interface uses W; {$IF SizeOf(W.TW)=8}{$DEFINE HW}{$ENDIF} implementation end.",
        &[],
    );
    let one_analysis = alone.analyze().unwrap().budget.budget_snapshot().work;
    let mut twice = provider_fixture(
        "unit Main; interface uses A, B; {$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF}{$IF SizeOf(B.TB)=8}{$DEFINE HB}{$ENDIF} implementation end.",
        &[
            (
                "A",
                "unit A; interface uses W; type TA = W.TW; implementation end.",
            ),
            (
                "B",
                "unit B; interface uses w; type TB = w.TW; implementation end.",
            ),
        ],
    );
    twice.spelled_units = true;
    let analysis = twice.analyze().unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE HB"), Truth::True);
    let work = analysis.budget.budget_snapshot().work;
    assert!(
        work < one_analysis * 3 / 2,
        "{work} steps; one analysis of W takes {one_analysis}"
    );
}

/// l4d-4nbs D19: a provider whose interface cannot be completed is
/// analysed once per session, not again for every importer (Classes.pas:
/// 14 analyses, 120M of byte work, in BaseRECORDLOCK's session).
#[test]
fn an_incomplete_provider_interface_is_analysed_once() {
    let usage = |importers: usize| {
        let mut big = String::from("unit W; interface {$I missing.inc}\nconst\n");
        let comment = "documentation of a provider constant ".repeat(6);
        let mut n = 0;
        while big.len() < 2_000_000 {
            big.push_str(&format!("  C{n} = {n}; {{ {comment} }}\n"));
            n += 1;
        }
        big.push_str("implementation end.");
        let names = (0..importers).map(|i| format!("A{i}")).collect::<Vec<_>>();
        let texts = names
            .iter()
            .map(|n| format!("unit {n}; interface uses W; implementation end."))
            .collect::<Vec<_>>();
        let mut f = Fixture::new(
            &format!(
                "unit Main; interface uses {}; {{$IF Declared(Missing)}}{{$DEFINE HIT}}{{$ENDIF}} implementation end.",
                names.join(", ")
            ),
            context(LayoutPlatform::Win32),
        );
        for (n, t) in names.iter().zip(&texts) {
            f.unit(n, t, context(LayoutPlatform::Win32));
        }
        f.unit("W", &big, context(LayoutPlatform::Win32));
        f.limits.max_retained_bytes = 64 * 1024 * 1024;
        let analysis = f.analyze().unwrap();
        analysis.budget.budget_snapshot().work
    };
    let (one, four) = (usage(1), usage(4));
    assert!(
        four < one * 3 / 2,
        "{four} steps for four importers, {one} for one"
    );
}

/// l4d-4nbs D19: a unit closure can declare more than 32,768 names
/// (SFClient2's BOSFBase units: 41,838 across RTL, VCL, DB, IBX, InfoPower
/// and business objects). The count fuse is a multiple of that; memory
/// stays bounded by the retained-bytes limit.
#[test]
fn a_closure_with_forty_thousand_declarations_is_analysed() {
    let mut big = String::from("unit W; interface\nconst\n");
    for n in 0..40_000 {
        big.push_str(&format!("  C{n} = {n};\n"));
    }
    big.push_str("implementation end.");
    let mut f = Fixture::new(
        "unit Main; interface uses W; {$IF Declared(Missing)}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("W", &big, context(LayoutPlatform::Win32));
    f.limits.max_retained_bytes = 256 * 1024 * 1024;
    let analysis = f.analyze().unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
}

/// l4d-4nbs D19: measured on SFClient2 (RAD Studio 7.0 RTL/VCL/DB/IBX and
/// InfoPower closures), a session's byte work needs about 46 per scanned
/// source byte beyond the fixed budget; the grant keeps a margin of more
/// than a third over that.
#[test]
fn the_byte_grant_covers_the_measured_closure_cost_with_margin() {
    let grant =
        pascal_core::compile_time::CompileTimeLimits::default().byte_work_per_source_byte as f64;
    assert!(grant >= 46.0 * 1.35, "{grant} byte work per source byte");
}

/// A shared store of compiled provider interfaces that counts compilations
/// (puts of a key it holds no valid graph for) and reuses (hits).
#[derive(Default)]
struct CountingStore {
    graphs: std::sync::Mutex<
        std::collections::HashMap<
            pascal_core::compile_time::ProviderGraphKey,
            std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>,
        >,
    >,
    puts: std::sync::atomic::AtomicUsize,
    hits: std::sync::atomic::AtomicUsize,
    /// Overlay versions sources have now, as a real store's probes see them.
    current: std::sync::Mutex<std::collections::HashMap<String, i32>>,
}

impl pascal_core::compile_time::SharedProviderGraphs for CountingStore {
    fn get(
        &self,
        key: &pascal_core::compile_time::ProviderGraphKey,
    ) -> Option<std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>> {
        let hit = self
            .graphs
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .filter(|graph| self.valid(graph));
        if hit.is_some() {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        hit
    }
    fn put(
        &self,
        key: pascal_core::compile_time::ProviderGraphKey,
        graph: std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>,
    ) {
        let mut graphs = self.graphs.lock().unwrap();
        // A graph with more names prepared replaces a valid one: that is no
        // compilation.
        if !graphs.get(&key).is_some_and(|old| self.valid(old)) {
            self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        graphs.insert(key, graph);
    }
}

impl CountingStore {
    fn valid(&self, graph: &pascal_core::compile_time::SharedProviderGraph) -> bool {
        let current = self.current.lock().unwrap();
        graph.dependencies.iter().all(|d| {
            current
                .get(d.source.as_str())
                .is_none_or(|version| d.overlay_version == Some(*version))
        })
    }
    fn puts(&self) -> usize {
        self.puts.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }
}

const SHARED_ROOT: &str =
    "unit Main; interface uses A; {$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF} implementation end.";

fn shared_fixture(store: &std::sync::Arc<CountingStore>) -> Fixture {
    let mut f = provider_fixture(
        SHARED_ROOT,
        &[(
            "A",
            "unit A; interface uses W; type TA = W.TW; implementation end.",
        )],
    );
    f.shared = Some(store.clone());
    f
}

/// l4d-4nbs D29: a second request compiles no provider again: it reuses
/// the interfaces the first compiled, reports the same dependencies, and
/// pays only for preparing names, not for scanning (nothing is granted).
#[test]
fn consecutive_requests_compile_each_provider_once() {
    let store = std::sync::Arc::new(CountingStore::default());
    let first = shared_fixture(&store).analyze().unwrap();
    assert_eq!(activity(&first, "DEFINE HA"), Truth::True);
    let compiled = store.puts();
    assert_eq!(compiled, 2, "A and W");
    let second = shared_fixture(&store).analyze().unwrap();
    assert!(second.root.complete, "{:?}", second.reasons);
    assert_eq!(activity(&second, "DEFINE HA"), Truth::True);
    assert_eq!(store.puts(), compiled, "no provider compiled again");
    assert!(store.hits() >= 1);
    assert_eq!(
        first.observations, second.observations,
        "same dependencies reported"
    );
    let (one, two) = (
        first.budget.budget_snapshot(),
        second.budget.budget_snapshot(),
    );
    assert!(two.work * 4 < one.work, "{one:?} then {two:?}");
}

/// A changed provider source or define is a different key and compiles
/// again; differing environments do not share.
#[test]
fn a_changed_provider_or_environment_compiles_again() {
    let store = std::sync::Arc::new(CountingStore::default());
    shared_fixture(&store).analyze().unwrap();
    let compiled = store.puts();
    let mut changed = shared_fixture(&store);
    changed.unit_revision("W", 7);
    store
        .current
        .lock()
        .unwrap()
        .insert("/fixture/W.pas".into(), 7);
    changed.analyze().unwrap();
    assert_eq!(
        store.puts(),
        compiled + 2,
        "W compiled again for its new revision, and A, which depends on it"
    );
    // The next fixtures read W at its first revision again.
    store.current.lock().unwrap().remove("/fixture/W.pas");
    let mut defined = shared_fixture(&store);
    let mut extra = context(LayoutPlatform::Win32);
    extra.set_define("EXTRA", pascal_project::ConditionalFact::True);
    defined.unit(
        "A",
        "unit A; interface uses W; type TA = W.TW; implementation end.",
        extra,
    );
    defined.analyze().unwrap();
    assert_eq!(
        store.puts(),
        compiled + 3,
        "A compiled again under its new define"
    );
    let mut other = shared_fixture(&store);
    other.unit(
        "A",
        "unit A; interface uses W; type TA = W.TW; implementation end.",
        context(LayoutPlatform::Win64),
    );
    other.analyze().unwrap();
    assert_eq!(
        store.puts(),
        compiled + 4,
        "a Win64 A does not reuse the Win32 one"
    );
}

/// Occurrence ids are numbered per session, so a graph reused from another
/// session must not share ids with the graphs this session compiles: the
/// memo of compiled interfaces and declaration anchors tell them apart by
/// occurrence.
#[test]
fn shared_graphs_never_share_occurrence_ids() {
    let store = std::sync::Arc::new(CountingStore::default());
    shared_fixture(&store).analyze().unwrap();
    let mut both = provider_fixture(
        "unit Main; interface uses B, A; {$IF SizeOf(B.TB)=4}{$DEFINE HB}{$ENDIF}{$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF} implementation end.",
        &[
            (
                "A",
                "unit A; interface uses W; type TA = W.TW; implementation end.",
            ),
            (
                "B",
                "unit B; interface type TB = Integer; implementation end.",
            ),
        ],
    );
    both.shared = Some(store.clone());
    let analysis = both.analyze().unwrap();
    assert_eq!(activity(&analysis, "DEFINE HB"), Truth::True);
    assert_eq!(activity(&analysis, "DEFINE HA"), Truth::True);
    assert!(store.hits() >= 1, "A is reused");
    let graphs = store.graphs.lock().unwrap();
    let mut occurrences = graphs.values().map(|g| g.occurrence).collect::<Vec<_>>();
    occurrences.sort_by_key(|o| o.0);
    let count = occurrences.len();
    occurrences.dedup();
    assert_eq!(occurrences.len(), count, "{occurrences:?}");
}

/// A name a reusing session prepares on a shared interface is shared too:
/// the next session that asks for it copies and prepares nothing.
#[test]
fn names_prepared_on_a_shared_interface_are_shared() {
    let store = std::sync::Arc::new(CountingStore::default());
    let a = "unit A; interface uses W; type TA = W.TW; TB = W.TW; implementation end.";
    let session = |root: &str| {
        let mut f = provider_fixture(root, &[("A", a)]);
        f.shared = Some(store.clone());
        f.analyze().unwrap()
    };
    session(SHARED_ROOT);
    let tb =
        "unit Main; interface uses A; {$IF SizeOf(A.TB)=8}{$DEFINE HB}{$ENDIF} implementation end.";
    let preparing = session(tb);
    assert_eq!(activity(&preparing, "DEFINE HB"), Truth::True);
    let puts = store.puts();
    let prepared = session(tb);
    assert_eq!(activity(&prepared, "DEFINE HB"), Truth::True);
    assert_eq!(store.puts(), puts, "nothing new is prepared");
    let (one, two) = (
        preparing.budget.budget_snapshot(),
        prepared.budget.budget_snapshot(),
    );
    assert!(
        two.retained_bytes < one.retained_bytes,
        "{one:?} then {two:?}"
    );
}

fn provider_chain(levels: usize) -> Fixture {
    let mut f = Fixture::new(
        "unit Main; interface uses U0; {$IF SizeOf(U0.T0)=8}{$DEFINE DEEP}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    for n in 0..levels {
        let text = if n + 1 == levels {
            format!("unit U{n}; interface type T{n} = Int64; implementation end.")
        } else {
            let m = n + 1;
            format!("unit U{n}; interface uses U{m}; type T{n} = U{m}.T{m}; implementation end.")
        };
        f.unit(&format!("U{n}"), &text, context(LayoutPlatform::Win32));
    }
    f.limits.max_retained_bytes = 512 * 1024 * 1024;
    f
}

/// l4d-4nbs D29: each nested provider interface costs about 50 KiB of stack
/// in a debug build (11 KiB in release), so a deep interface `uses` chain
/// overflowed a default 2 MiB thread at about 40 levels and aborted the
/// process. The nesting now stops at a stack budget and reports the limit.
#[test]
fn a_deep_interface_uses_chain_reports_the_stack_limit() {
    let analysis = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let mut f = provider_chain(300);
            f.analyze()
        })
        .unwrap()
        .join()
        .expect("no overflow");
    let limited = match &analysis {
        Ok(analysis) => analysis.reasons.iter().any(|reason| {
            matches!(
                reason,
                pascal_core::compile_time::IncompleteReason::Limit {
                    name: "max_provider_stack_bytes",
                    ..
                }
            )
        }),
        Err(pascal_core::ResolverError::LimitExceeded { limit, .. }) => {
            *limit == "max_provider_stack_bytes"
        }
        Err(_) => false,
    };
    assert!(limited, "{:?}", analysis.as_ref().map(|a| &a.reasons));
}

/// l4d-4nbs review I1: a shared interface is charged everything it keeps:
/// its provider sources' bytes and decoded text, its reports, dependencies,
/// scans, nested parts, lookup trace and key context.
#[test]
fn a_shared_interface_is_charged_what_it_retains() {
    let store = std::sync::Arc::new(CountingStore::default());
    // Mostly a comment: few declarations, many retained source bytes.
    let w_source = format!(
        "unit W; interface {{{}}} type TW = Int64; implementation end.",
        "x".repeat(1_000_000)
    );
    let mut f = Fixture::new(SHARED_ROOT, context(LayoutPlatform::Win32));
    f.unit(
        "A",
        "unit A; interface uses W; type TA = W.TW; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("W", &w_source, context(LayoutPlatform::Win32));
    f.limits.max_retained_bytes = 64 * 1024 * 1024;
    f.shared = Some(store.clone());
    f.analyze().unwrap();
    let graphs = store.graphs.lock().unwrap();
    let (key, w) = graphs
        .iter()
        .find(|(key, _)| key.source.as_str().ends_with("W.pas"))
        .expect("W is shared");
    let source = w_source.len();
    let reports = w.observations.len() * std::mem::size_of::<pascal_core::ResolutionObservation>()
        + w.warnings.iter().map(|w| w.len()).sum::<usize>()
        + w.dependencies.len()
            * std::mem::size_of::<pascal_core::compile_time::ProviderDependency>()
        + w.scanned.len() * std::mem::size_of::<pascal_core::compile_time::ScannedExtent>()
        + w.children.len() * std::mem::size_of::<pascal_core::compile_time::ChildSegment>()
        + w.trace.len() * std::mem::size_of::<pascal_core::ResolutionObservation>();
    assert!(
        w.retained_bytes >= source + reports + key.source.as_str().len(),
        "{} bytes charged for a {source} byte provider",
        w.retained_bytes
    );
}

/// l4d-4nbs re-review I1: an interface whose provider includes a 1 MB file
/// keeps no more source text than it is charged for.
#[test]
fn a_shared_interface_keeps_no_uncharged_include_text() {
    let store = std::sync::Arc::new(CountingStore::default());
    let mut f = Fixture::new(SHARED_ROOT, context(LayoutPlatform::Win32));
    f.unit(
        "A",
        "unit A; interface uses W; type TA = W.TW; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "W",
        "unit W; interface {$I big.inc} type TW = Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("big.inc", &format!("{{{}}}", "x".repeat(1_000_000)));
    f.limits.max_retained_bytes = 64 * 1024 * 1024;
    f.shared = Some(store.clone());
    let analysis = f.analyze().unwrap();
    assert_eq!(activity(&analysis, "DEFINE HA"), Truth::True);
    let graphs = store.graphs.lock().unwrap();
    for (key, graph) in graphs.iter() {
        let kept = graph
            .graph
            .as_ref()
            .map_or(0, |graph| graph.retained_source_bytes());
        assert!(
            kept <= graph.retained_bytes,
            "{} keeps {kept} source bytes, charged {}",
            key.source.as_str(),
            graph.retained_bytes
        );
    }
}

/// l4d-4nbs re-review minor: a nested lookup that fails does not leave the
/// resolver tracing consulted directories.
#[test]
fn a_failed_nested_lookup_stops_lookup_tracing() {
    let store = std::sync::Arc::new(CountingStore::default());
    let mut f = shared_fixture(&store);
    f.fail_unit_lookup = Some("W".into());
    assert!(f.analyze().is_err());
    assert!(!f.tracing, "tracing was left on");
}
