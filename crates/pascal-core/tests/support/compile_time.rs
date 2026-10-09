#![allow(dead_code)]
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_core::{
    CancellationToken, IncludeResolveRequest, Resolution, ResolutionOutcome, ResolverError,
    UnitResolveRequest,
};
use pascal_core::{LoadedSource, NoCancellation, SourceId, SourceRevision};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, LayoutPlatform,
    LayoutSettings,
};

pub fn context(platform: LayoutPlatform) -> ConditionalContext {
    ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(37, 0))
        .with_layout(LayoutContext {
            platform: Some(platform),
            defaults: LayoutSettings {
                long_strings: ConditionalFact::True,
                real_compatibility: ConditionalFact::False,
                ..LayoutSettings::default()
            },
        })
}

pub struct NoResources;
impl CompileTimeResolver for NoResources {
    fn include(
        &mut self,
        _: IncludeResolveRequest<'_>,
        _: &mut CompileTimeBudget<'_>,
        _: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
        Ok(ResolutionOutcome {
            result: Resolution::Unavailable {
                reason: "no resource capability".into(),
            },
            observations: vec![],
            warnings: vec![],
        })
    }
    fn unit(
        &mut self,
        _: UnitResolveRequest<'_>,
        _: &mut CompileTimeBudget<'_>,
        _: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
        Ok(ResolutionOutcome {
            result: Resolution::Unavailable {
                reason: "no resource capability".into(),
            },
            observations: vec![],
            warnings: vec![],
        })
    }
}

pub struct Fixture {
    pub root: LoadedSource,
    pub context: ConditionalContext,
    pub limits: CompileTimeLimits,
    pub max_include_depth: usize,
    pub requests: Vec<(std::path::PathBuf, std::ops::Range<usize>, String)>,
    pub include_warnings: Vec<String>,
    pub unit_requests: Vec<(std::path::PathBuf, String)>,
    includes: std::collections::BTreeMap<String, LoadedSource>,
    failures: std::collections::BTreeMap<String, Resolution<LoadedSource>>,
    denied: std::collections::BTreeSet<String>,
    reads: Vec<String>,
    units: std::collections::BTreeMap<String, ProviderSource>,
    denied_units: std::collections::BTreeSet<String>,
    ambiguous_units: std::collections::BTreeSet<String>,
    cached_unit: Option<AdmittedSourceBacking>,
    callback_logs: Vec<AdmittedOwned<()>>,
}
impl Fixture {
    pub fn new(text: &str, context: ConditionalContext) -> Self {
        Self {
            root: loaded("T.pas", text),
            context,
            limits: CompileTimeLimits::default(),
            max_include_depth: 64,
            requests: vec![],
            include_warnings: vec![],
            unit_requests: vec![],
            includes: Default::default(),
            failures: Default::default(),
            denied: Default::default(),
            reads: vec![],
            units: Default::default(),
            denied_units: Default::default(),
            ambiguous_units: Default::default(),
            cached_unit: None,
            callback_logs: Vec::new(),
        }
    }
    pub fn include(&mut self, name: &str, text: &str) {
        self.includes.insert(name.into(), loaded(name, text));
    }
    pub fn unit(&mut self, name: &str, text: &str, context: ConditionalContext) {
        self.units.insert(
            name.into(),
            ProviderSource::new(
                pascal_core::ResolvedUnit {
                    requested_name: name.into(),
                    declared_name: name.into(),
                    source: loaded(&format!("{name}.pas"), text),
                },
                context,
                vec![name.into()],
            ),
        );
    }
    pub fn unit_qualifiers(&mut self, name: &str, qualifiers: &[&str]) {
        self.units
            .get_mut(name)
            .expect("fixture unit")
            .authorized_qualifiers = qualifiers.iter().map(|q| (*q).into()).collect();
    }
    pub fn unit_revision(&mut self, name: &str, version: i32) {
        let source = &mut self.units.get_mut(name).expect("fixture unit").unit.source;
        source.revision = SourceRevision::Overlay {
            version,
            content_hash: pascal_project::content_hash_bytes(&source.bytes),
        };
    }
    pub fn deny_unit(&mut self, name: &str) {
        self.denied_units.insert(name.into());
    }
    pub fn ambiguous_unit(&mut self, name: &str) {
        self.ambiguous_units.insert(name.into());
    }
    pub fn deny_include(&mut self, name: &str) {
        self.denied.insert(name.into());
    }
    pub fn fail_include(&mut self, name: &str, failure: Resolution<LoadedSource>) {
        self.failures.insert(name.into(), failure);
    }
    pub fn read_names(&self) -> Vec<String> {
        self.reads.clone()
    }
    pub fn cached_unit_usage(&self) -> Option<BudgetSnapshot> {
        self.cached_unit.as_ref().map(|s| s.budget_snapshot())
    }
    pub fn analyze(&mut self) -> Result<CompileTimeAnalysis, ResolverError> {
        self.analyze_with_cancel(&NoCancellation)
    }
    pub fn analyze_with_cancel(
        &mut self,
        cancel: &dyn CancellationToken,
    ) -> Result<CompileTimeAnalysis, ResolverError> {
        let root = self.root.clone();
        let context = self.context.clone();
        let limits = self.limits;
        analyze_source(&root, &context, self, limits, cancel)
    }
}
impl CompileTimeResolver for Fixture {
    fn unit_capability(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<AdmittedProviderSource>, ResolverError> {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        let log_bytes = request.importer_path.as_os_str().len()
            + request.requested_name.len() * 2
            + std::mem::size_of::<(std::path::PathBuf, String)>()
            + std::mem::size_of::<String>()
            + std::mem::size_of::<AdmittedOwned<()>>();
        let log = budget
            .admit_owned(1, log_bytes, log_bytes, || ())
            .map_err(|_| ResolverError::InvalidRequest("fixture callback log admission".into()))?;
        self.callback_logs.reserve_exact(1);
        self.callback_logs.push(log);
        self.unit_requests.reserve_exact(1);
        self.unit_requests
            .push((request.importer_path.into(), request.requested_name.into()));
        let result = if self.denied_units.contains(request.requested_name) {
            Resolution::Unavailable {
                reason: "denied fixture unit".into(),
            }
        } else if self.ambiguous_units.contains(request.requested_name) {
            Resolution::Ambiguous { candidates: vec![] }
        } else if let Some(provider) = self.units.get(request.requested_name) {
            let reusable = self.cached_unit.as_ref().is_some_and(|s| {
                s.is_admitted_by(budget)
                    && s.id == provider.unit.source.id
                    && s.revision == provider.unit.source.revision
            });
            if !reusable {
                self.cached_unit = Some(ProviderSource::admit_cached_backing(
                    &provider.unit.source,
                    budget,
                )?);
            }
            let capability = provider.admit_copy(self.cached_unit.as_ref().unwrap(), budget)?;
            self.reads.reserve_exact(1);
            self.reads.push(request.requested_name.into());
            Resolution::Found(capability)
        } else {
            Resolution::Unavailable {
                reason: "missing fixture unit".into(),
            }
        };
        Ok(ResolutionOutcome {
            result,
            observations: vec![],
            warnings: vec![],
        })
    }
    fn max_include_depth(&self) -> usize {
        self.max_include_depth
    }
    fn include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        _: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
        budget
            .charge(
                1,
                request.requested_name.len(),
                request.requested_name.len(),
            )
            .map_err(|_| ResolverError::InvalidRequest("fixture budget".into()))?;
        self.reads.push(request.requested_name.into());
        self.requests.push((
            request.including_path.into(),
            request.byte_range.clone(),
            request.requested_name.into(),
        ));
        let result = if let Some(failure) = self.failures.get(request.requested_name) {
            failure.clone()
        } else if self.denied.contains(request.requested_name) {
            Resolution::Unavailable {
                reason: "denied fixture include".into(),
            }
        } else {
            self.includes
                .get(request.requested_name)
                .cloned()
                .map(Resolution::Found)
                .unwrap_or_else(|| Resolution::Unavailable {
                    reason: "missing fixture include".into(),
                })
        };
        Ok(ResolutionOutcome {
            result,
            observations: vec![],
            warnings: {
                let n = self.include_warnings.iter().map(String::len).sum();
                budget
                    .charge(1, n, n)
                    .map_err(|_| ResolverError::InvalidRequest("fixture warning budget".into()))?;
                self.include_warnings.clone()
            },
        })
    }
    fn unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        budget
            .charge(
                1,
                request.importer_path.as_os_str().len() + request.requested_name.len(),
                std::mem::size_of::<(std::path::PathBuf, String)>()
                    + request.importer_path.as_os_str().len()
                    + request.requested_name.len(),
            )
            .map_err(|_| ResolverError::InvalidRequest("fixture unit origin budget".into()))?;
        self.unit_requests
            .push((request.importer_path.into(), request.requested_name.into()));
        budget
            .charge(1, request.requested_name.len(), 0)
            .map_err(|_| ResolverError::InvalidRequest("fixture unit budget".into()))?;
        let result = if self.denied_units.contains(request.requested_name) {
            Resolution::Unavailable {
                reason: "denied fixture unit".into(),
            }
        } else if self.ambiguous_units.contains(request.requested_name) {
            Resolution::Ambiguous { candidates: vec![] }
        } else if let Some(provider) = self.units.get(request.requested_name) {
            let mut failed = false;
            provider
                .context
                .visit_recovery_payload(&mut |n| {
                    budget.charge(1, n, n).map_err(|_| {
                        failed = true;
                        "fixture context budget".into()
                    })
                })
                .map_err(|_| ResolverError::InvalidRequest("fixture context budget".into()))?;
            let _ = failed;
            budget
                .charge(
                    1,
                    provider.unit.source.bytes.len(),
                    provider.unit.source.bytes.len() * 4,
                )
                .map_err(|_| ResolverError::InvalidRequest("fixture provider budget".into()))?;
            self.reads.push(request.requested_name.into());
            Resolution::Found(provider.clone())
        } else {
            Resolution::Unavailable {
                reason: "missing fixture unit".into(),
            }
        };
        Ok(ResolutionOutcome {
            result,
            observations: vec![],
            warnings: vec![],
        })
    }
}

pub fn activity(analysis: &CompileTimeAnalysis, body: &str) -> Truth {
    let matches = analysis
        .root
        .directives
        .iter()
        .filter(|d| d.body == body)
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "directive {body}");
    matches[0].activity
}

pub fn loaded(path: &str, text: &str) -> LoadedSource {
    LoadedSource {
        id: SourceId::new(format!("/fixture/{path}")),
        path: format!("/fixture/{path}").into(),
        bytes: text.as_bytes().into(),
        decoded_text: Some(text.into()),
        compile_time_backing: None,
        revision: SourceRevision::Overlay {
            version: 1,
            content_hash: pascal_project::content_hash_bytes(text.as_bytes()),
        },
    }
}

pub struct Harness {
    pub state: CompileTimeState,
    pub root: LoadedSource,
    text: String,
    finished: bool,
    budget: CompileTimeBudget<'static>,
}
impl Harness {
    pub fn stream(text: &str, context: ConditionalContext) -> Self {
        let root = loaded("T.pas", text);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let state = CompileTimeState::new(&context, &root, &mut budget).unwrap();
        let mut h = Self {
            state,
            root,
            text: String::new(),
            finished: false,
            budget,
        };
        h.feed(text);
        h
    }
    pub fn plain(text: &str, context: ConditionalContext) -> Self {
        let mut h = Self::stream(text, context);
        h.finish();
        h
    }
    pub fn feed(&mut self, text: &str) {
        self.feed_activity(text, Truth::True);
    }
    pub fn feed_activity(&mut self, text: &str, activity: Truth) {
        assert!(!self.finished);
        self.state
            .feed(
                SourceFragment {
                    source: &self.root.id,
                    occurrence: OccurrenceId(0),
                    physical_start: self.text.len(),
                    text,
                },
                activity,
                &mut self.budget,
            )
            .unwrap();
        self.text.push_str(text);
    }
    pub fn finish(&mut self) {
        assert!(!self.finished);
        self.state.finish_root(&mut self.budget).unwrap();
        self.finished = true;
    }
    pub fn with_layout(&mut self, body: &str) {
        self.with_layout_activity(body, Truth::True);
    }
    pub fn with_layout_activity(&mut self, body: &str, activity: Truth) {
        assert!(!self.finished);
        self.state
            .apply_layout_directive(body, activity, &mut self.budget)
            .unwrap();
    }
    pub fn size(&self, name: &str) -> Option<u64> {
        self.at(self.text.len(), name)
    }
    pub fn size_at(&self, marker: &str, name: &str) -> Option<u64> {
        assert_eq!(self.text.matches(marker).count(), 1);
        self.at(self.text.find(marker).unwrap(), name)
    }
    pub fn query(&self, marker: &str, operand: &str) -> Fact<KnownLayout> {
        assert_eq!(self.text.matches(marker).count(), 1);
        let site = self
            .state
            .site_at(
                &self.root.id,
                OccurrenceId(0),
                self.text.find(marker).unwrap(),
            )
            .unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        match parse_sizeof_operand(operand, &mut budget) {
            Fact::Known(operand) => self.state.layout_operand(site, &operand, &mut budget),
            Fact::Unknown(reason) => Fact::Unknown(reason),
            Fact::Incomplete(reason) => Fact::Incomplete(reason),
        }
    }
    fn at(&self, offset: usize, name: &str) -> Option<u64> {
        let site = self.state.site_at(&self.root.id, OccurrenceId(0), offset)?;
        self.state
            .layout_name(
                site,
                name,
                &mut CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation),
            )
            .known()
            .map(|v| v.storage.size)
    }
}
