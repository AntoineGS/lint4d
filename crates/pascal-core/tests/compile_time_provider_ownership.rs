#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_core::{NoCancellation, Resolution, UnitResolveRequest};
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

#[test]
fn actual_provider_callback_unwind_refunds_dead_compilation_not_live_cache() {
    struct PanickingProvider(Fixture);
    impl CompileTimeResolver for PanickingProvider {
        fn unit_report_capability(
            &mut self,
            request: UnitResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<
            pascal_core::AdmittedCapabilityOutcome<AdmittedProviderSource>,
            pascal_core::ResolverError,
        > {
            let _returned = self.0.unit_report_capability(request, budget, cancel)?;
            panic!("actual callback unwind after admitted return/cache allocation");
        }
        fn include(
            &mut self,
            request: pascal_core::IncludeResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<
            pascal_core::ResolutionOutcome<pascal_core::LoadedSource>,
            pascal_core::ResolverError,
        > {
            CompileTimeResolver::include(&mut self.0, request, budget, cancel)
        }
        fn unit(
            &mut self,
            request: UnitResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<pascal_core::ResolutionOutcome<ProviderSource>, pascal_core::ResolverError>
        {
            CompileTimeResolver::unit(&mut self.0, request, budget, cancel)
        }
        fn unit_capability(
            &mut self,
            request: UnitResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::CancellationToken,
        ) -> Result<
            pascal_core::ResolutionOutcome<AdmittedProviderSource>,
            pascal_core::ResolverError,
        > {
            let _returned = self.0.unit_capability(request, budget, cancel)?;
            panic!("actual callback unwind after admitted return/cache allocation");
        }
    }
    fn fixture() -> Fixture {
        let mut f = Fixture::new(
            "unit Main; interface uses P; {$IF SizeOf(P.T)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "P",
            "unit P; interface type T=Int64; implementation end.",
            context(LayoutPlatform::Win32),
        );
        f
    }
    let mut baseline = fixture();
    let root = baseline.root.clone();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let returned = baseline
        .unit_capability(
            UnitResolveRequest {
                requested_name: "P",
                importer_path: &root.path,
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
    drop(returned);
    drop(budget);
    let expected_cache = baseline.cached_unit_usage().unwrap().retained_bytes;
    let mut resolver = PanickingProvider(fixture());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        analyze_source(
            &root,
            &context(LayoutPlatform::Win32),
            &mut resolver,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
    }));
    assert!(result.is_err());
    assert_eq!(
        resolver.0.cached_unit_usage().unwrap().retained_bytes,
        expected_cache,
        "unwound root compilation is dead; only actual callback cache/log owners remain"
    );
}

#[test]
fn actual_provider_pending_drop_does_not_refund_resolver_cache_owner() {
    let mut f = Fixture::new(
        "unit Main; interface uses Used, Other; {$IF SizeOf(Used.T)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Used",
        "unit Used; interface type T=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface type T=Byte; implementation {$I unrelated.inc} end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
    assert!(!f.read_names().iter().any(|n| n == "unrelated.inc"));
    let before = a.budget.budget_snapshot();
    drop(a);
    let after = f.cached_unit_usage().unwrap();
    assert!(after.retained_bytes > 0, "cache still owns source backing");
    assert!(
        after.retained_bytes < before.retained_bytes,
        "dead output and pending carrier refund"
    );
    assert_eq!(after.work, before.work);
    assert_eq!(after.byte_work, before.byte_work);
}

#[test]
fn actual_callback_capability_is_same_root_and_dropped_return_refunds_only_owned_copy() {
    let mut f = Fixture::new(
        "unit Main; interface implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "P",
        "unit P; interface type T=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let root = f.root.clone();
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    let result = CompileTimeResolver::unit_capability(
        &mut f,
        UnitResolveRequest {
            requested_name: "P",
            importer_path: &root.path,
            legacy_route: None,
        },
        &mut budget,
        &NoCancellation,
    )
    .unwrap();
    let Resolution::Found(provider) = result.result else {
        panic!("real provider capability required");
    };
    assert!(provider.is_admitted_by(&budget));
    let other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    assert!(!provider.is_admitted_by(&other));
    let live = budget.snapshot();
    drop(provider);
    assert!(budget.snapshot().retained_bytes < live.retained_bytes);
    assert_eq!(f.cached_unit_usage().unwrap(), budget.snapshot());
    assert_eq!(budget.snapshot().work, live.work);
}

#[test]
fn actual_interface_graph_snapshot_shares_backing_and_output_outlives_budget() {
    let mut f = Fixture::new(
        "unit Main; interface uses P; {$IF SizeOf(P.T)=8}{$DEFINE FIRST}{$ENDIF} {$IF SizeOf(P.T)=8}{$DEFINE SECOND}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "P",
        "unit P; interface type T=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE FIRST"), Truth::True);
    assert_eq!(activity(&a, "DEFINE SECOND"), Truth::True);
    let mut copy_budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
    // A different root cannot adopt the existing graph as admitted data.
    assert!(a.facts.try_clone(&mut copy_budget).is_err());
    let live = a.budget.budget_snapshot();
    assert!(live.retained_bytes > 0);
    assert_eq!(
        f.read_names().iter().filter(|n| n.as_str() == "P").count(),
        1
    );
    let ledger = a.budget.clone(); // test-only observer of the already admitted view
    drop(a);
    assert!(ledger.budget_snapshot().retained_bytes < live.retained_bytes);
    drop(f);
    assert!(
        ledger.budget_snapshot().retained_bytes < 512,
        "only the ledger/view remains"
    );
}
