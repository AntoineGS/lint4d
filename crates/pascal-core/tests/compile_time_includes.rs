#[path = "support/compile_time.rs"]
mod support;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

#[test]
fn generic_resolver_uses_source_order_and_retains_observations() {
    use pascal_core::{
        FilesystemSourceStore, NoCancellation, ResolutionObservation, ResolverLimits, UnitResolver,
    };
    use pascal_project::{ProjectContext, ProjectOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    let text = "unit T; interface const N={$I revision.inc}; {$IF N=7}{$I chosen.inc}{$ELSE}{$I forbidden.inc}{$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    std::fs::write(dir.path().join("revision.inc"), "7").unwrap();
    std::fs::write(dir.path().join("chosen.inc"), "type A=array[1..N] of Byte;").unwrap();
    let mut ctx =
        ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    ctx.conditional_context = context(LayoutPlatform::Win32);
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    let a = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    let payloads = a
        .observations
        .iter()
        .filter_map(|o| match o {
            ResolutionObservation::Payload { path, .. } => {
                Some(path.file_name().unwrap().to_str().unwrap())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads, vec!["revision.inc", "chosen.inc"]);
    assert_eq!(
        resolver.include_usage(),
        (2, 1 + "type A=array[1..N] of Byte;".len(), 3)
    );
}

#[test]
fn included_initializer_controls_later_include_reads() {
    let mut f = Fixture::new(
        "unit T; interface const Revision={$I revision.inc}; {$IF Revision=7}{$I chosen.inc}{$ELSE}{$I forbidden.inc}{$ENDIF} {$IF SizeOf(Small)=7}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("revision.inc", "7");
    f.include("chosen.inc", "type Small=array[1..Revision] of Byte;");
    f.deny_include("forbidden.inc");
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(f.read_names(), vec!["revision.inc", "chosen.inc"]);
    assert!(a.reasons.is_empty());
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

#[test]
fn partial_initializer_failure_is_not_one() {
    let mut f = Fixture::new(
        "unit T; interface const Revision=1+{$I missing.inc}; {$IF Revision=1}{$DEFINE WRONG}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.deny_include("missing.inc");
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
    assert_eq!(f.read_names(), vec!["missing.inc"]);
}

#[test]
fn nested_multiple_fragments_preserve_occurrences() {
    let mut f = Fixture::new(
        "unit T; interface const N={$I outer.inc}+{$I number.inc}; {$IF N=14}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("outer.inc", "{$I number.inc}");
    f.include("number.inc", "7");
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(
        f.read_names(),
        vec!["outer.inc", "number.inc", "number.inc"]
    );
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    assert_eq!(a.occurrences.len(), 4);
    assert_ne!(a.includes[1].child, a.includes[2].child);
}

#[test]
fn child_switches_and_defines_resume_parent_context() {
    let mut f = Fixture::new(
        "unit T; interface type Before=string; {$I switches.inc} type After=string; {$IF (SizeOf(Before)=4) and (SizeOf(After)=256) and Defined(CHILD)}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("switches.inc", "{$H-}{$DEFINE CHILD}");
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

#[test]
fn unknown_arm_is_not_authorized_by_hypothetical_path() {
    let mut f = Fixture::new(
        "unit T; interface {$IF unknown}{$I secret.inc}{$ENDIF} {$IF true}{$DEFINE WRONG}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("secret.inc", "{$DEFINE SECRET}");
    let a = f.analyze().unwrap();
    assert!(f.read_names().is_empty());
    assert!(!a.root.complete);
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
}

#[test]
fn cycle_is_required_failure() {
    let mut f = Fixture::new(
        "unit T; interface const N={$I cycle.inc}; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("cycle.inc", "{$I cycle.inc}");
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert!(!a.reasons.is_empty());
    assert_eq!(f.read_names(), vec!["cycle.inc", "cycle.inc"]);
}

#[test]
fn child_open_comment_masks_parent_directives() {
    let mut f = Fixture::new(
        "unit T; interface {$I comment.inc}{$I forbidden.inc} const N=7; {$IF N=7}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("comment.inc", "{continued ");
    f.deny_include("forbidden.inc");
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(f.read_names(), vec!["comment.inc"]);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

#[test]
fn repeated_physical_bytes_keep_local_scope_and_occurrence() {
    let mut f = Fixture::new(
        "unit T; interface implementation procedure P; const N=1; {$I local.inc} begin end; procedure Q; const N=2; {$I local.inc} begin end; end.",
        context(LayoutPlatform::Win32),
    );
    f.include(
        "local.inc",
        "type A=array[1..N] of Byte; {$IF SizeOf(A)=1}{$DEFINE ONE}{$ELSE}{$DEFINE TWO}{$ENDIF}",
    );
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(f.read_names(), vec!["local.inc", "local.inc"]);
    let children = a
        .occurrences
        .iter()
        .filter(|o| o.id.0 != 0)
        .collect::<Vec<_>>();
    assert_ne!(children[0].id, children[1].id);
    for (child, selected) in children.iter().zip(["DEFINE ONE", "DEFINE TWO"]) {
        assert_eq!(
            child
                .analysis
                .directives
                .iter()
                .find(|d| d.body == selected)
                .unwrap()
                .activity,
            Truth::True
        );
    }
}

#[test]
fn split_operand_initializer_maps_child_and_parent_bytes() {
    use pascal_core::NoCancellation;
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, OccurrenceId};
    let text = "unit T; interface const N=SizeOf({$I operand.inc}); {$IF N=1}{$DEFINE HIT}{$ENDIF} implementation end.";
    let mut f = Fixture::new(text, context(LayoutPlatform::Win32));
    f.include("operand.inc", "Byte");
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    let child = a
        .occurrences
        .iter()
        .find(|o| o.id != OccurrenceId(0))
        .unwrap();
    assert!(a.facts.site_at(&child.source.id, child.id, 0).is_some());
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), text.find("{$IF N").unwrap());
    assert!(site.is_some());
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    assert!(
        a.facts
            .layout_name(site.unwrap(), "Byte", &mut budget)
            .known()
            .is_some()
    );
    assert_eq!(
        f.requests[0].1,
        text.find("{$I operand.inc}").unwrap()
            ..text.find("{$I operand.inc}").unwrap() + "{$I operand.inc}".len()
    );
}

#[test]
fn child_eof_does_not_close_literal_and_comment_continuations() {
    for (payload, resume) in [
        ("'abc", "{$I forbidden.inc}'"),
        ("(*", "{$I forbidden.inc}*)"),
        ("//", "{$I forbidden.inc}\n"),
    ] {
        let text = format!(
            "unit T; interface const S={{$I literal.inc}}{resume}; const N=7; {{$IF N=7}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
        );
        let mut f = Fixture::new(&text, context(LayoutPlatform::Win32));
        f.include("literal.inc", payload);
        f.deny_include("forbidden.inc");
        let a = f.analyze().unwrap();
        assert_eq!(f.read_names(), vec!["literal.inc"], "{payload}");
        // Comments in an otherwise empty initializer may be unsupported, but
        // never become an authorization to execute the embedded directive.
        if payload.starts_with('\'') {
            assert!(a.root.complete, "{:?}", a.reasons);
            assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
        }
    }
}

#[test]
fn conditional_initializer_fragments_inside_active_child_remain_correlated() {
    for (left, right, expected) in [("7", "7", Truth::True), ("7", "8", Truth::Unknown)] {
        let mut f = Fixture::new(
            "unit T; interface const N={$I value.inc}; {$IF N=7}{$DEFINE HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.include(
            "value.inc",
            &format!("{{$IF Mystery}}{left}{{$ELSE}}{right}{{$ENDIF}}"),
        );
        let a = f.analyze().unwrap();
        assert!(a.root.complete, "{:?}", a.reasons);
        assert_eq!(activity(&a, "DEFINE HIT"), expected);
        assert_eq!(f.read_names(), vec!["value.inc"]);
    }
}

#[test]
fn include_failure_classes_are_required_not_empty_fragments() {
    use pascal_core::Resolution;
    for failure in [
        Resolution::Unavailable {
            reason: "unauthorized".into(),
        },
        Resolution::Ambiguous { candidates: vec![] },
        Resolution::Incomplete {
            reason: "source byte limit".into(),
            candidates: vec![],
        },
    ] {
        let mut f = Fixture::new(
            "unit T; interface const N=1+{$I failed.inc}; {$IF N=1}{$DEFINE WRONG}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.fail_include("failed.inc", failure.clone());
        let a = f.analyze().unwrap();
        assert!(!a.root.complete);
        assert_eq!(a.includes[0].target, failure);
        assert_eq!(a.includes[0].child, None);
        assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
    }
}

#[test]
fn depth_limit_and_cancel_stop_before_child_lookup() {
    let mut f = Fixture::new(
        "unit T; interface {$I outer.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("outer.inc", "{$I forbidden.inc}");
    f.max_include_depth = 1;
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert_eq!(f.read_names(), vec!["outer.inc"]);
    struct Cancel;
    impl pascal_core::CancellationToken for Cancel {
        fn is_cancelled(&self) -> bool {
            true
        }
    }
    let mut f = Fixture::new(
        "unit T; interface {$I forbidden.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert!(matches!(
        f.analyze_with_cancel(&Cancel),
        Err(pascal_core::ResolverError::Cancelled)
    ));
    assert!(f.read_names().is_empty());
    f.limits.max_work = 0;
    assert!(matches!(
        f.analyze(),
        Err(pascal_core::ResolverError::LimitExceeded {
            limit: "max_work",
            ..
        })
    ));
    assert!(f.read_names().is_empty());
}

#[test]
fn include_preserves_explicit_unknown_layout_and_cross_boundary_option_stack() {
    use pascal_project::ConditionalFact;
    let mut ctx = context(LayoutPlatform::Win32);
    ctx.layout.defaults.long_strings = ConditionalFact::Unknown;
    let mut f = Fixture::new(
        "unit T; interface {$PUSHOPT}{$H+}{$I options.inc} {$IFOPT H+}{$DEFINE RESTORED}{$ENDIF} {$IF SizeOf(String)=4}{$DEFINE WRONG}{$ENDIF} implementation end.",
        ctx,
    );
    f.include("options.inc", "{$H-}{$POPOPT}");
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE RESTORED"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
}

#[test]
fn generic_active_import_uses_included_constant_first_pass() {
    use pascal_core::{
        FilesystemSourceStore, ImportSection, ImportSite, NoCancellation, ResolutionTarget,
        ResolvedUnit, ResolverLimits, UnitResolver,
    };
    use pascal_project::{ProjectContext, ProjectOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    let text = "unit T; interface const N={$I revision.inc}; {$IF N=7} uses Chosen; {$ELSE} uses Forbidden; {$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    std::fs::write(dir.path().join("revision.inc"), "7").unwrap();
    std::fs::write(
        dir.path().join("Chosen.pas"),
        "unit Chosen; interface implementation end.",
    )
    .unwrap();
    let mut ctx =
        ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    ctx.conditional_context = context(LayoutPlatform::Win32);
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    let importer = ResolvedUnit {
        requested_name: "T".into(),
        declared_name: "T".into(),
        source,
    };
    let sites = ["Chosen", "Forbidden"].map(|name| {
        let start = text.find(name).unwrap();
        ImportSite {
            requested_name: name.into(),
            byte_range: start..start + name.len(),
            section: ImportSection::Interface,
        }
    });
    let a = resolver
        .resolve_imports_with_text(&importer, &sites, text, &NoCancellation)
        .unwrap();
    assert!(a.complete);
    assert!(matches!(a.bindings[0].target, ResolutionTarget::Found(_)));
    assert_eq!(a.bindings[1].target, ResolutionTarget::Unavailable);
    assert_eq!(resolver.include_usage().0, 1);
}

#[test]
fn scalar_only_legacy_callback_reports_missing_constant_continuation() {
    use pascal_core::NoCancellation;
    use pascal_core::conditional::{self, ConditionalEnvironment, IncludeTransition};
    let mut env = ConditionalEnvironment::default();
    let a = conditional::analyze_with_include_callback(
        "{$I values.inc}{$IF N=7}{$DEFINE HIT}{$ENDIF}",
        &mut env,
        &NoCancellation,
        &mut |_, env| {
            let child = conditional::analyze_with_include_callback(
                "const N=7;",
                env,
                &NoCancellation,
                &mut |_, _| unreachable!(),
            );
            IncludeTransition {
                complete: child.complete,
                environment_known: child.complete,
            }
        },
    );
    assert!(
        !a.complete,
        "a scalar callback cannot prove shared typed binding/continuation"
    );
    assert_eq!(
        a.directives
            .iter()
            .find(|d| d.body == "DEFINE HIT")
            .unwrap()
            .activity,
        Truth::Unknown
    );
}

#[test]
fn scalar_only_callback_cannot_claim_typed_include_capability() {
    use pascal_core::NoCancellation;
    use pascal_core::conditional::{self, ConditionalEnvironment, IncludeTransition};
    let mut env = ConditionalEnvironment::default();
    let a = conditional::analyze_with_include_callback(
        "{$I types.inc}{$IF true}{$DEFINE HIT}{$ENDIF}",
        &mut env,
        &NoCancellation,
        &mut |_, env| {
            let child = conditional::analyze_with_include_callback(
                "type R=Byte;",
                env,
                &NoCancellation,
                &mut |_, _| unreachable!(),
            );
            IncludeTransition {
                complete: child.complete,
                environment_known: child.complete,
            }
        },
    );
    assert!(
        !a.complete,
        "the legacy callback signature has no source-backed typed include capability"
    );
}

#[test]
fn generic_child_limits_retain_failed_lookup_observations_and_warnings() {
    use pascal_core::{
        FilesystemSourceStore, NoCancellation, Resolution, ResolutionObservation, ResolverLimits,
        UnitResolver,
    };
    use pascal_project::{ProjectContext, ProjectOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    let text = "unit T; interface const N=1+{$I value.inc}; {$IF N=1}{$DEFINE WRONG}{$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    std::fs::write(dir.path().join("value.inc"), "7777777").unwrap();
    for limits in [
        ResolverLimits {
            max_source_bytes: 4,
            ..Default::default()
        },
        ResolverLimits {
            max_include_bytes: 4,
            ..Default::default()
        },
        ResolverLimits {
            max_include_files: 0,
            ..Default::default()
        },
    ] {
        let mut ctx =
            ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
                .unwrap();
        ctx.conditional_context = context(LayoutPlatform::Win32);
        let mut resolver = UnitResolver::new(
            ctx,
            vec![dir.path().into()],
            FilesystemSourceStore::new(),
            limits,
        );
        let mut source = support::loaded("T.pas", text);
        source.path = path.clone();
        let a = resolver
            .analyze_compile_time(&source, &NoCancellation)
            .unwrap();
        assert!(!a.root.complete);
        assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
        assert!(matches!(
            a.includes[0].target,
            Resolution::Incomplete { .. }
        ));
        assert!(!a.observations.is_empty());
        assert!(!a.warnings.is_empty());
        assert!(
            !a.observations
                .iter()
                .any(|o| matches!(o, ResolutionObservation::Payload { .. }))
        );
    }
}

#[test]
fn latin1_child_retains_raw_revision_and_physical_text_coordinates() {
    use pascal_core::{FilesystemSourceStore, NoCancellation, UnitResolver};
    use pascal_project::{ProjectContext, ProjectOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    let text = "unit T; interface const N={$I latin1.inc}; {$IF N=7}{$DEFINE HIT}{$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    std::fs::write(
        dir.path().join("latin1.inc"),
        [b'7', b' ', b'{', 0xe9, b'}'],
    )
    .unwrap();
    let mut ctx =
        ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default()).unwrap();
    ctx.conditional_context = context(LayoutPlatform::Win32);
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        Default::default(),
    );
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    let a = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    let child = &a.occurrences[1];
    assert_eq!(&*child.source.bytes, &[b'7', b' ', b'{', 0xe9, b'}']);
    assert_eq!(child.source.decoded_text.as_deref(), Some("7 {é}"));
}

#[test]
fn successful_and_failed_include_warnings_are_transported() {
    for found in [true, false] {
        let mut f = Fixture::new(
            "unit T; interface const N={$I value.inc}; implementation end.",
            context(LayoutPlatform::Win32),
        );
        if found {
            f.include("value.inc", "7");
        } else {
            f.deny_include("value.inc");
        }
        f.include_warnings.push("resource warning".into());
        let a = f.analyze().unwrap();
        assert_eq!(a.root.complete, found);
        assert_eq!(a.warnings, vec!["resource warning"]);
    }
}

#[test]
fn nested_occurrences_share_definition_and_byte_work_limits() {
    let mut f = Fixture::new(
        "unit T; interface {$I types.inc}{$I types.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("types.inc", "type R=Byte;");
    f.limits.max_definitions = 1;
    let a = f.analyze().unwrap();
    assert!(!a.root.complete);
    assert!(a.reasons.iter().any(|r| matches!(
        r,
        pascal_core::compile_time::IncompleteReason::Limit {
            name: "max_definitions",
            maximum: 1
        }
    )));
    assert_eq!(f.read_names(), vec!["types.inc", "types.inc"]);
    let comment = format!("{{{}}}", "x".repeat(2048));
    let mut one = Fixture::new(
        "unit T; interface {$I body.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    one.include("body.inc", &comment);
    one.limits.max_byte_work = 10000;
    assert!(one.analyze().unwrap().root.complete);
    let mut many = Fixture::new(
        "unit T; interface {$I body.inc}{$I body.inc}{$I body.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    many.include("body.inc", &comment);
    many.limits.max_byte_work = 10000;
    assert!(matches!(
        many.analyze(),
        Err(pascal_core::ResolverError::LimitExceeded {
            limit: "max_byte_work",
            ..
        })
    ));
}

#[test]
fn cancellation_in_child_cannot_authorize_following_lookup() {
    struct CancelAfter(std::cell::Cell<usize>);
    impl pascal_core::CancellationToken for CancelAfter {
        fn is_cancelled(&self) -> bool {
            let n = self.0.get() + 1;
            self.0.set(n);
            n >= 200
        }
    }
    let mut f = Fixture::new(
        "unit T; interface {$I first.inc}{$I forbidden.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("first.inc", &format!("{{{}}}", "x".repeat(1000)));
    f.deny_include("forbidden.inc");
    assert!(matches!(
        f.analyze_with_cancel(&CancelAfter(std::cell::Cell::new(0))),
        Err(pascal_core::ResolverError::Cancelled)
    ));
    assert_eq!(f.read_names(), vec!["first.inc"]);
}

#[test]
fn record_scope_and_name_tokens_continue_through_child_eof() {
    for (prefix, child, suffix, operand, size) in [
        ("type R=packed record A:", "Byte; B: Byte; end", ";", "R", 2),
        ("type Na", "me", "=Byte;", "Name", 1),
    ] {
        let text = format!(
            "unit T; interface {prefix}{{$I fragment.inc}}{suffix} {{$IF SizeOf({operand})={size}}}{{$I chosen.inc}}{{$ELSE}}{{$I forbidden.inc}}{{$ENDIF}} implementation end."
        );
        let mut f = Fixture::new(&text, context(LayoutPlatform::Win32));
        f.include("fragment.inc", child);
        f.include("chosen.inc", "{chosen}");
        f.deny_include("forbidden.inc");
        let a = f.analyze().unwrap();
        assert!(a.root.complete, "{operand}: {:?}", a.reasons);
        assert_eq!(f.read_names(), vec!["fragment.inc", "chosen.inc"]);
    }
}
