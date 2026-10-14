#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::*;
use pascal_core::*;
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

struct Extra {
    fixture: Fixture,
    bytes: usize,
    held: Option<AdmittedOwned<Vec<u8>>>,
}
impl CompileTimeResolver for Extra {
    fn unit_report_capability(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError> {
        if self.bytes != 0 && self.held.is_none() {
            self.held = Some(
                budget
                    .admit_owned(1, 0, self.bytes, || vec![0; self.bytes])
                    .unwrap(),
            );
        }
        self.fixture.unit_report_capability(request, budget, cancel)
    }
    fn include(
        &mut self,
        r: IncludeResolveRequest<'_>,
        b: &mut CompileTimeBudget<'_>,
        c: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
        CompileTimeResolver::include(&mut self.fixture, r, b, c)
    }
    fn unit(
        &mut self,
        r: UnitResolveRequest<'_>,
        b: &mut CompileTimeBudget<'_>,
        c: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
        CompileTimeResolver::unit(&mut self.fixture, r, b, c)
    }
}

#[test]
fn callback_owner_cannot_erase_live_declaration_invoices() {
    let declarations = (0..150)
        .map(|n| format!("Keep{n}=Byte; "))
        .collect::<String>();
    let text = format!(
        "unit Main; interface uses Other; type {declarations} Local=record A:Byte; end; {{$IF SizeOf(Other.T)=8}}{{$DEFINE HIT}}{{$ENDIF}} implementation end."
    );
    let mut samples = Vec::new();
    for bytes in [0, 100_000] {
        let mut fixture = Fixture::new(&text, context(LayoutPlatform::Win32));
        fixture.unit(
            "Other",
            "unit Other; interface type T=Int64; implementation end.",
            context(LayoutPlatform::Win32),
        );
        let root = fixture.root.clone();
        let context = fixture.context.clone();
        let mut resolver = Extra {
            fixture,
            bytes,
            held: None,
        };
        let analysis = analyze_source(
            &root,
            &context,
            &mut resolver,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
        .unwrap();
        assert_eq!(
            activity(&analysis, "DEFINE HIT"),
            pascal_core::conditional::Truth::True
        );
        let mut payload = 0;
        analysis
            .facts
            .visit_recovery_payload(&mut |n| {
                payload += n;
                Ok(())
            })
            .unwrap();
        let before = analysis.budget.budget_snapshot();
        eprintln!("callback bytes={bytes}: usage={before:?}, facts recovery payload={payload}");
        if let Some(owner) = resolver.held.take() {
            assert_eq!(owner.len(), bytes);
            drop(owner);
            let after = analysis.budget.budget_snapshot();
            assert!(before.retained_bytes - after.retained_bytes >= bytes);
            assert_eq!(before.work, after.work);
            assert_eq!(before.byte_work, after.byte_work);
        }
        samples.push((before, payload));
    }
    assert_eq!(samples[0].1, samples[1].1);
    assert!(samples[0].1 > 70_000);
    assert!(
        samples[1].0.retained_bytes >= samples[0].0.retained_bytes + 100_000,
        "adding a live owner erased invoices: {:?}",
        samples
    );
}
