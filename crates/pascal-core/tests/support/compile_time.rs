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
    includes: std::collections::BTreeMap<String, LoadedSource>,
    denied: std::collections::BTreeSet<String>,
    reads: Vec<String>,
}
impl Fixture {
    pub fn new(text: &str, context: ConditionalContext) -> Self {
        Self {
            root: loaded("T.pas", text),
            context,
            limits: CompileTimeLimits::default(),
            includes: Default::default(),
            denied: Default::default(),
            reads: vec![],
        }
    }
    pub fn include(&mut self, name: &str, text: &str) {
        self.includes.insert(name.into(), loaded(name, text));
    }
    pub fn deny_include(&mut self, name: &str) {
        self.denied.insert(name.into());
    }
    pub fn read_names(&self) -> Vec<String> {
        self.reads.clone()
    }
    pub fn analyze(&mut self) -> Result<CompileTimeAnalysis, ResolverError> {
        let root = self.root.clone();
        let context = self.context.clone();
        let limits = self.limits;
        analyze_source(&root, &context, self, limits, &NoCancellation)
    }
}
impl CompileTimeResolver for Fixture {
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
        let result = if self.denied.contains(request.requested_name) {
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
            warnings: vec![],
        })
    }
    fn unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
        NoResources.unit(request, budget, cancel)
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
