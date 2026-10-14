#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_core::{NoCancellation, ResolverError};
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

fn edit_context<R>(
    context: &mut pascal_project::ProjectContext,
    edit: impl FnOnce(&mut pascal_project::ProjectContextParts) -> R,
) {
    let mut parts = std::mem::take(context).into_parts();
    edit(&mut parts);
    *context = pascal_project::ProjectContext::from_parts(parts);
}

#[test]
fn task13_depth_boundaries_are_safe_in_isolated_default_stack_processes() {
    for case in [
        "alias256",
        "alias257",
        "fields256",
        "fields257",
        "explicit4",
        "explicit5",
        "include256",
        "include257",
        "include4",
        "include5",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "task13_depth_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("L4D_TASK13_DEPTH_CASE", case)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("{case}: {stdout}{stderr}");
        assert!(
            output.status.success(),
            "{case}: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains("TASK13_BOUNDARY"),
            "child must execute, not skip"
        );
    }
}

#[test]
fn task13_depth_child() {
    let Ok(case) = std::env::var("L4D_TASK13_DEPTH_CASE") else {
        return;
    };
    if case.starts_with("include") {
        task13_include_boundary(&case);
        return;
    }
    let (depth, explicit) = match case.as_str() {
        "alias256" | "fields256" => (256, false),
        "alias257" | "fields257" => (257, false),
        "explicit4" => (4, true),
        "explicit5" => (5, true),
        _ => panic!("unexpected case"),
    };
    let source = if case.starts_with("fields") {
        format!(
            "unit T; interface {{$A8}} type Deep={}Byte{}; {{$IF SizeOf(Deep)=1}}{{$DEFINE HIT}}{{$ENDIF}} implementation end.",
            "record F:".repeat(depth),
            "; end".repeat(depth)
        )
    } else {
        let mut text = String::from("unit T; interface type A0=Byte;");
        for i in 1..=depth {
            text.push_str(&format!("A{i}=A{};", i - 1));
        }
        text.push_str(&format!(
            "{{$IF SizeOf(A{depth})=1}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
        ));
        text
    };
    let mut fixture = Fixture::new(&source, context(LayoutPlatform::Win32));
    if explicit {
        fixture.limits.max_type_depth = 4;
    }
    assert_eq!(CompileTimeLimits::default().max_type_depth, 256);
    match fixture.analyze() {
        Ok(analysis) => {
            let counters = analysis.budget.budget_snapshot();
            println!(
                "TASK13_BOUNDARY {case} complete={} activity={:?} reasons={:?} counters={counters:?}",
                analysis.root.complete,
                activity(&analysis, "DEFINE HIT"),
                analysis.reasons
            );
            assert!(counters.work <= fixture.limits.max_work);
            assert!(counters.byte_work <= fixture.limits.max_byte_work);
            assert!(counters.retained_bytes <= fixture.limits.max_retained_bytes);
            if case == "alias256" {
                assert!(analysis.root.complete);
                assert_eq!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
            if case == "alias257" {
                assert!(!analysis.root.complete);
                assert_ne!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
            if explicit && depth == 4 {
                assert_eq!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
            if explicit && depth > 4 {
                assert_ne!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
            if !analysis.root.complete {
                assert_ne!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
        }
        Err(ResolverError::LimitExceeded { limit, maximum, .. }) => {
            println!("TASK13_BOUNDARY {case} refused={limit} maximum={maximum}");
            assert!(
                [
                    "max_type_depth",
                    "max_scope_depth",
                    "max_work",
                    "max_byte_work",
                    "max_retained_bytes"
                ]
                .contains(&limit)
            );
        }
        Err(error) => panic!("{case}: {error:?}"),
    }
}

fn task13_include_boundary(case: &str) {
    use pascal_core::{FilesystemSourceStore, ResolverLimits, UnitResolver};
    use pascal_project::{ProjectContext, ProjectOptions};
    let depth = case.trim_start_matches("include").parse::<usize>().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let path = root.join("T.pas");
    let text =
        "unit T; interface {$I f1.inc} {$IF Final=7}{$DEFINE HIT}{$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    for i in 1..=depth {
        let body = if i == depth {
            "const Final=7;".into()
        } else {
            format!("{{$I f{}.inc}}", i + 1)
        };
        std::fs::write(root.join(format!("f{i}.inc")), body).unwrap();
    }
    let mut ctx = ProjectContext::discover(&path, &[root.into()], &ProjectOptions::default())
        .unwrap()
        .context;
    edit_context(&mut ctx, |p| {
        p.conditional_context = context(LayoutPlatform::Win32)
    });
    let mut limits = ResolverLimits::default();
    assert_eq!(limits.max_include_depth, 256);
    if depth <= 5 {
        limits.max_include_depth = 4;
    }
    let mut resolver =
        UnitResolver::new(ctx, vec![root.into()], FilesystemSourceStore::new(), limits);
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    match resolver.analyze_compile_time(&source, &NoCancellation) {
        Ok(analysis) => {
            let counters = analysis.budget.budget_snapshot();
            println!(
                "TASK13_BOUNDARY {case} complete={} activity={:?} reasons={:?} counters={counters:?} include_usage={:?}",
                analysis.root.complete,
                activity(&analysis, "DEFINE HIT"),
                analysis.reasons,
                resolver.include_usage()
            );
            if depth == 4 {
                assert_eq!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
            if depth == 5 || !analysis.root.complete {
                assert_ne!(activity(&analysis, "DEFINE HIT"), Truth::True);
            }
            assert!(counters.work <= CompileTimeLimits::default().max_work);
            assert!(counters.byte_work <= CompileTimeLimits::default().max_byte_work);
        }
        Err(ResolverError::LimitExceeded { limit, maximum, .. }) => println!(
            "TASK13_BOUNDARY {case} refused={limit} maximum={maximum} include_usage={:?}",
            resolver.include_usage()
        ),
        Err(error) => panic!("{case}: {error:?}"),
    }
}

#[test]
fn task13_ten_thousand_declarations_and_many_const_parameters_obey_defaults() {
    let mut source = String::from("unit T; interface const ");
    for i in 0..10_000 {
        source.push_str(&format!("C{i}={i};"));
    }
    source.push_str("{$IF SizeOf(Byte)=1}{$DEFINE HIT}{$ENDIF} implementation end.");
    let mut fixture = Fixture::new(&source, context(LayoutPlatform::Win32));
    let result = fixture.analyze();
    match result {
        Ok(analysis) => {
            let counters = analysis.budget.budget_snapshot();
            eprintln!("10000 declarations: {counters:?}, {:?}", analysis.reasons);
            assert!(counters.work <= fixture.limits.max_work);
            assert!(counters.byte_work <= fixture.limits.max_byte_work);
            assert!(counters.retained_bytes <= fixture.limits.max_retained_bytes);
        }
        Err(ResolverError::LimitExceeded { limit, maximum, .. }) => {
            eprintln!("10000 declarations refused: {limit}={maximum}")
        }
        Err(error) => panic!("{error:?}"),
    }
    let parameters = (0..100)
        .map(|i| format!("const P{i}:Byte"))
        .collect::<Vec<_>>()
        .join(";");
    let source = format!(
        "unit T; interface implementation procedure Run({parameters}); begin {{$IF SizeOf(P99)=1}}{{$DEFINE HIT}}{{$ENDIF}} end; end."
    );
    let mut fixture = Fixture::new(&source, context(LayoutPlatform::Win32));
    let analysis = fixture.analyze().unwrap();
    assert_eq!(activity(&analysis, "DEFINE HIT"), Truth::True);
    let counters = analysis.budget.budget_snapshot();
    eprintln!("100 const parameters: {counters:?}");
    assert!(counters.work < CompileTimeLimits::default().max_work);
    assert!(counters.byte_work < CompileTimeLimits::default().max_byte_work);
}

#[test]
fn task13_large_operand_admission_is_transactional_and_work_never_refunds() {
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    budget.charge(7, 11, 13).unwrap();
    let before = budget.snapshot();
    let operand = "n".repeat(4097);
    assert!(matches!(
        parse_sizeof_operand(&operand, &mut budget),
        Fact::Incomplete(_)
    ));
    assert_eq!(budget.snapshot(), before);
    budget.release(13);
    assert_eq!(budget.snapshot().work, before.work);
    assert_eq!(budget.snapshot().byte_work, before.byte_work);
    let mut fixture = Fixture::new(
        &format!(
            "unit T; interface const S='{}'; {{$IF SizeOf(Byte)=1}}{{$DEFINE HIT}}{{$ENDIF}} implementation end.",
            "x".repeat(1_048_577)
        ),
        context(LayoutPlatform::Win32),
    );
    assert!(matches!(
        fixture.analyze(),
        Err(ResolverError::LimitExceeded { .. })
    ));
}

#[test]
fn task13_cancellation_during_late_operand_queries_never_returns_partial_facts() {
    struct Polls {
        count: std::cell::Cell<usize>,
        stop: usize,
    }
    impl pascal_core::CancellationToken for Polls {
        fn is_cancelled(&self) -> bool {
            let next = self.count.get() + 1;
            self.count.set(next);
            next >= self.stop
        }
    }
    let mut source = String::from("unit T; interface {$I first.inc} const ");
    for i in 0..40 {
        source.push_str(&format!("N{i}=SizeOf ( Types.T );"));
    }
    source.push_str("{$IF N39=4}{$I selected.inc}{$ENDIF} implementation end.");
    let make_fixture = || {
        let mut fixture = Fixture::new(&source, context(LayoutPlatform::Win32));
        fixture.include("first.inc", "uses Types;");
        fixture.include("selected.inc", "const Selected=1;");
        fixture.unit(
            "Types",
            "unit Types; interface type T=LongWord; implementation end.",
            context(LayoutPlatform::Win32),
        );
        fixture
    };
    let baseline = Polls {
        count: std::cell::Cell::new(0),
        stop: usize::MAX,
    };
    let output = make_fixture().analyze_with_cancel(&baseline).unwrap();
    assert!(output.root.complete);
    assert_eq!(output.includes.len(), 2);
    let total = baseline.count.get();
    eprintln!(
        "40-query cancellation baseline polls={total} counters={:?}",
        output.budget.budget_snapshot()
    );
    for stop in [total / 2, total * 3 / 4, total - 10, total - 1] {
        let cancel = Polls {
            count: std::cell::Cell::new(0),
            stop,
        };
        let mut fixture = make_fixture();
        assert!(
            matches!(
                fixture.analyze_with_cancel(&cancel),
                Err(ResolverError::Cancelled)
            ),
            "stop={stop}"
        );
        assert!(cancel.count.get() >= stop);
    }
}

#[test]
fn task13_provider_limits_are_shared_across_queries() {
    let mut fixture = Fixture::new(
        "unit T; interface uses A, B; {$IF SizeOf(A.T)=1}{$DEFINE FIRST}{$ENDIF} {$IF SizeOf(B.T)=1}{$DEFINE SECOND}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    for name in ["A", "B"] {
        fixture.unit(
            name,
            &format!("unit {name}; interface type T=Byte; implementation end."),
            context(LayoutPlatform::Win32),
        );
    }
    fixture.limits.max_provider_queries = 1;
    let result = fixture.analyze();
    eprintln!("provider limit result={result:?}");
    assert!(
        matches!(result, Err(ResolverError::InvalidRequest(ref reason)) if reason.contains("max_provider_queries") && reason.contains("maximum: 1"))
    );
    assert_eq!(fixture.unit_requests.len(), 1);
    assert!(!fixture.read_names().contains(&"B".into()));
}

#[test]
fn task13_pending_initializer_fragment_copy_cost_is_bounded_and_monotonic() {
    let mut previous = None;
    for count in [1, 4, 8] {
        let source = format!(
            "unit T; interface const Width=SizeOf ( {} Byte ); {{$IF Width=1}}{{$DEFINE HIT}}{{$ENDIF}} implementation end.",
            "{$I space.inc}".repeat(count)
        );
        let mut fixture = Fixture::new(&source, context(LayoutPlatform::Win32));
        fixture.include("space.inc", " ");
        let analysis = fixture.analyze().unwrap();
        assert!(analysis.root.complete, "{:?}", analysis.reasons);
        assert_eq!(activity(&analysis, "DEFINE HIT"), Truth::True);
        assert_eq!(analysis.occurrences.len(), count + 1);
        let counters = analysis.budget.budget_snapshot();
        eprintln!("pending count={count}: {counters:?}");
        assert!(counters.work < 50_000);
        assert!(counters.byte_work < 256_000);
        if let Some((work, bytes)) = previous {
            assert!(counters.work > work);
            assert!(counters.byte_work > bytes);
        }
        previous = Some((counters.work, counters.byte_work));
    }
}

#[test]
fn task13_failed_include_probes_recover_without_resetting_work_or_publishing_partial_initializer() {
    use pascal_core::{FilesystemSourceStore, ResolverLimits, UnitResolver};
    use pascal_project::{ProjectContext, ProjectOptions};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let path = root.join("T.pas");
    let text = "unit T; interface const Width=SizeOf({$I operand.inc}); {$IF Width=1}{$DEFINE HIT}{$ENDIF} implementation end.";
    std::fs::write(&path, text).unwrap();
    let mut ctx = ProjectContext::discover(&path, &[root.into()], &ProjectOptions::default())
        .unwrap()
        .context;
    edit_context(&mut ctx, |p| {
        p.conditional_context = context(LayoutPlatform::Win32)
    });
    let mut resolver = UnitResolver::new(
        ctx,
        vec![root.into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    );
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    let missing = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(!missing.root.complete);
    assert_ne!(activity(&missing, "DEFINE HIT"), Truth::True);
    assert!(!missing.observations.is_empty());
    let first = resolver.compile_time_usage();
    drop(missing);
    let repeated = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(!repeated.root.complete);
    let second = resolver.compile_time_usage();
    assert!(second.work > first.work);
    assert!(second.byte_work > first.byte_work);
    drop(repeated);
    std::fs::write(root.join("operand.inc"), "Byte").unwrap();
    let recovered = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    assert!(recovered.root.complete, "{:?}", recovered.reasons);
    assert_eq!(activity(&recovered, "DEFINE HIT"), Truth::True);
    let third = resolver.compile_time_usage();
    eprintln!("failed/cached/recovered probes: {first:?} / {second:?} / {third:?}");
    assert!(third.work > second.work);
    assert!(third.byte_work > second.byte_work);
    assert!(third.work < CompileTimeLimits::default().max_work);
    assert!(third.byte_work < CompileTimeLimits::default().max_byte_work);
}
