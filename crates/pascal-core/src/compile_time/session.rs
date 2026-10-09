//! Source-backed root session with independent reachable typed continuations.
//! Textual includes, providers and verified headers have separate owners.
use super::*;
use crate::conditional::{
    self, BoundValue, ConditionalAnalysis, ConditionalDirective, ConditionalEnvironment,
    ConstantValue, DirectiveKind, ExpressionQuery, SourceOrder, Truth,
};
use crate::{
    CancellationToken, IncludeResolveRequest, LoadedSource, Resolution, ResolutionObservation,
    ResolutionOutcome, ResolvedUnit, ResolverError, UnitResolveRequest,
};
use pascal_project::ConditionalContext;
use std::sync::Arc;
use std::{mem::size_of, ops::Range};

pub type AdmittedProviderSource = Arc<AdmittedShared<ProviderSource>>;
pub type AdmittedSourceBacking = Arc<AdmittedShared<LoadedSource>>;
pub type AdmittedInterfaceGraph = Arc<AdmittedShared<CompileTimeState>>;

#[derive(Debug, Clone)]
pub struct ProviderSource {
    pub unit: ResolvedUnit,
    pub context: ConditionalContext,
    pub authorized_qualifiers: Vec<String>,
    backing: Option<AdmittedSourceBacking>,
    metadata_credit: Option<Arc<AdmittedShared<()>>>,
}

impl ProviderSource {
    pub fn new(
        unit: ResolvedUnit,
        context: ConditionalContext,
        authorized_qualifiers: Vec<String>,
    ) -> Self {
        Self {
            unit,
            context,
            authorized_qualifiers,
            backing: None,
            metadata_credit: None,
        }
    }

    /// The plain compile-time compatibility return owns its newly constructed
    /// context/qualifiers separately from shared source backing and cache data.
    pub(crate) fn admit_metadata_build(
        budget: &mut CompileTimeBudget<'_>,
        bytes: usize,
        payload: usize,
        build: impl FnOnce() -> Self,
    ) -> Result<Self, ResolverError> {
        let credit = budget
            .admit_owned(1, bytes, payload + size_of::<Self>(), || ())
            .and_then(|v| v.into_shared(budget))
            .and_then(|v| v.into_arc(budget))
            .map_err(|r| resolver_error(r, budget))?;
        let mut value = build();
        value.metadata_credit = Some(credit);
        Ok(value)
    }

    /// Admit a bounded pre-existing cache/input source before its root-local
    /// metadata/Arc handle copy. This is not permission to read a legacy store
    /// unboundedly and then call this method as a substitute for read admission.
    pub fn admit_cached_backing(
        source: &LoadedSource,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedSourceBacking, ResolverError> {
        if let Some(backing) = &source.compile_time_backing {
            if backing.is_admitted_by(budget) {
                return Ok(backing.clone());
            }
            return Err(ResolverError::InvalidRequest(
                "source backing belongs to another root".into(),
            ));
        }
        let payload = crate::resolver::loaded_source_metadata_retained(source)
            .checked_add(source.bytes.len())
            .and_then(|n| n.checked_add(source.decoded_text.as_ref().map_or(0, |s| s.len())))
            .ok_or_else(|| {
                ResolverError::InvalidRequest("source backing admission overflow".into())
            })?;
        budget
            .admit_owned(1, payload, payload, || source.clone())
            .and_then(|value| value.into_shared(budget))
            .and_then(|value| value.into_arc(budget))
            .map_err(|r| resolver_error(r, budget))
    }

    pub fn admit_copy(
        &self,
        backing: &AdmittedSourceBacking,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedProviderSource, ResolverError> {
        if !backing.is_admitted_by(budget)
            || backing.id != self.unit.source.id
            || backing.revision != self.unit.source.revision
        {
            return Err(ResolverError::InvalidRequest(
                "provider source has no same-root backing admission".into(),
            ));
        }
        let payload = provider_owned_payload(self)?;
        budget
            .admit_owned(1, payload, payload, || Self {
                unit: self.unit.clone(),
                context: self.context.clone(),
                authorized_qualifiers: self.authorized_qualifiers.clone(),
                backing: Some(backing.clone()),
                metadata_credit: None,
            })
            .and_then(|value| value.into_shared(budget))
            .and_then(|value| value.into_arc(budget))
            .map_err(|r| resolver_error(r, budget))
    }

    fn valid_backing(&self, budget: &CompileTimeBudget<'_>) -> bool {
        self.backing.as_ref().is_some_and(|s| {
            s.is_admitted_by(budget)
                && s.id == self.unit.source.id
                && s.revision == self.unit.source.revision
        })
    }
}

fn provider_owned_payload(provider: &ProviderSource) -> Result<usize, ResolverError> {
    let mut bytes = crate::resolver::loaded_source_metadata_retained(&provider.unit.source)
        + provider.unit.requested_name.len()
        + provider.unit.declared_name.len()
        + provider.authorized_qualifiers.len() * size_of::<String>();
    provider
        .context
        .visit_recovery_payload(&mut |n| {
            bytes = bytes.checked_add(n).ok_or("provider context overflow")?;
            Ok(())
        })
        .map_err(ResolverError::InvalidRequest)?;
    bytes = bytes
        .checked_add(
            (provider.context.defines.len()
                + provider.context.options.len()
                + provider.context.constants.len())
                * (size_of::<(String, ConstantValue)>() + 4 * size_of::<usize>()),
        )
        .ok_or_else(|| {
            ResolverError::InvalidRequest("provider context metadata overflow".into())
        })?;
    for q in &provider.authorized_qualifiers {
        bytes = bytes
            .checked_add(q.len())
            .ok_or_else(|| ResolverError::InvalidRequest("provider qualifier overflow".into()))?;
    }
    Ok(bytes)
}

pub trait CompileTimeResolver {
    /// Compile-time capability entrypoint. Never delegates to an unbounded legacy
    /// callback. Adapters without this capability fail closed; legacy lookup
    /// remains available for non-admitted callers until its store bridge exists.
    fn unit_capability(
        &mut self,
        _request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<AdmittedProviderSource>, ResolverError> {
        let reason = "no admitted provider capability";
        budget
            .charge(1, reason.len(), reason.len())
            .map_err(|r| resolver_error(r, budget))?;
        Ok(ResolutionOutcome {
            result: Resolution::Unavailable {
                reason: reason.into(),
            },
            observations: vec![],
            warnings: vec![],
        })
    }
    fn max_include_depth(&self) -> usize {
        64
    }
    fn admit_include(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<bool, ResolverError> {
        budget
            .charge(1, 0, 0)
            .map_err(|r| resolver_error(r, budget))?;
        Ok(true)
    }
    fn include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError>;
    fn unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError>;
}

#[derive(Debug, Clone)]
pub struct OccurrenceAnalysis {
    pub id: OccurrenceId,
    pub source: LoadedSource,
    pub analysis: ConditionalAnalysis,
}

#[derive(Debug, Clone)]
pub struct IncludeOccurrence {
    pub owner: OccurrenceId,
    pub directive: SourceAnchor,
    pub target: Resolution<LoadedSource>,
    pub child: Option<OccurrenceId>,
    /// Exact physical delimiter/body fragments; `directive` names the opener.
    pub fragments: Vec<SourceAnchor>,
}

/// A logical directive may span several physical source/occurrence fragments.
/// The legacy directive range names only its completing physical fragment.
#[derive(Debug, Clone)]
pub struct DirectiveFragments {
    pub completion: OccurrenceId,
    pub directive: ConditionalDirective,
    pub fragments: Vec<SourceAnchor>,
    pub body_fragments: Vec<SourceAnchor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUnitHeader {
    pub name: String,
    pub declaration: SourceAnchor,
    pub name_parts: Vec<SourceAnchor>,
}

#[derive(Debug)]
pub struct CompileTimeOutput {
    pub root: ConditionalAnalysis,
    pub occurrences: Vec<OccurrenceAnalysis>,
    pub includes: Vec<IncludeOccurrence>,
    pub facts: AdmittedShared<CompileTimeState>,
    pub header: Fact<VerifiedUnitHeader>,
    pub reasons: Vec<IncompleteReason>,
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
    pub directive_fragments: Vec<DirectiveFragments>,
    /// Admitted session storage and cumulative work (not allocator telemetry).
    pub budget: Arc<AdmittedShared<()>>,
}

pub type CompileTimeAnalysis = AdmittedOwned<CompileTimeOutput>;

#[derive(Debug)]
struct SessionOutput {
    root: ConditionalAnalysis,
    occurrences: Vec<OccurrenceAnalysis>,
    includes: Vec<IncludeOccurrence>,
    facts: CompileTimeState,
    header: Fact<VerifiedUnitHeader>,
    reasons: Vec<IncompleteReason>,
    observations: AdmittedOwned<Vec<ResolutionObservation>>,
    warnings: AdmittedOwned<Vec<String>>,
    directive_fragments: AdmittedOwned<Vec<DirectiveFragments>>,
}

/// Analyze one root with the shared conditional interpreter and lexical binder.
/// Authorized includes suspend only physical coordinates: the lexical binder,
/// conditional paths and compiler context continue through every child EOF.
pub fn analyze_source(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    limits: CompileTimeLimits,
    cancel: &dyn CancellationToken,
) -> Result<CompileTimeAnalysis, ResolverError> {
    let mut budget = CompileTimeBudget::new(limits, cancel);
    analyze_source_in_session(root, context, resolver, &mut budget, cancel)
}

/// Generic resolver roots use their remaining aggregate admission and inspect
/// this same budget on success AND failure. Children never create a new budget.
pub(crate) fn analyze_source_in_session(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<CompileTimeAnalysis, ResolverError> {
    let metadata = crate::resolver::loaded_source_metadata_retained(root);
    budget
        .charge(1, metadata, metadata.saturating_mul(2))
        .map_err(|r| resolver_error(r, budget))?;
    let analysis = budget.admit_compilation(metadata.saturating_mul(2), |budget| {
        let mut providers = ProviderSession::new(budget)?;
        analyze_source_with_providers_inner(
            root,
            context,
            resolver,
            budget,
            cancel,
            &mut providers,
            false,
            OccurrenceId(0),
            true,
        )
    })?;
    publish_analysis(analysis, budget)
}

struct ProviderSession {
    active: AdmittedOwned<Vec<AdmittedOwned<ProviderKey>>>,
    queries: usize,
    next_occurrence: u64,
    declarations: AdmittedOwned<Vec<AdmittedOwned<DefinitionKey>>>,
}

impl ProviderSession {
    fn new(budget: &mut CompileTimeBudget<'_>) -> Result<Self, ResolverError> {
        let active = budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?;
        let declarations = budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?;
        Ok(Self {
            active,
            declarations,
            queries: 0,
            next_occurrence: 1,
        })
    }
}

fn publish_analysis(
    mut analysis: AdmittedOwned<SessionOutput>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<CompileTimeAnalysis, ResolverError> {
    let facts = analysis
        .facts
        .admitted_snapshot(budget)
        .map_err(|r| resolver_error(r, budget))?;
    let view = budget
        .admit_owned(0, 0, 0, || ())
        .and_then(|v| v.into_shared(budget))
        .and_then(|v| v.into_arc(budget))
        .map_err(|r| resolver_error(r, budget))?;
    let mut payload = conditional_output_payload(&analysis.root)?;
    for o in &analysis.occurrences {
        payload = payload
            .checked_add(
                size_of::<OccurrenceAnalysis>()
                    + crate::resolver::loaded_source_metadata_retained(&o.source)
                    + conditional_output_payload(&o.analysis)?,
            )
            .ok_or_else(|| ResolverError::InvalidRequest("occurrence output overflow".into()))?;
    }
    for i in &analysis.includes {
        payload = payload
            .checked_add(
                size_of::<IncludeOccurrence>()
                    + i.directive.source.as_str().len()
                    + i.fragments.len() * size_of::<SourceAnchor>(),
            )
            .ok_or_else(|| ResolverError::InvalidRequest("include output overflow".into()))?;
        for f in &i.fragments {
            payload += f.source.as_str().len();
        }
        match &i.target {
            Resolution::Found(s) => payload += crate::resolver::loaded_source_metadata_retained(s),
            Resolution::Unavailable { reason } => payload += reason.len(),
            Resolution::Ambiguous { candidates } | Resolution::Incomplete { candidates, .. } => {
                for c in candidates {
                    payload += size_of::<crate::ResolutionCandidate>()
                        + c.path.as_os_str().len()
                        + c.declared_name.as_ref().map_or(0, String::len);
                }
            }
        }
    }
    payload += analysis.reasons.len() * size_of::<IncompleteReason>();
    for r in &analysis.reasons {
        if let IncompleteReason::RequiredSource { source, reason } = r {
            payload += source.as_str().len() + reason.len();
        }
    }
    let mut output = budget
        .admit_owned(1, payload, payload, || CompileTimeOutput {
            root: analysis.root.clone(),
            occurrences: analysis.occurrences.clone(),
            includes: analysis.includes.clone(),
            facts,
            header: analysis.header.clone(),
            reasons: analysis.reasons.clone(),
            observations: Vec::new(),
            warnings: Vec::new(),
            directive_fragments: Vec::new(),
            budget: view,
        })
        .map_err(|r| resolver_error(r, budget))?;
    // The side buffers already have independent construction/return owners.
    // Replace their internal slots with admitted empty carriers, then move the
    // actual vectors and their exact minted credits into the published fields.
    let empty_observations = budget
        .admit_owned(0, 0, 0, Vec::new)
        .map_err(|r| resolver_error(r, budget))?;
    let empty_warnings = budget
        .admit_owned(0, 0, 0, Vec::new)
        .map_err(|r| resolver_error(r, budget))?;
    let empty_fragments = budget
        .admit_owned(0, 0, 0, Vec::new)
        .map_err(|r| resolver_error(r, budget))?;
    let observations =
        std::mem::replace(&mut analysis.value_mut().observations, empty_observations);
    let warnings = std::mem::replace(&mut analysis.value_mut().warnings, empty_warnings);
    let fragments = std::mem::replace(
        &mut analysis.value_mut().directive_fragments,
        empty_fragments,
    );
    output
        .move_field(observations, budget, |output, observations| {
            output.observations = observations
        })
        .map_err(|r| resolver_error(r, budget))?;
    output
        .move_field(warnings, budget, |output, warnings| {
            output.warnings = warnings
        })
        .map_err(|r| resolver_error(r, budget))?;
    output
        .move_field(fragments, budget, |output, fragments| {
            output.directive_fragments = fragments
        })
        .map_err(|r| resolver_error(r, budget))?;
    Ok(output)
}

fn conditional_output_payload(a: &ConditionalAnalysis) -> Result<usize, ResolverError> {
    let mut payload = size_of::<ConditionalAnalysis>()
        + a.projected_source.len()
        + (a.inactive_spans.len() + a.unknown_spans.len()) * size_of::<Range<usize>>()
        + a.directives.len() * size_of::<ConditionalDirective>();
    for d in &a.directives {
        payload = payload
            .checked_add(d.body.len())
            .ok_or_else(|| ResolverError::InvalidRequest("conditional output overflow".into()))?;
    }
    Ok(payload)
}

#[derive(Debug, PartialEq, Eq)]
struct ProviderKey {
    source: crate::SourceId,
    revision: crate::SourceRevision,
    context: ConditionalContext,
    qualifiers: Vec<String>,
}

fn push_owner<T>(
    owners: &mut AdmittedOwned<Vec<AdmittedOwned<T>>>,
    owner: AdmittedOwned<T>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    owners
        .with_budget_mut(budget, |v, budget| {
            if v.len() == v.capacity() {
                budget
                    .charge(1, 0, size_of::<AdmittedOwned<T>>())
                    .map_err(|r| resolver_error(r, budget))?;
                v.reserve_exact(1);
            }
            v.push(owner);
            Ok(())
        })
        .map_err(|r| resolver_error(r, budget))?
}

fn analyze_source_with_providers(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
    providers: &mut ProviderSession,
    interface_only: bool,
    root_occurrence: OccurrenceId,
) -> Result<AdmittedOwned<SessionOutput>, ResolverError> {
    budget.admit_compilation(0, |budget| {
        analyze_source_with_providers_inner(
            root,
            context,
            resolver,
            budget,
            cancel,
            providers,
            interface_only,
            root_occurrence,
            false,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn analyze_source_with_providers_inner(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    mut budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
    providers: &mut ProviderSession,
    interface_only: bool,
    root_occurrence: OccurrenceId,
    metadata_admitted: bool,
) -> Result<SessionOutput, ResolverError> {
    let limits = budget.limits();
    let bytes = root.analysis_bytes();
    let metadata = crate::resolver::loaded_source_metadata_retained(root);
    if !metadata_admitted {
        budget
            .charge(1, metadata, metadata.saturating_mul(2))
            .map_err(|r| resolver_error(r, budget))?;
    }
    // Reserve root projection, lexer/output scratch and source identity copies
    // before the existing lexer's allocations. Its original directive/depth/
    // environment limits remain enforced by the shared interpreter.
    let reserve = bytes
        .len()
        .checked_mul(4)
        .and_then(|n| {
            n.checked_add(
                size_of::<CompileTimeAnalysis>()
                    + size_of::<OccurrenceAnalysis>()
                    + root.id.as_str().len()
                    + root.path.as_os_str().len(),
            )
        })
        .ok_or_else(|| ResolverError::InvalidRequest("root admission overflow".into()))?;
    budget
        .charge(1, bytes.len(), reserve)
        .map_err(|r| resolver_error(r, &budget))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ResolverError::InvalidRequest("root coordinate text is not UTF-8".into()))?;
    let entries = context
        .defines
        .len()
        .checked_add(context.options.len())
        .and_then(|n| n.checked_add(context.constants.len()))
        .ok_or_else(|| ResolverError::InvalidRequest("context admission overflow".into()))?;
    let metadata = entries
        .checked_mul(size_of::<(String, ConstantValue)>() + 4 * size_of::<usize>())
        .and_then(|n| n.checked_add(size_of::<ConditionalEnvironment>()))
        .ok_or_else(|| ResolverError::InvalidRequest("context admission overflow".into()))?;
    budget
        .charge(entries + 1, 0, metadata)
        .map_err(|r| resolver_error(r, &budget))?;
    // The conditional environment is separately retained from the immutable
    // declaration context. Admit its context payload before construction.
    let mut failure = None;
    context
        .visit_recovery_payload(&mut |n| {
            budget.charge(1, n, n).map_err(|reason| {
                failure = Some(reason);
                "conditional context admission".into()
            })
        })
        .map_err(|_| {
            resolver_error(
                failure.unwrap_or(IncompleteReason::MalformedSource),
                &budget,
            )
        })?;
    let mut state = CompileTimeState::new(context, root, &mut budget)
        .map_err(|r| resolver_error(r, &budget))?;
    state.interface_only = interface_only;
    super::declarations::limit(1, limits.max_branch_states.min(64), "max_branch_states")
        .map_err(|r| resolver_error(r, &budget))?;
    budget
        .charge(1, 0, size_of::<ReachablePath>() + size_of::<Sequence>())
        .map_err(|r| resolver_error(r, &budget))?;
    let mut environment = ConditionalEnvironment::from_context(context);
    environment.typed_state = Some(Box::new(
        state
            .snapshot(&mut budget)
            .map_err(|r| resolver_error(r, &budget))?,
    ));
    let scalar = environment
        .snapshot_scalar(&mut budget)
        .map_err(|r| resolver_error(r, &budget))?;
    environment
        .typed_state
        .as_deref_mut()
        .expect("typed root")
        .scalar_history
        .push((Sequence(0), scalar));
    let first = ReachablePath { environment };
    budget
        .charge(
            1,
            root.path.as_os_str().len(),
            size_of::<(OccurrenceId, std::path::PathBuf)>() + root.path.as_os_str().len(),
        )
        .map_err(|r| resolver_error(r, budget))?;
    let mut session = RootSession {
        root: root.clone(),
        text: text.to_owned(),
        resolver,
        cancel,
        context,
        occurrence: root_occurrence,
        active_sources: vec![root.id.clone()],
        occurrences: Vec::new(),
        observations: budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?,
        warnings: budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?,
        scanner: conditional::DirectiveScanner::new(budget)
            .map_err(|r| resolver_error(r, budget))?,
        resource_error: None,
        directive_count: 0,
        directive_fragments: budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?,
        directive_fragment_slots: 0,
        current_directive: 0,
        state,
        budget,
        reasons: Vec::new(),
        includes: Vec::new(),
        fatal: None,
        current: vec![first],
        frames: Vec::new(),
        last_condition: Truth::Unknown,
        providers,
        physical_paths: vec![(root_occurrence, root.path.clone())],
    };
    let mut analysis = conditional::analyze_with_source_order(text, context, cancel, &mut session);
    if let Some(error) = session.resource_error.take() {
        return Err(error);
    }
    if !session.scanner.complete() {
        session.record(IncompleteReason::MalformedSource);
    }
    if let Some(reason) = session.fatal.take() {
        return Err(resolver_error(reason, &session.budget));
    }
    if !session.frames.is_empty() {
        session.record(IncompleteReason::MalformedSource);
        let paths = session.reachable_count();
        session
            .budget
            .charge(paths, 0, paths * size_of::<ReachablePath>())
            .map_err(|r| resolver_error(r, &session.budget))?;
        for mut frame in std::mem::take(&mut session.frames) {
            session.current.append(&mut frame.pending);
            session.current.append(&mut frame.joined);
        }
    }
    if session.reasons.is_empty() {
        let mut failure = None;
        for path in &mut session.current {
            if let Err(reason) = path.state_mut().finish_root(&mut session.budget) {
                failure = Some(reason);
                break;
            } else if path.state().unknown {
                failure = Some(IncompleteReason::MalformedSource);
                break;
            }
        }
        if let Some(reason) = failure {
            session.record(reason);
        }
    }
    if !session.current.is_empty() {
        let count = session.current.len();
        session
            .budget
            .charge(
                count,
                0,
                count * (size_of::<CompileTimeState>() + size_of::<ConditionalEnvironment>()),
            )
            .map_err(|r| resolver_error(r, &session.budget))?;
        let mut paths = std::mem::take(&mut session.current).into_iter();
        session.state = paths.next().expect("nonempty").into_state();
        session.state.alternatives = paths.map(|path| path.into_state()).collect();
    }
    if let Some(reason) = session.fatal.take() {
        return Err(resolver_error(reason, &session.budget));
    }
    if cancel.is_cancelled() {
        return Err(ResolverError::Cancelled);
    }
    if !analysis.complete && session.reasons.is_empty() {
        session.record(IncompleteReason::MalformedSource);
    }
    if let Some(reason) = session.reasons.first() {
        let dynamic = match reason {
            IncompleteReason::RequiredSource { source, reason } => {
                source.as_str().len() + reason.len()
            }
            _ => 0,
        };
        let count = session.state.alternatives.len() + 1;
        session
            .budget
            .charge(
                count,
                dynamic.saturating_mul(count),
                (size_of::<IncompleteReason>() + dynamic).saturating_mul(count),
            )
            .map_err(|r| resolver_error(r, &session.budget))?;
        // Every retained graph remains incomplete; never publish the primary
        // healthy path merely because it happened to be the first alternative.
        session.state.failure = Some(reason.clone());
        for alternative in &mut session.state.alternatives {
            alternative.failure = Some(reason.clone());
        }
    }
    if let Some(reason) = session.fatal.take() {
        return Err(resolver_error(reason, &session.budget));
    }
    if !session.reasons.is_empty() {
        analysis.complete = false;
        analysis.inactive_spans.clear();
        analysis.unknown_spans.clear();
        if !text.is_empty() {
            analysis.unknown_spans.push(0..text.len());
        }
        // A failed typed admission cannot publish an apparently safe projection.
        analysis.projected_source = text.to_owned();
    }
    // Complete output cloning is admitted before allocation. Source byte/text
    // Arcs are shared; source identity/path metadata and analysis are copied.
    let mut payload = analysis.projected_source.len()
        + analysis.inactive_spans.len() * size_of::<Range<usize>>()
        + analysis.unknown_spans.len() * size_of::<Range<usize>>()
        + root.id.as_str().len()
        + root.path.as_os_str().len();
    for d in &analysis.directives {
        payload = payload
            .checked_add(size_of::<ConditionalDirective>() + d.body.len())
            .ok_or_else(|| ResolverError::InvalidRequest("output admission overflow".into()))?;
    }
    session
        .budget
        .charge(analysis.directives.len() + 1, payload, payload)
        .map_err(|r| resolver_error(r, &session.budget))?;
    let mut occurrence = OccurrenceAnalysis {
        id: root_occurrence,
        source: root.clone(),
        analysis: analysis.clone(),
    };
    // Projection only: finalized logical directives mask their actual physical
    // fragments, including child fragments whose EOF preceded the terminator.
    // No parser, evaluator or read decision is run again.
    for occurrence in std::iter::once(&mut occurrence).chain(session.occurrences.iter_mut()) {
        if !occurrence.analysis.complete {
            continue;
        }
        session
            .budget
            .charge(1, occurrence.analysis.projected_source.len(), 0)
            .map_err(|r| resolver_error(r, &session.budget))?;
        let mut bytes = std::mem::take(&mut occurrence.analysis.projected_source).into_bytes();
        for directive in session.directive_fragments.iter() {
            session
                .budget
                .charge(1, 0, 0)
                .map_err(|r| resolver_error(r, &session.budget))?;
            for anchor in &directive.fragments {
                session
                    .budget
                    .charge(1, 0, 0)
                    .map_err(|r| resolver_error(r, &session.budget))?;
                if anchor.occurrence == occurrence.id {
                    session
                        .budget
                        .charge(1, anchor.range.len(), 0)
                        .map_err(|r| resolver_error(r, &session.budget))?;
                    for byte in &mut bytes[anchor.range.clone()] {
                        if !matches!(*byte, b'\n' | b'\r') {
                            *byte = b' ';
                        }
                    }
                }
            }
        }
        occurrence.analysis.projected_source =
            String::from_utf8(bytes).expect("complete physical fragments preserve UTF-8 masking");
    }
    if cancel.is_cancelled() {
        return Err(ResolverError::Cancelled);
    }
    Ok(SessionOutput {
        root: analysis,
        occurrences: {
            session.occurrences.insert(0, occurrence);
            session.occurrences
        },
        includes: session.includes,
        facts: session.state,
        header: Fact::Unknown(UnknownReason::HeaderNotAnalyzed),
        reasons: session.reasons,
        observations: session.observations,
        warnings: session.warnings,
        directive_fragments: session.directive_fragments,
    })
}

fn admit_observation(
    observation: &ResolutionObservation,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    let n = observation_payload(observation)?;
    budget
        .charge(1, n, n)
        .map_err(|r| resolver_error(r, budget))
}

fn observation_payload(observation: &ResolutionObservation) -> Result<usize, ResolverError> {
    let mut total = size_of::<ResolutionObservation>();
    let mut visit = |n| {
        total = total.checked_add(n).ok_or("observation payload overflow")?;
        Ok(())
    };
    let result = match observation {
        ResolutionObservation::Directory { path, entry, .. } => {
            visit(path.as_os_str().len()).and_then(|_| entry.visit_recovery_payload(&mut visit))
        }
        ResolutionObservation::Candidate { path, entry, .. } => visit(path.as_os_str().len())
            .and_then(|_| {
                if let Some(entry) = entry {
                    entry.visit_recovery_payload(&mut visit)
                } else {
                    Ok(())
                }
            }),
        ResolutionObservation::Payload {
            source_id,
            path,
            revision,
        } => visit(source_id.as_str().len())
            .and_then(|_| visit(path.as_os_str().len()))
            .and_then(|_| match revision {
                crate::SourceRevision::Disk {
                    read_policy,
                    path_entry,
                    ..
                } => read_policy
                    .visit_owned_clone_payload(&mut visit)
                    .and_then(|_| path_entry.visit_recovery_payload(&mut visit)),
                _ => Ok(()),
            }),
        ResolutionObservation::Metadata(observation) => {
            observation.visit_recovery_payload(&mut visit)
        }
        ResolutionObservation::ProjectRead(observation) => {
            observation.visit_recovery_payload(&mut visit)
        }
    };
    result.map_err(ResolverError::InvalidRequest)?;
    Ok(total)
}

fn append_observations(
    target: &mut AdmittedOwned<Vec<ResolutionObservation>>,
    incoming: &[ResolutionObservation],
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    let mut payload = 0usize;
    for o in incoming {
        payload = payload
            .checked_add(observation_payload(o)?)
            .ok_or_else(|| ResolverError::InvalidRequest("observation vector overflow".into()))?;
    }
    target
        .with_budget_mut(budget, |v, budget| {
            let growth = (v.len() + incoming.len()).saturating_sub(v.capacity())
                * size_of::<ResolutionObservation>();
            budget
                .charge(incoming.len(), payload, payload + growth)
                .map_err(|r| resolver_error(r, budget))?;
            v.reserve_exact(incoming.len());
            v.extend_from_slice(incoming);
            Ok(())
        })
        .map_err(|r| resolver_error(r, budget))?
}

fn append_warnings(
    target: &mut AdmittedOwned<Vec<String>>,
    incoming: &[String],
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    let payload = incoming.iter().map(String::len).sum::<usize>();
    target
        .with_budget_mut(budget, |v, budget| {
            let growth =
                (v.len() + incoming.len()).saturating_sub(v.capacity()) * size_of::<String>();
            budget
                .charge(incoming.len(), payload, payload + growth)
                .map_err(|r| resolver_error(r, budget))?;
            v.reserve_exact(incoming.len());
            v.extend_from_slice(incoming);
            Ok(())
        })
        .map_err(|r| resolver_error(r, budget))?
}

pub(crate) fn resolver_error(
    reason: IncompleteReason,
    budget: &CompileTimeBudget<'_>,
) -> ResolverError {
    match reason {
        IncompleteReason::Cancelled => ResolverError::Cancelled,
        IncompleteReason::Limit { name, maximum } => match budget.failure_observed(name, maximum) {
            Some(observed) => ResolverError::LimitExceeded {
                limit: name,
                observed,
                maximum,
            },
            // Structural failures carry no observed count in the approved Fact
            // model. Preserve that reason rather than inventing max+1 metadata.
            None => ResolverError::InvalidRequest(format!("compile-time admission: {reason:?}")),
        },
        _ => ResolverError::InvalidRequest(format!("compile-time admission: {reason:?}")),
    }
}

struct RootSession<'a, 'b> {
    root: LoadedSource,
    text: String,
    resolver: &'a mut dyn CompileTimeResolver,
    cancel: &'a dyn CancellationToken,
    context: &'a ConditionalContext,
    occurrence: OccurrenceId,
    active_sources: Vec<crate::SourceId>,
    occurrences: Vec<OccurrenceAnalysis>,
    observations: AdmittedOwned<Vec<ResolutionObservation>>,
    warnings: AdmittedOwned<Vec<String>>,
    scanner: conditional::DirectiveScanner,
    resource_error: Option<ResolverError>,
    directive_count: usize,
    directive_fragments: AdmittedOwned<Vec<DirectiveFragments>>,
    directive_fragment_slots: usize,
    current_directive: usize,
    state: CompileTimeState,
    budget: &'a mut CompileTimeBudget<'b>,
    reasons: Vec<IncompleteReason>,
    includes: Vec<IncludeOccurrence>,
    fatal: Option<IncompleteReason>,
    current: Vec<ReachablePath>,
    frames: Vec<ReachableFrame>,
    last_condition: Truth,
    providers: &'a mut ProviderSession,
    physical_paths: Vec<(OccurrenceId, std::path::PathBuf)>,
}

struct ReachablePath {
    environment: ConditionalEnvironment,
}

#[derive(Default)]
struct ReachableFrame {
    pending: Vec<ReachablePath>,
    joined: Vec<ReachablePath>,
    has_else: bool,
}

struct ProviderEngine<'a, 'b> {
    resolver: &'a mut dyn CompileTimeResolver,
    providers: &'a mut ProviderSession,
    cancel: &'a dyn CancellationToken,
    budget: &'a mut CompileTimeBudget<'b>,
    observations: &'a mut AdmittedOwned<Vec<ResolutionObservation>>,
    warnings: &'a mut AdmittedOwned<Vec<String>>,
}

fn admit_provider_context(
    context: &ConditionalContext,
    budget: &mut CompileTimeBudget<'_>,
    retained: bool,
) -> Result<(), ResolverError> {
    let entries = context
        .defines
        .len()
        .checked_add(context.options.len())
        .and_then(|n| n.checked_add(context.constants.len()))
        .ok_or_else(|| ResolverError::InvalidRequest("provider context entries overflow".into()))?;
    let metadata = entries
        .checked_mul(size_of::<(String, ConstantValue)>() + 4 * size_of::<usize>())
        .ok_or_else(|| {
            ResolverError::InvalidRequest("provider context metadata overflow".into())
        })?;
    budget
        .charge(entries, 0, if retained { metadata } else { 0 })
        .map_err(|r| resolver_error(r, budget))?;
    let mut failure = None;
    context
        .visit_recovery_payload(&mut |n| {
            budget
                .charge(1, n, if retained { n } else { 0 })
                .map_err(|r| {
                    failure = Some(r);
                    "provider context admission".into()
                })
        })
        .map_err(|_| resolver_error(failure.unwrap_or(IncompleteReason::MalformedSource), budget))
}

impl ProviderEngine<'_, '_> {
    fn prepare_query(
        &mut self,
        state: &mut CompileTimeState,
        site: &BindingSite,
        query: &ExpressionQuery<'_>,
        depth: usize,
    ) -> Result<(), ResolverError> {
        match query {
            ExpressionQuery::Defined(..) => Ok(()),
            ExpressionQuery::SizeOf { argument, .. } => {
                let before = self.budget.manual_retained();
                match parse_sizeof_operand(argument, self.budget) {
                    Fact::Known(operand) => {
                        let leading = operand
                            .leading_name(self.budget)
                            .map_err(|r| resolver_error(r, self.budget));
                        let scratch = self.budget.manual_retained().saturating_sub(before);
                        let result = match &leading {
                            Ok((name, pointers)) => {
                                self.prepare_name(state, site, name, *pointers, depth + 1)
                            }
                            Err(_) => Ok(()),
                        };
                        drop(operand);
                        let error = leading.err();
                        self.budget.release(scratch);
                        if let Some(error) = error {
                            return Err(error);
                        }
                        result
                    }
                    Fact::Incomplete(r) => Err(resolver_error(r, self.budget)),
                    Fact::Unknown(_) => Ok(()),
                }
            }
            ExpressionQuery::LexicalName(name, _) => {
                // Intrinsic probes are explicitly source-lexical checks, not
                // actual imported operand queries (e.g. the SizeOf callee).
                if super::bindings::lookup(state, site, name, self.budget)
                    .map_err(|r| resolver_error(r, self.budget))?
                    .is_some()
                {
                    self.prepare_name(state, site, name, false, depth + 1)
                } else {
                    Ok(())
                }
            }
            ExpressionQuery::Name(name, _) | ExpressionQuery::Declared(name, _) => {
                self.prepare_name(state, site, name, false, depth + 1)
            }
        }
    }

    fn prepare_name(
        &mut self,
        state: &mut CompileTimeState,
        site: &BindingSite,
        name: &str,
        pointers: bool,
        depth: usize,
    ) -> Result<(), ResolverError> {
        use super::bindings::Binding;
        if state.interfaces.is_empty() {
            return Ok(());
        }
        super::declarations::limit(depth, self.budget.limits().max_type_depth, "max_type_depth")
            .map_err(|r| resolver_error(r, self.budget))?;
        let root = name.split('.').next().unwrap_or(name);
        let binding = super::bindings::lookup(state, site, name, self.budget)
            .map_err(|r| resolver_error(r, self.budget))?;
        let binding = if binding.is_some() {
            binding
        } else {
            super::bindings::lookup(state, site, root, self.budget)
                .map_err(|r| resolver_error(r, self.budget))?
        };
        if let Some(binding) = binding {
            let Binding::Declaration(decl) = binding else {
                return Ok(());
            };
            if decl.is_routine {
                return Ok(());
            }
            self.budget
                .charge(
                    self.providers.declarations.len() + 1,
                    decl.key.anchor.source.as_str().len(),
                    0,
                )
                .map_err(|r| resolver_error(r, self.budget))?;
            if self
                .providers
                .declarations
                .iter()
                .any(|key| **key == decl.key)
            {
                return Ok(());
            }
            let key_payload = decl.key.anchor.source.as_str().len();
            let key = self
                .budget
                .admit_owned(1, key_payload, key_payload, || decl.key.clone())
                .map_err(|r| resolver_error(r, self.budget))?;
            push_owner(&mut self.providers.declarations, key, self.budget)?;
            // Retain the original captured sites while mutating only the live
            // graph's lazy capabilities. Copying a snapshot is not syntax work.
            let snapshot_before = self.budget.manual_retained();
            let copy = state
                .snapshot(self.budget)
                .map_err(|r| resolver_error(r, self.budget));
            let snapshot_bytes = self
                .budget
                .manual_retained()
                .saturating_sub(snapshot_before);
            let result = copy.and_then(|copy| {
                let binding = super::bindings::lookup(&copy, site, name, self.budget)
                    .map_err(|r| resolver_error(r, self.budget))?;
                let binding = if binding.is_some() {
                    binding
                } else {
                    super::bindings::lookup(&copy, site, root, self.budget)
                        .map_err(|r| resolver_error(r, self.budget))?
                };
                let Some(Binding::Declaration(decl)) = binding else {
                    return Ok(());
                };
                if let Some(expression) = &decl.initializer {
                    let prior = BindingSite {
                        scope: decl.site.scope,
                        sequence: Sequence(decl.site.sequence.0.saturating_sub(1)),
                    };
                    self.prepare_expression(
                        state,
                        &prior,
                        super::declarations::initializer_expression(expression),
                        depth + 1,
                    )?;
                }
                self.prepare_type(state, &decl.type_site, &decl.ty, pointers, depth + 1)
            });
            self.budget.release(snapshot_bytes);
            self.providers.declarations.value_mut().pop();
            return result;
        }
        for index in 0..state.interfaces.len() {
            self.budget
                .charge(1, 0, 0)
                .map_err(|r| resolver_error(r, self.budget))?;
            if state.interfaces[index].site.sequence.0 > site.sequence.0
                || !state.interfaces[index].pending.authorized
            {
                continue;
            }
            if !state.interfaces[index].attempted {
                super::declarations::limit(
                    self.providers.queries + 1,
                    self.budget.limits().max_provider_queries,
                    "max_provider_queries",
                )
                .map_err(|r| resolver_error(r, self.budget))?;
                self.providers.queries += 1;
                let pending = &state.interfaces[index].pending;
                let outcome = self.budget.external(|budget| {
                    self.resolver.unit_capability(
                        UnitResolveRequest {
                            requested_name: &pending.identity.name,
                            importer_path: &pending.importer_path,
                            legacy_route: None,
                        },
                        budget,
                        self.cancel,
                    )
                })?;
                append_observations(self.observations, &outcome.observations, self.budget)?;
                append_warnings(self.warnings, &outcome.warnings, self.budget)?;
                state.interfaces[index].attempted = true;
                match outcome.result {
                    Resolution::Found(provider) => {
                        if !provider.is_admitted_by(self.budget)
                            || !provider.valid_backing(self.budget)
                        {
                            return Err(ResolverError::InvalidRequest(
                                "unadmitted provider capability".into(),
                            ));
                        }
                        let n = provider
                            .authorized_qualifiers
                            .iter()
                            .map(|q| size_of::<String>() + q.len())
                            .sum();
                        self.budget
                            .charge(1, n, n)
                            .map_err(|r| resolver_error(r, self.budget))?;
                        state.interfaces[index].qualifiers = provider.authorized_qualifiers.clone();
                        state.interfaces[index].provided = Some(provider);
                    }
                    Resolution::Ambiguous { .. } => {
                        state.interfaces[index].failure = Some(UnknownReason::AmbiguousBinding)
                    }
                    _ => state.interfaces[index].failure = Some(UnknownReason::UnresolvedName),
                }
            }
        }
        // Resolve authorized spellings before selecting a graph. An alias may
        // be supplied only by the existing unit capability, not guessed from
        // uses text. A qualified query must not compile unrelated interfaces.
        let mut qualified = false;
        for interface in &state.interfaces {
            if interface.site.sequence.0 > site.sequence.0 || !interface.pending.authorized {
                continue;
            }
            for qualifier in &interface.qualifiers {
                self.budget
                    .charge(1, qualifier.len().min(name.len()), 0)
                    .map_err(|r| resolver_error(r, self.budget))?;
                qualified |= name.len() > qualifier.len()
                    && name.as_bytes().get(qualifier.len()) == Some(&b'.')
                    && name
                        .get(..qualifier.len())
                        .is_some_and(|p| super::bindings::same_name(p, qualifier));
            }
        }
        for index in 0..state.interfaces.len() {
            self.budget
                .charge(1, 0, 0)
                .map_err(|r| resolver_error(r, self.budget))?;
            if state.interfaces[index].site.sequence.0 > site.sequence.0
                || !state.interfaces[index].pending.authorized
            {
                continue;
            }
            let mut suffix = root;
            let mut matches_qualifier = false;
            for qualifier in &state.interfaces[index].qualifiers {
                self.budget
                    .charge(1, qualifier.len().min(name.len()), 0)
                    .map_err(|r| resolver_error(r, self.budget))?;
                if name.len() > qualifier.len()
                    && name.as_bytes().get(qualifier.len()) == Some(&b'.')
                    && name
                        .get(..qualifier.len())
                        .is_some_and(|p| super::bindings::same_name(p, qualifier))
                {
                    suffix = name[qualifier.len() + 1..].split('.').next().unwrap_or("");
                    matches_qualifier = true;
                    break;
                }
            }
            if qualified && !matches_qualifier {
                continue;
            }
            if let Some(provider) = state.interfaces[index].provided.take() {
                match self.compile_provider_interface(provider, suffix, pointers, depth + 1)? {
                    Fact::Known(graph) => state.interfaces[index].graph = Some(graph),
                    Fact::Unknown(reason) => state.interfaces[index].failure = Some(reason),
                    Fact::Incomplete(reason) => return Err(resolver_error(reason, self.budget)),
                }
            } else if let Some(graph) = &state.interfaces[index].graph {
                state.interfaces[index].graph =
                    Some(self.replace_interface_graph(graph, suffix, pointers, depth + 1)?);
            }
        }
        Ok(())
    }

    fn replace_interface_graph(
        &mut self,
        graph: &AdmittedInterfaceGraph,
        suffix: &str,
        pointers: bool,
        depth: usize,
    ) -> Result<AdmittedInterfaceGraph, ResolverError> {
        // An independent mutable copy is required; attach its construction
        // credits before traversing/copying, not after preparation completes.
        let mut copy = self.budget.admit_compilation(0, |budget| {
            graph
                .snapshot(budget)
                .map_err(|r| resolver_error(r, budget))
        })?;
        copy.with_budget_mut(self.budget, |copy, budget| {
            let captured = copy.site();
            let mut engine = ProviderEngine {
                resolver: self.resolver,
                providers: self.providers,
                cancel: self.cancel,
                budget,
                observations: self.observations,
                warnings: self.warnings,
            };
            engine.prepare_name(copy, &captured, suffix, pointers, depth)
        })
        .map_err(|r| resolver_error(r, self.budget))??;
        // Move the now-prepared graph AND its existing credits into shared
        // backing. Only destination Arc/handle controls are new admissions.
        // No discarded projections/scanner/whole-analysis receipt comes along.
        copy.into_shared(self.budget)
            .and_then(|v| v.into_arc(self.budget))
            .map_err(|r| resolver_error(r, self.budget))
    }

    fn prepare_expression(
        &mut self,
        state: &mut CompileTimeState,
        site: &BindingSite,
        text: &str,
        depth: usize,
    ) -> Result<(), ResolverError> {
        let before = self.budget.manual_retained();
        let environment = if let Some((_, environment)) = state
            .scalar_history
            .iter()
            .rev()
            .find(|(seq, _)| seq.0 <= site.sequence.0)
        {
            environment.snapshot_scalar(self.budget)
        } else {
            ConditionalEnvironment::context_snapshot(&state.context, self.budget)
        }
        .map_err(|r| resolver_error(r, self.budget))?;
        let environment_bytes = self.budget.manual_retained().saturating_sub(before);
        let resolver = &mut *self.resolver;
        let providers = &mut *self.providers;
        let observations = &mut *self.observations;
        let warnings = &mut *self.warnings;
        let cancel = self.cancel;
        let mut error = None;
        let result = conditional::evaluate_layout_expression(
            text,
            &environment,
            self.budget,
            &mut |query, budget| {
                let mut engine = ProviderEngine {
                    resolver,
                    providers,
                    cancel,
                    budget,
                    observations,
                    warnings,
                };
                if let Err(e) = engine.prepare_query(state, site, &query, depth + 1) {
                    error = Some(e);
                    return Err(IncompleteReason::MalformedSource);
                }
                super::arrays::scalar_query(
                    state,
                    site,
                    &environment,
                    query,
                    depth + 1,
                    engine.budget,
                )
            },
            false,
            false,
        );
        self.budget.release(environment_bytes);
        if let Some(error) = error {
            return Err(error);
        }
        result
            .map(|_| ())
            .map_err(|r| resolver_error(r, self.budget))
    }

    fn prepare_type(
        &mut self,
        state: &mut CompileTimeState,
        site: &BindingSite,
        ty: &super::bindings::TypeExpr,
        pointers: bool,
        depth: usize,
    ) -> Result<(), ResolverError> {
        use super::bindings::TypeExpr;
        self.budget
            .charge(1, 0, 0)
            .map_err(|r| resolver_error(r, self.budget))?;
        super::declarations::limit(depth, self.budget.limits().max_type_depth, "max_type_depth")
            .map_err(|r| resolver_error(r, self.budget))?;
        match ty {
            TypeExpr::Named(name) | TypeExpr::StrongAlias(name) => {
                self.prepare_name(state, site, name, pointers, depth + 1)
            }
            TypeExpr::Pointer(name) if pointers => {
                self.prepare_name(state, site, name, false, depth + 1)
            }
            TypeExpr::Array {
                dimensions,
                element,
                ..
            } => {
                for index in dimensions {
                    self.prepare_index(state, site, index, depth + 1)?;
                }
                self.prepare_type(state, site, element, pointers, depth + 1)
            }
            TypeExpr::Range(index) => self.prepare_index(state, site, index, depth + 1),
            TypeExpr::Enum(members) => {
                for member in members {
                    if let Some(value) = &member.value {
                        self.prepare_expression(state, site, value, depth + 1)?;
                    }
                }
                Ok(())
            }
            TypeExpr::Record(record) => {
                record.visit_dependencies(&mut |dependency, captured| match dependency {
                    super::records::RecordDependency::Type(ty) => {
                        self.prepare_type(state, captured, ty, pointers, depth + 1)
                    }
                    super::records::RecordDependency::Index(index) => {
                        self.prepare_index(state, captured, index, depth + 1)
                    }
                })
            }
            _ => Ok(()),
        }
    }

    fn prepare_index(
        &mut self,
        state: &mut CompileTimeState,
        site: &BindingSite,
        index: &super::bindings::IndexExpr,
        depth: usize,
    ) -> Result<(), ResolverError> {
        match index {
            super::bindings::IndexExpr::Domain(name) => {
                self.prepare_name(state, site, name, false, depth + 1)
            }
            super::bindings::IndexExpr::Bounds { lower, upper } => {
                self.prepare_expression(state, site, lower, depth + 1)?;
                self.prepare_expression(state, site, upper, depth + 1)
            }
        }
    }

    fn compile_provider_interface(
        &mut self,
        provider: AdmittedProviderSource,
        name: &str,
        pointers: bool,
        depth: usize,
    ) -> Result<Fact<AdmittedInterfaceGraph>, ResolverError> {
        self.budget
            .charge(self.providers.active.len() + 1, 0, 0)
            .map_err(|r| resolver_error(r, self.budget))?;
        for key in self.providers.active.iter() {
            admit_provider_context(&provider.context, self.budget, false)?;
            admit_provider_context(&key.context, self.budget, false)?;
            let n = crate::resolver::loaded_source_metadata_retained(&provider.unit.source)
                + key.source.as_str().len()
                + key.qualifiers.iter().map(String::len).sum::<usize>()
                + provider
                    .authorized_qualifiers
                    .iter()
                    .map(String::len)
                    .sum::<usize>();
            self.budget
                .charge(1, n, 0)
                .map_err(|r| resolver_error(r, self.budget))?;
            if let crate::SourceRevision::Disk {
                read_policy,
                path_entry,
                ..
            } = &key.revision
            {
                let mut failure = None;
                let mut visit = |n| {
                    self.budget.charge(1, n, 0).map_err(|r| {
                        failure = Some(r);
                        "provider revision comparison".into()
                    })
                };
                read_policy
                    .visit_recovery_payload(&mut visit)
                    .and_then(|_| path_entry.visit_recovery_payload(&mut visit))
                    .map_err(|_| {
                        resolver_error(
                            failure.unwrap_or(IncompleteReason::MalformedSource),
                            self.budget,
                        )
                    })?;
            }
        }
        if self.providers.active.iter().any(|key| {
            key.source == provider.unit.source.id
                && key.revision == provider.unit.source.revision
                && key.context == provider.context
                && key.qualifiers == provider.authorized_qualifiers
        }) {
            return Ok(Fact::Unknown(UnknownReason::CyclicProvider));
        }
        super::declarations::limit(
            self.providers.active.len() + 1,
            self.budget.limits().max_type_depth,
            "max_type_depth",
        )
        .map_err(|r| resolver_error(r, self.budget))?;
        let metadata = provider_owned_payload(&provider)?;
        let occurrence = OccurrenceId(self.providers.next_occurrence);
        self.providers.next_occurrence = self
            .providers
            .next_occurrence
            .checked_add(1)
            .ok_or_else(|| ResolverError::InvalidRequest("occurrence overflow".into()))?;
        let key = self
            .budget
            .admit_owned(1, metadata, metadata, || ProviderKey {
                source: provider.unit.source.id.clone(),
                revision: provider.unit.source.revision.clone(),
                context: provider.context.clone(),
                qualifiers: provider.authorized_qualifiers.clone(),
            })
            .map_err(|r| resolver_error(r, self.budget))?;
        push_owner(&mut self.providers.active, key, self.budget)?;
        let result = analyze_source_with_providers(
            &provider.unit.source,
            &provider.context,
            self.resolver,
            self.budget,
            self.cancel,
            self.providers,
            true,
            occurrence,
        );
        let result = result.and_then(|mut analysis| {
            if let Some(reason) = analysis.reasons.iter().find(|r| {
                matches!(
                    r,
                    IncompleteReason::Limit { .. } | IncompleteReason::Cancelled
                )
            }) {
                return Err(resolver_error(reason.clone(), self.budget));
            }
            let result = analysis
                .with_budget_mut(self.budget, |analysis, budget| {
                    let site = analysis.facts.site();
                    let mut engine = ProviderEngine {
                        resolver: self.resolver,
                        providers: self.providers,
                        cancel: self.cancel,
                        budget,
                        observations: self.observations,
                        warnings: self.warnings,
                    };
                    engine.prepare_name(&mut analysis.facts, &site, name, pointers, depth + 1)
                })
                .map_err(|r| resolver_error(r, self.budget))?;
            result?;
            Ok(analysis)
        });
        self.providers.active.value_mut().pop();
        let mut analysis = result?;
        append_observations(self.observations, &analysis.observations, self.budget)?;
        append_warnings(self.warnings, &analysis.warnings, self.budget)?;
        if !analysis.facts.interface_complete {
            return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
        }
        self.budget
            .charge(
                analysis.occurrences.len(),
                0,
                analysis.occurrences.len() * size_of::<LoadedSource>(),
            )
            .map_err(|r| resolver_error(r, self.budget))?;
        analysis
            .with_budget_mut(self.budget, |analysis, budget| {
                let metadata = analysis
                    .occurrences
                    .iter()
                    .map(|o| crate::resolver::loaded_source_metadata_retained(&o.source))
                    .sum::<usize>();
                budget
                    .charge(1, metadata, metadata + size_of::<AdmittedSourceBacking>())
                    .map_err(|r| resolver_error(r, budget))?;
                analysis.facts.provider_sources = analysis
                    .occurrences
                    .iter()
                    .map(|o| o.source.clone())
                    .collect();
                analysis.facts.provider_backings.push(
                    provider
                        .backing
                        .as_ref()
                        .expect("validated provider backing")
                        .clone(),
                );
                Ok(())
            })
            .map_err(|r| resolver_error(r, self.budget))??;
        let graph = analysis
            .facts
            .admitted_snapshot(self.budget)
            .and_then(|v| v.into_arc(self.budget))
            .map_err(|r| resolver_error(r, self.budget))?;
        drop(analysis);
        Ok(Fact::Known(graph))
    }
}

impl RootSession<'_, '_> {
    fn resolve_pending_imports(&mut self, activity: Truth) -> Result<(), ResolverError> {
        for index in 0..self.current.len() {
            let imports = std::mem::take(&mut self.current[index].state_mut().pending_imports);
            for identity in imports {
                self.budget
                    .charge(self.physical_paths.len(), 0, 0)
                    .map_err(|r| resolver_error(r, self.budget))?;
                let path = &self
                    .physical_paths
                    .iter()
                    .find(|(id, _)| *id == identity.opener.occurrence)
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("missing import opener occurrence".into())
                    })?
                    .1;
                self.budget
                    .charge(
                        1,
                        identity.name.len() + path.as_os_str().len(),
                        size_of::<super::bindings::InterfaceFacts>()
                            + size_of::<String>()
                            + identity.name.len()
                            + path.as_os_str().len(),
                    )
                    .map_err(|r| resolver_error(r, self.budget))?;
                let qualifiers = vec![identity.name.clone()];
                let site = identity.site.clone();
                let pending = super::bindings::PendingProvider {
                    identity,
                    importer_path: path.clone(),
                    authorized: activity == Truth::True,
                };
                self.current[index].state_mut().interfaces.reserve_exact(1);
                self.current[index]
                    .state_mut()
                    .interfaces
                    .push(super::bindings::InterfaceFacts {
                        qualifiers,
                        site,
                        graph: None,
                        pending,
                        attempted: activity != Truth::True,
                        provided: None,
                        failure: if activity != Truth::True {
                            Some(UnknownReason::UnknownActivity)
                        } else {
                            None
                        },
                    });
            }
        }
        Ok(())
    }

    fn walk_include(&mut self, directive: &ConditionalDirective) -> Result<(), ResolverError> {
        // A reachable hypothetical path is not an authorization to read bytes.
        let name = crate::resolver::include_name(&directive.body);
        self.budget
            .charge(self.occurrences.len() + 1, 0, 0)
            .map_err(|r| resolver_error(r, &self.budget))?;
        let mapping = &self.directive_fragments[self.current_directive];
        let anchor = mapping.fragments.first().expect("include delimiter");
        let owner = anchor.occurrence;
        let source = if owner == self.occurrence {
            &self.root
        } else {
            &self
                .occurrences
                .iter()
                .find(|o| o.id == owner)
                .expect("completed physical opener")
                .source
        };
        let including_path = &source.path;
        self.budget
            .charge(
                1,
                directive.body.len(),
                size_of::<IncludeOccurrence>() + self.root.id.as_str().len(),
            )
            .map_err(|r| resolver_error(r, &self.budget))?;
        let outcome = if directive.activity != Truth::True {
            ResolutionOutcome {
                result: Resolution::Unavailable {
                    reason: "include conditional activity is unknown".into(),
                },
                observations: vec![],
                warnings: vec![],
            }
        } else if self.active_sources.len() > self.resolver.max_include_depth() {
            ResolutionOutcome {
                result: Resolution::Incomplete {
                    reason: "include depth limit reached".into(),
                    candidates: vec![],
                },
                observations: vec![],
                warnings: vec![],
            }
        } else if let Some(name) = name.as_deref() {
            self.budget.external(|budget| {
                self.resolver.include(
                    IncludeResolveRequest {
                        including_path,
                        byte_range: anchor.range.clone(),
                        requested_name: name,
                        legacy_route: None,
                    },
                    budget,
                    self.cancel,
                )
            })?
        } else {
            ResolutionOutcome {
                result: Resolution::Unavailable {
                    reason: "include has no statically known name".into(),
                },
                observations: vec![],
                warnings: vec![],
            }
        };
        let ResolutionOutcome {
            mut result,
            observations,
            warnings,
        } = outcome;
        self.budget
            .charge(
                observations.len() + warnings.len(),
                0,
                observations.len() * size_of::<ResolutionObservation>()
                    + warnings.len() * size_of::<String>(),
            )
            .map_err(|r| resolver_error(r, &self.budget))?;
        for observation in &observations {
            admit_observation(observation, &mut self.budget)?;
        }
        for warning in &warnings {
            self.budget
                .charge(1, warning.len(), warning.len())
                .map_err(|r| resolver_error(r, &self.budget))?;
        }
        append_observations(&mut self.observations, &observations, self.budget)?;
        append_warnings(&mut self.warnings, &warnings, self.budget)?;
        match &result {
            Resolution::Found(source) => {
                self.budget
                    .charge(
                        1,
                        source.id.as_str().len() + source.path.as_os_str().len(),
                        source.id.as_str().len() + source.path.as_os_str().len(),
                    )
                    .map_err(|r| resolver_error(r, &self.budget))?;
            }
            Resolution::Unavailable { reason } => {
                self.budget
                    .charge(1, reason.len(), reason.len())
                    .map_err(|r| resolver_error(r, &self.budget))?;
            }
            Resolution::Ambiguous { candidates } | Resolution::Incomplete { candidates, .. } => {
                self.budget
                    .charge(
                        candidates.len(),
                        0,
                        candidates.len() * size_of::<crate::ResolutionCandidate>(),
                    )
                    .map_err(|r| resolver_error(r, &self.budget))?;
                for candidate in candidates {
                    let n = candidate.path.as_os_str().len()
                        + candidate.declared_name.as_ref().map_or(0, String::len);
                    self.budget
                        .charge(1, n, n)
                        .map_err(|r| resolver_error(r, &self.budget))?;
                }
                if let Resolution::Incomplete { reason, .. } = &result {
                    self.budget
                        .charge(1, reason.len(), reason.len())
                        .map_err(|r| resolver_error(r, &self.budget))?;
                }
            }
        }
        if let Resolution::Found(child) = &result {
            if self.active_sources.contains(&child.id) {
                result = Resolution::Incomplete {
                    reason: "textual include cycle".into(),
                    candidates: vec![],
                };
            }
        }
        let index = self.includes.len();
        let fragments_bytes = mapping
            .fragments
            .iter()
            .map(|a| std::mem::size_of::<SourceAnchor>() + a.source.as_str().len())
            .sum::<usize>();
        self.budget
            .charge(
                mapping.fragments.len(),
                0,
                fragments_bytes + anchor.source.as_str().len(),
            )
            .map_err(|r| resolver_error(r, &self.budget))?;
        self.includes.push(IncludeOccurrence {
            owner,
            directive: anchor.clone(),
            fragments: mapping.fragments.clone(),
            target: result,
            child: None,
        });
        match &self.includes[index].target {
            Resolution::Found(child) => {
                let bytes = child.analysis_bytes();
                let metadata = crate::resolver::loaded_source_metadata_retained(child);
                self.budget
                    .charge(1, metadata, metadata)
                    .map_err(|r| resolver_error(r, &self.budget))?;
                let reserve = bytes
                    .len()
                    .checked_mul(4)
                    .and_then(|n| {
                        n.checked_add(
                            size_of::<OccurrenceAnalysis>()
                                + child.id.as_str().len()
                                + child.path.as_os_str().len(),
                        )
                    })
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("include admission overflow".into())
                    })?;
                self.budget
                    .charge(1, bytes.len(), reserve)
                    .map_err(|r| resolver_error(r, &self.budget))?;
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| {
                        ResolverError::InvalidRequest("include coordinate text is not UTF-8".into())
                    })?
                    .to_owned();
                let child = child.clone();
                let occurrence = OccurrenceId(self.providers.next_occurrence);
                self.budget
                    .charge(
                        1,
                        child.path.as_os_str().len(),
                        size_of::<(OccurrenceId, std::path::PathBuf)>()
                            + child.path.as_os_str().len(),
                    )
                    .map_err(|r| resolver_error(r, self.budget))?;
                self.physical_paths.reserve_exact(1);
                self.physical_paths.push((occurrence, child.path.clone()));
                self.providers.next_occurrence = self
                    .providers
                    .next_occurrence
                    .checked_add(1)
                    .ok_or_else(|| ResolverError::InvalidRequest("occurrence overflow".into()))?;
                self.includes[index].child = Some(occurrence);
                let parent = std::mem::replace(&mut self.root, child);
                let parent_text = std::mem::replace(&mut self.text, text.clone());
                let parent_occurrence = std::mem::replace(&mut self.occurrence, occurrence);
                self.active_sources.push(self.root.id.clone());
                let mut analysis =
                    conditional::analyze_with_source_order(&text, self.context, self.cancel, self);
                if !analysis.complete {
                    self.record(IncompleteReason::MalformedSource);
                }
                if !self.reasons.is_empty() {
                    analysis.complete = false;
                    analysis.inactive_spans.clear();
                    analysis.unknown_spans = if text.is_empty() {
                        vec![]
                    } else {
                        vec![0..text.len()]
                    };
                    analysis.projected_source = text.clone();
                }
                // Only physical coordinates change. Typed paths, scopes, lexer,
                // scalar history, settings and conditional frames stay alive.
                let source = std::mem::replace(&mut self.root, parent);
                self.text = parent_text;
                self.occurrence = parent_occurrence;
                self.active_sources.pop();
                self.occurrences.push(OccurrenceAnalysis {
                    id: occurrence,
                    source,
                    analysis,
                });
            }
            failed => {
                let reason = match failed {
                    Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                        reason.clone()
                    }
                    Resolution::Ambiguous { .. } => "ambiguous required include".into(),
                    Resolution::Found(_) => unreachable!(),
                };
                self.record(IncompleteReason::RequiredSource {
                    source: self.root.id.clone(),
                    reason,
                });
            }
        }
        Ok(())
    }

    fn reachable_count(&self) -> usize {
        self.current.len()
            + self
                .frames
                .iter()
                .map(|f| f.pending.len() + f.joined.len())
                .sum::<usize>()
    }

    fn split(
        &mut self,
        directive: &ConditionalDirective,
    ) -> Result<Vec<ReachablePath>, IncompleteReason> {
        let mut remaining = Vec::new();
        let total = self.reachable_count();
        let mut selected = Vec::new();
        let mut aggregate = None;
        self.budget.charge(
            self.current.len(),
            0,
            self.current.len() * (size_of::<Truth>() + 2 * size_of::<ReachablePath>()),
        )?;
        let mut decisions = Vec::new();
        let mut paths = std::mem::take(&mut self.current);
        for path in &mut paths {
            let condition = evaluate_path_condition(
                &self.directive_fragments[self.current_directive].body_fragments,
                path,
                directive,
                &mut self.budget,
                self.resolver,
                self.providers,
                self.cancel,
                &mut self.observations,
                &mut self.warnings,
                &mut self.resource_error,
            )?;
            aggregate = Some(aggregate.map_or(condition, |prior: Truth| prior.merge(condition)));
            decisions.push(condition);
        }
        self.current = paths;
        let forks = decisions.iter().filter(|d| **d == Truth::Unknown).count();
        super::declarations::limit(
            total + forks,
            self.budget.limits().max_branch_states.min(64),
            "max_branch_states",
        )?;
        let mut clones = Vec::new();
        for (path, decision) in self.current.iter().zip(&decisions) {
            clones.push(if *decision == Truth::Unknown {
                Some(path.snapshot(&mut self.budget)?)
            } else {
                None
            });
        }
        for ((path, condition), clone) in std::mem::take(&mut self.current)
            .into_iter()
            .zip(decisions)
            .zip(clones)
        {
            match condition {
                Truth::True => selected.push(path),
                Truth::False => remaining.push(path),
                Truth::Unknown => {
                    selected.push(path);
                    remaining.push(clone.expect("admitted fork"));
                }
            }
        }
        self.current = selected;
        self.last_condition = aggregate.unwrap_or(Truth::Unknown);
        Ok(remaining)
    }

    fn transition(&mut self, directive: &ConditionalDirective) -> Result<(), IncompleteReason> {
        match directive.kind {
            DirectiveKind::ConditionalStart => {
                super::declarations::limit(
                    self.frames.len() + 1,
                    conditional::MAX_CONDITIONAL_DEPTH,
                    "max_conditional_depth",
                )?;
                self.budget.charge(1, 0, size_of::<ReachableFrame>())?;
                let pending = self.split(directive)?;
                self.frames.push(ReachableFrame {
                    pending,
                    ..Default::default()
                });
            }
            DirectiveKind::ConditionalMiddle => {
                let Some(mut frame) = self.frames.pop() else {
                    return Err(IncompleteReason::MalformedSource);
                };
                if frame.has_else {
                    self.frames.push(frame);
                    return Err(IncompleteReason::MalformedSource);
                }
                frame.joined.append(&mut self.current);
                self.current = std::mem::take(&mut frame.pending);
                let is_else = layout_body_key(&directive.body).eq_ignore_ascii_case("ELSE");
                if is_else {
                    frame.has_else = true;
                }
                // Keep completed arms visible while admitting additional forks.
                self.frames.push(frame);
                if !is_else {
                    let pending = self.split(directive)?;
                    self.frames.last_mut().expect("frame").pending = pending;
                }
            }
            DirectiveKind::ConditionalEnd => {
                let Some(mut frame) = self.frames.pop() else {
                    return Err(IncompleteReason::MalformedSource);
                };
                frame.joined.append(&mut self.current);
                frame.joined.append(&mut frame.pending);
                self.current = frame.joined;
            }
            _ => unreachable!("conditional transition only"),
        }
        Ok(())
    }

    fn record(&mut self, reason: IncompleteReason) {
        if self.fatal.is_some() {
            return;
        }
        let dynamic = match &reason {
            IncompleteReason::RequiredSource { source, reason } => {
                source.as_str().len() + reason.len()
            }
            _ => 0,
        };
        if let Err(reason) = self.budget.charge(
            self.reasons.len() + 1,
            dynamic,
            2 * (size_of::<IncompleteReason>() + dynamic),
        ) {
            self.fatal = Some(reason);
            return;
        }
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason.clone());
        }
        if self.state.failure.is_none() {
            self.state.failure = Some(reason);
        }
    }
}

impl ReachablePath {
    fn poison_options(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        budget.charge(self.state().option_stack.len(), 0, 0)?;
        for saved in &mut self.state_mut().option_stack {
            if let Some(saved) = saved.take() {
                budget.release(saved.recovery_payload_bytes());
            }
        }
        Ok(())
    }
    fn state(&self) -> &CompileTimeState {
        self.environment.typed_state.as_deref().expect("typed path")
    }
    fn state_mut(&mut self) -> &mut CompileTimeState {
        self.environment
            .typed_state
            .as_deref_mut()
            .expect("typed path")
    }
    fn into_state(mut self) -> CompileTimeState {
        let mut state = *self.environment.typed_state.take().expect("typed path");
        state.conditional = Some(Box::new(self.environment));
        state
    }
    fn snapshot(&self, budget: &mut CompileTimeBudget<'_>) -> Result<Self, IncompleteReason> {
        // The environment owns the complete typed continuation, not a context projection.
        let before = budget.manual_retained();
        budget.charge(1, 0, size_of::<Self>())?;
        match self.environment.snapshot_typed(budget) {
            Ok(environment) => Ok(Self { environment }),
            Err(reason) => {
                budget.release(budget.manual_retained().saturating_sub(before));
                Err(reason)
            }
        }
    }
}

fn evaluate_path_condition(
    body_fragments: &[SourceAnchor],
    path: &mut ReachablePath,
    directive: &ConditionalDirective,
    budget: &mut CompileTimeBudget<'_>,
    resolver: &mut dyn CompileTimeResolver,
    providers: &mut ProviderSession,
    cancel: &dyn CancellationToken,
    observations: &mut AdmittedOwned<Vec<ResolutionObservation>>,
    warnings: &mut AdmittedOwned<Vec<String>>,
    resource_error: &mut Option<ResolverError>,
) -> Result<Truth, IncompleteReason> {
    let mut state = path.environment.typed_state.take().expect("typed path");
    let site = state.site();
    let environment = &path.environment;
    let result = conditional::evaluate_source_condition(
        &directive.body,
        environment,
        budget,
        &mut |query, budget| {
            if let ExpressionQuery::SizeOf { argument, range } = &query {
                let before = budget.manual_retained();
                let mapped = (|| {
                    let expression = conditional::condition_expression(&directive.body)
                        .ok_or(IncompleteReason::MalformedSource)?;
                    if expression.get(range.clone()) != Some(*argument) {
                        return Err(IncompleteReason::MalformedSource);
                    }
                    let offset = expression.as_ptr() as usize - directive.body.as_ptr() as usize;
                    let wanted = offset + range.start..offset + range.end;
                    let mut logical = 0;
                    let mut covered = 0;
                    for anchor in body_fragments {
                        budget.charge(1, 0, 0)?;
                        let end = logical + anchor.range.len();
                        covered += end
                            .min(wanted.end)
                            .saturating_sub(logical.max(wanted.start));
                        logical = end;
                    }
                    if covered != range.len() {
                        return Err(IncompleteReason::MalformedSource);
                    }
                    Ok(())
                })();
                budget.release(budget.manual_retained().saturating_sub(before));
                mapped?;
            }
            let mut engine = ProviderEngine {
                resolver,
                providers,
                cancel,
                budget,
                observations,
                warnings,
            };
            if let Err(error) = engine.prepare_query(&mut state, &site, &query, 0) {
                *resource_error = Some(error);
                return Err(IncompleteReason::MalformedSource);
            }
            super::arrays::scalar_query(&state, &site, environment, query, 0, engine.budget)
        },
    );
    path.environment.typed_state = Some(state);
    result
}

impl SourceOrder for RootSession<'_, '_> {
    fn next_directive(
        &mut self,
        source: &str,
        cursor: usize,
    ) -> Result<conditional::SourceOrderStep, ()> {
        if !self.current.is_empty() && self.current.iter().all(|p| p.state().interface_complete) {
            return Ok(conditional::SourceOrderStep {
                text_end: source.len(),
                directive: None,
            });
        }
        let next = match self.scanner.next(
            source,
            cursor,
            &self.root.id,
            self.occurrence,
            &mut self.budget,
            self.cancel,
        ) {
            Ok(next) => next,
            Err(reason) => {
                self.record(reason);
                return Err(());
            }
        };
        for (anchor, text) in next.deferred.iter() {
            if self.reasons.is_empty() {
                for path in &mut self.current {
                    if let Err(reason) = path.state_mut().feed(
                        SourceFragment {
                            source: &anchor.source,
                            occurrence: anchor.occurrence,
                            physical_start: anchor.range.start,
                            text: &text,
                        },
                        Truth::True,
                        &mut self.budget,
                    ) {
                        self.record(reason);
                        return Err(());
                    }
                }
            }
        }
        // Each deferred owner keeps its actual prefix text and moved anchors
        // alive through the feed, then drops payload before refunding credits.
        drop(next.deferred);
        let Some(scanned) = next.directive else {
            return Ok(conditional::SourceOrderStep {
                text_end: next.text_end,
                directive: None,
            });
        };
        if self.directive_count >= conditional::MAX_DIRECTIVES {
            self.record(IncompleteReason::Limit {
                name: "max_directives",
                maximum: conditional::MAX_DIRECTIVES,
            });
            return Err(());
        }
        self.directive_count += 1;
        if !self.admit_directive(scanned.body.len()) {
            return Err(());
        }
        let kind = conditional::directive_kind(&scanned.body);
        if kind == DirectiveKind::Include {
            match self.resolver.admit_include(&mut self.budget, self.cancel) {
                Ok(true) => {}
                Ok(false) => {
                    self.record(IncompleteReason::RequiredSource {
                        source: self.root.id.clone(),
                        reason: "include directive admission limit reached".into(),
                    });
                    return Err(());
                }
                Err(error) => {
                    self.resource_error = Some(error);
                    return Err(());
                }
            }
        }
        let directive = ConditionalDirective {
            kind,
            body: String::new(),
            start: scanned.range.start,
            end: scanned.range.end,
            activity: Truth::Unknown,
            condition: None,
        };
        self.current_directive = self.directive_fragments.len();
        let slots = if self.directive_fragments.len() == self.directive_fragments.capacity() {
            (self.directive_fragments.len() + 1)
                .checked_mul(size_of::<DirectiveFragments>())
                .ok_or_else(|| {
                    self.fatal = Some(IncompleteReason::MalformedSource);
                })?
        } else {
            0
        };
        let copy_slots = if slots != 0 {
            self.directive_fragment_slots
        } else {
            0
        };
        self.directive_fragments
            .grow(self.budget, 1, copy_slots, slots, |values| {
                if slots != 0 {
                    values.reserve_exact(1);
                }
                values.push(DirectiveFragments {
                    completion: self.occurrence,
                    directive,
                    fragments: Vec::new(),
                    body_fragments: Vec::new(),
                });
            })
            .map_err(|r| {
                self.fatal = Some(r);
            })?;
        if slots != 0 {
            self.directive_fragments
                .retire_storage(self.directive_fragment_slots)
                .map_err(|r| {
                    self.fatal = Some(r);
                })?;
            self.directive_fragment_slots = slots;
        }
        self.directive_fragments
            .move_field(scanned.body, self.budget, |values, body| {
                values
                    .last_mut()
                    .expect("physical directive")
                    .directive
                    .body = body;
            })
            .map_err(|r| {
                self.fatal = Some(r);
            })?;
        self.directive_fragments
            .move_field(scanned.fragments, self.budget, |values, fragments| {
                values.last_mut().expect("physical directive").fragments = fragments;
            })
            .map_err(|r| {
                self.fatal = Some(r);
            })?;
        self.directive_fragments
            .move_field(scanned.body_fragments, self.budget, |values, fragments| {
                values
                    .last_mut()
                    .expect("physical directive")
                    .body_fragments = fragments;
            })
            .map_err(|r| {
                self.fatal = Some(r);
            })?;
        // The source-order ConditionalAnalysis needs its own body String. This
        // is a real independent copy, not a second invoice for emitted metadata.
        let physical = &self.directive_fragments[self.current_directive].directive;
        self.budget
            .charge(1, physical.body.len(), physical.body.len())
            .map_err(|r| {
                self.fatal = Some(r);
            })?;
        let directive = physical.clone();
        Ok(conditional::SourceOrderStep {
            text_end: next.text_end,
            directive: Some(directive),
        })
    }
    fn activity(&self) -> Truth {
        if !self.reasons.is_empty() || self.fatal.is_some() {
            Truth::Unknown
        } else if self.current.is_empty() {
            Truth::False
        } else if self
            .frames
            .iter()
            .all(|frame| frame.pending.is_empty() && frame.joined.is_empty())
        {
            Truth::True
        } else {
            Truth::Unknown
        }
    }
    fn admit_directive(&mut self, body_bytes: usize) -> bool {
        if self.fatal.is_some() {
            return false;
        }
        // Interpreter directive/mask/activity slots and conservative frame/key
        // controls. Scanner body/anchor payload and physical Vec backing have
        // their own receipts; the one independent source-order body copy is
        // admitted separately immediately before cloning it.
        let retained = 3 * size_of::<ConditionalDirective>()
            + 8 * size_of::<Range<usize>>()
            + 4 * size_of::<String>()
            + size_of::<ConditionalEnvironment>();
        match self.budget.charge(1, body_bytes, retained) {
            Ok(()) => true,
            Err(reason) => {
                self.fatal = Some(reason);
                false
            }
        }
    }

    fn admit_environment(&mut self, environment: &ConditionalEnvironment) -> bool {
        if self.fatal.is_some() {
            return false;
        }
        let bytes = environment.recovery_payload_bytes();
        match self.budget.charge(1, bytes, bytes) {
            Ok(()) => true,
            Err(reason) => {
                self.fatal = Some(reason);
                false
            }
        }
    }
    fn feed(&mut self, range: Range<usize>, activity: Truth) {
        if self.fatal.is_some() || self.state.failure.is_some() || activity == Truth::False {
            return;
        }
        let mut failure = None;
        for path in &mut self.current {
            if let Err(reason) = path.state_mut().feed(
                SourceFragment {
                    source: &self.root.id,
                    occurrence: self.occurrence,
                    physical_start: range.start,
                    text: &self.text[range.clone()],
                },
                Truth::True,
                &mut self.budget,
            ) {
                failure = Some(reason);
                break;
            }
        }
        if let Some(reason) = failure {
            self.record(reason);
        }
        if let Err(error) = self.resolve_pending_imports(activity) {
            self.resource_error = Some(error);
        }
    }

    fn condition(
        &mut self,
        _body: &str,
        _start: usize,
        _environment: &ConditionalEnvironment,
        _complete: &mut bool,
    ) -> Truth {
        if self.fatal.is_some() || !self.reasons.is_empty() {
            Truth::Unknown
        } else {
            self.last_condition
        }
    }

    fn directive(&mut self, directive: &ConditionalDirective) {
        self.directive_fragments.value_mut()[self.current_directive]
            .directive
            .activity = directive.activity;
        if !self.current.is_empty() && self.current.iter().all(|p| p.state().interface_complete) {
            return;
        }
        if self.fatal.is_some() || !self.reasons.is_empty() {
            return;
        }
        if matches!(
            directive.kind,
            DirectiveKind::ConditionalStart
                | DirectiveKind::ConditionalMiddle
                | DirectiveKind::ConditionalEnd
        ) {
            if let Err(reason) = self.transition(directive) {
                self.record(reason);
            }
            if matches!(
                directive.kind,
                DirectiveKind::ConditionalStart | DirectiveKind::ConditionalMiddle
            ) && !layout_body_key(&directive.body).eq_ignore_ascii_case("ELSE")
            {
                self.directive_fragments.value_mut()[self.current_directive]
                    .directive
                    .condition = Some(self.last_condition);
            }
            return;
        }
        if self.current.is_empty() {
            return;
        }
        if directive.kind == DirectiveKind::Include {
            if let Err(error) = self.walk_include(directive) {
                self.resource_error = Some(error);
                self.record(IncompleteReason::RequiredSource {
                    source: self.root.id.clone(),
                    reason: "include admission failed".into(),
                });
            }
        } else {
            let mut failure = None;
            for path in &mut self.current {
                if let Err(reason) = apply_path_directive(path, directive, &mut self.budget) {
                    failure = Some(reason);
                    break;
                }
            }
            if let Some(reason) = failure {
                self.record(reason);
            }
        }
    }
}

fn apply_path_directive(
    path: &mut ReachablePath,
    directive: &ConditionalDirective,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), IncompleteReason> {
    // Bound shared-decoder scratch and scan/copy work before parsing switch
    // lists or invoking the legacy scalar mutator's independent safety limits.
    let bytes = directive.body.len();
    let scratch = bytes.saturating_mul(2 * size_of::<&str>());
    budget.charge(
        bytes.saturating_mul(8).saturating_add(1),
        bytes.saturating_mul(32),
        scratch,
    )?;
    let result = apply_path_directive_inner(path, directive, budget);
    budget.release(scratch);
    result
}

fn apply_path_directive_inner(
    path: &mut ReachablePath,
    directive: &ConditionalDirective,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), IncompleteReason> {
    let key = layout_body_key(&directive.body);
    let value = directive.body.trim().get(key.len()..).unwrap_or("!").trim();
    let supported = value.is_empty()
        && path.state().context.compiler_version
            == Some(pascal_project::CompilerVersion::new(37, 0))
        && matches!(
            path.state().context.layout.platform,
            Some(pascal_project::LayoutPlatform::Win32 | pascal_project::LayoutPlatform::Win64)
        );
    if key.eq_ignore_ascii_case("PUSHOPT") {
        super::declarations::limit(
            path.state().option_stack.len() + 1,
            budget.limits().max_scope_depth,
            "max_scope_depth",
        )?;
        budget.charge(1, 0, size_of::<Option<ConditionalEnvironment>>())?;
        let saved = if supported {
            Some(path.environment.snapshot_scalar(budget)?)
        } else {
            path.poison_options(budget)?;
            None
        };
        path.state_mut().option_stack.push(saved);
    } else if key.eq_ignore_ascii_case("POPOPT")
        || key.eq_ignore_ascii_case("POP")
        || key.eq_ignore_ascii_case("PUSH")
    {
        let saved = if key.eq_ignore_ascii_case("POPOPT") {
            let popped = path.state_mut().option_stack.pop();
            if popped.is_some() {
                budget.release(size_of::<Option<ConditionalEnvironment>>());
            }
            popped.flatten()
        } else {
            None
        };
        let restored = conditional::restore_source_options(
            &mut path.environment,
            if supported && key.eq_ignore_ascii_case("POPOPT") {
                saved.as_ref()
            } else {
                None
            },
            budget,
        );
        if let Some(saved) = &saved {
            budget.release(saved.recovery_payload_bytes());
        }
        restored?;
        if !supported || saved.is_none() {
            path.poison_options(budget)?;
        }
    }
    conditional::apply_source_directive(&mut path.environment, directive, budget)?;
    if matches!(directive.kind, DirectiveKind::Define | DirectiveKind::Undef)
        || conditional::is_option_directive(&directive.body)
    {
        let scalar = path.environment.snapshot_scalar(budget)?;
        budget.charge(
            1,
            0,
            size_of::<Sequence>() + size_of::<ConditionalEnvironment>(),
        )?;
        let state = path.state_mut();
        state.sequence = Sequence(
            state
                .sequence
                .0
                .checked_add(1)
                .ok_or(IncompleteReason::MalformedSource)?,
        );
        state.scalar_history.push((state.sequence, scalar));
    }
    {
        if let Some(members) = conditional::switch_list_members(&directive.body) {
            for member in members {
                if is_layout_body(member) {
                    path.state_mut()
                        .apply_layout_directive(member, Truth::True, budget)?;
                }
            }
        } else if let Some(members) = conditional::switch_list_candidates(&directive.body) {
            // Use the existing decoder's invalid-list candidate rules. An
            // invalid list cannot leave a dependent layout switch precise.
            for member in members {
                if is_layout_body(member) {
                    path.state_mut()
                        .apply_layout_directive(member, Truth::Unknown, budget)?;
                }
            }
        } else if is_layout_body(&directive.body) {
            if directive.body.contains(',') {
                // Numeric/mixed lists have no verified list grammar here;
                // invalidate each named family, never keep a stale H/Z fact.
                for member in directive.body.split(',') {
                    if is_layout_body(member) {
                        path.state_mut()
                            .apply_layout_directive(member, Truth::Unknown, budget)?;
                    }
                }
            } else {
                path.state_mut()
                    .apply_layout_directive(&directive.body, Truth::True, budget)?;
            }
        }
    }
    Ok(())
}

fn is_layout_body(body: &str) -> bool {
    let key = layout_body_key(body);
    [
        "A",
        "ALIGN",
        "Z",
        "MINENUMSIZE",
        "H",
        "LONGSTRINGS",
        "OLDTYPELAYOUT",
        "REALCOMPATIBILITY",
        "PUSHOPT",
        "POPOPT",
        "PUSH",
        "POP",
    ]
    .iter()
    .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

fn layout_body_key(body: &str) -> &str {
    let body = body.trim().strip_prefix('$').unwrap_or(body.trim());
    let end = body
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(body.len());
    &body[..end]
}

#[cfg(test)]
fn argument_anchor(
    root: &LoadedSource,
    occurrence: OccurrenceId,
    text: &str,
    body: &str,
    directive_start: usize,
    range: Range<usize>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<SourceAnchor, IncompleteReason> {
    let prefix = if text
        .get(directive_start..)
        .is_some_and(|s| s.starts_with("{$"))
    {
        2
    } else if text
        .get(directive_start..)
        .is_some_and(|s| s.starts_with("(*$"))
    {
        3
    } else {
        return Err(IncompleteReason::MalformedSource);
    };
    let expression =
        conditional::condition_expression(body).ok_or(IncompleteReason::MalformedSource)?;
    let offset = (expression.as_ptr() as usize)
        .checked_sub(body.as_ptr() as usize)
        .ok_or(IncompleteReason::MalformedSource)?;
    let start = directive_start
        .checked_add(prefix)
        .and_then(|n| n.checked_add(offset))
        .ok_or(IncompleteReason::MalformedSource)?;
    let physical = start
        .checked_add(range.start)
        .ok_or(IncompleteReason::MalformedSource)?
        ..start
            .checked_add(range.end)
            .ok_or(IncompleteReason::MalformedSource)?;
    budget.charge(
        1,
        range.len() + root.id.as_str().len(),
        size_of::<SourceAnchor>() + root.id.as_str().len(),
    )?;
    let argument = expression
        .get(range)
        .ok_or(IncompleteReason::MalformedSource)?;
    if text.get(physical.clone()) != Some(argument) {
        return Err(IncompleteReason::MalformedSource);
    }
    Ok(SourceAnchor {
        source: root.id.clone(),
        occurrence,
        range: physical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NoCancellation, SourceId, SourceRevision};

    #[test]
    fn actual_final_sidebuffer_publication_moves_backing_and_receipts() {
        struct NoCallbacks;
        impl CompileTimeResolver for NoCallbacks {
            fn include(
                &mut self,
                _: IncludeResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                panic!("no include in this root");
            }
            fn unit(
                &mut self,
                _: UnitResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                panic!("no uses in this root");
            }
        }
        let text = "unit T; interface {$H+} type R=record X:Byte; end; implementation end.";
        let root = LoadedSource {
            id: SourceId::new("T.pas"),
            path: "T.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            compile_time_backing: None,
        };
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let view = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = view.budget_snapshot().retained_bytes;
        let mut providers = ProviderSession::new(&mut budget).unwrap();
        let mut analysis = analyze_source_with_providers(
            &root,
            &ConditionalContext::default(),
            &mut NoCallbacks,
            &mut budget,
            &NoCancellation,
            &mut providers,
            false,
            OccurrenceId(0),
        )
        .unwrap();
        let observation = ResolutionObservation::Directory {
            path: "observed".into(),
            entry: pascal_project::ProjectPathEntry {
                path: "observed".into(),
                provenance: pascal_project::ProjectPathProvenance::Configured,
            },
            stamp: None,
            complete: true,
        };
        append_observations(
            &mut analysis.value_mut().observations,
            &[observation.clone()],
            &mut budget,
        )
        .unwrap();
        append_warnings(
            &mut analysis.value_mut().warnings,
            &["warning".repeat(128)],
            &mut budget,
        )
        .unwrap();
        let observations_ptr = analysis.observations.as_ptr();
        let warnings_ptr = analysis.warnings.as_ptr();
        let warning_text_ptr = analysis.warnings[0].as_ptr();
        let physical_ptr = analysis.directive_fragments.as_ptr();
        let fragment_ptr = analysis.directive_fragments[0].fragments.as_ptr();
        let body_fragment_ptr = analysis.directive_fragments[0].body_fragments.as_ptr();
        let directive_body_ptr = analysis.directive_fragments[0].directive.body.as_ptr();
        struct CancelAt(std::cell::Cell<usize>, usize);
        impl CancellationToken for CancelAt {
            fn is_cancelled(&self) -> bool {
                let n = self.0.get();
                self.0.set(n + 1);
                n >= self.1
            }
        }
        // Actual final publication refusal before graph promotion, during
        // destination controls and before taking either independent side field.
        // The consumed failed analysis must refund only its dead owners.
        for allowed in 0..8 {
            let before = view.budget_snapshot();
            let mut failed = analyze_source_with_providers(
                &root,
                &ConditionalContext::default(),
                &mut NoCallbacks,
                &mut budget,
                &NoCancellation,
                &mut providers,
                false,
                OccurrenceId(0),
            )
            .unwrap();
            append_warnings(
                &mut failed.value_mut().warnings,
                &["cancelled-warning".repeat(128)],
                &mut budget,
            )
            .unwrap();
            let cancel = CancelAt(std::cell::Cell::new(0), allowed);
            let mut cancelled_budget = view.store_budget(CompileTimeLimits::default(), &cancel);
            assert!(matches!(
                publish_analysis(failed, &mut cancelled_budget),
                Err(ResolverError::Cancelled)
            ));
            let after = view.budget_snapshot();
            assert_eq!(
                after.retained_bytes, before.retained_bytes,
                "publication cancel boundary {allowed}"
            );
            assert!(after.work >= before.work);
            assert!(after.byte_work >= before.byte_work);
        }
        let mut facts_copy = 0;
        analysis
            .facts
            .visit_snapshot_payload(&mut |n| {
                facts_copy += n;
                Ok(())
            })
            .unwrap();
        // admitted_snapshot's carrier constructor separately admits the inline
        // state/control floor; its byte-work invoice is owned backing only.
        facts_copy -= size_of::<CompileTimeState>();
        let remaining_output_copy = conditional_output_payload(&analysis.root).unwrap()
            + analysis
                .occurrences
                .iter()
                .map(|o| {
                    size_of::<OccurrenceAnalysis>()
                        + crate::resolver::loaded_source_metadata_retained(&o.source)
                        + conditional_output_payload(&o.analysis).unwrap()
                })
                .sum::<usize>();
        let before_publication = budget.snapshot();
        let output = publish_analysis(analysis, &mut budget).unwrap();
        assert_eq!(
            budget.snapshot().byte_work - before_publication.byte_work,
            facts_copy + remaining_output_copy,
            "publication copies the still-plain facts/root, not emitted physical metadata or sidebuffers"
        );
        assert_eq!(output.directive_fragments.as_ptr(), physical_ptr);
        assert_eq!(
            output.directive_fragments[0].fragments.as_ptr(),
            fragment_ptr
        );
        assert_eq!(
            output.directive_fragments[0].body_fragments.as_ptr(),
            body_fragment_ptr
        );
        assert_eq!(
            output.directive_fragments[0].directive.body.as_ptr(),
            directive_body_ptr
        );
        assert_eq!(
            output.observations.as_ptr(),
            observations_ptr,
            "move the actual observation buffer"
        );
        assert_eq!(
            output.warnings.as_ptr(),
            warnings_ptr,
            "move the actual warning buffer"
        );
        assert_eq!(
            output.warnings[0].as_ptr(),
            warning_text_ptr,
            "move owned warning text, not clone"
        );
        assert_eq!(output.observations, [observation]);
        let snapshot = output
            .facts
            .try_clone(&mut budget)
            .unwrap()
            .into_arc(&mut budget)
            .unwrap();
        drop(providers);
        drop(budget);
        assert_eq!(output.warnings[0], "warning".repeat(128));
        assert_eq!(output.directive_fragments[0].directive.body, "H+");
        assert_eq!(
            output.directive_fragments[0].fragments[0].occurrence,
            OccurrenceId(0)
        );
        let live = view.budget_snapshot();
        drop(output);
        assert!(
            view.budget_snapshot().retained_bytes > baseline,
            "shared final facts remain live"
        );
        assert!(
            snapshot
                .declarations
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case("R"))
        );
        drop(snapshot);
        let dead = view.budget_snapshot();
        assert_eq!(dead.retained_bytes, baseline);
        assert_eq!(dead.work, live.work);
        assert_eq!(dead.byte_work, live.byte_work);
    }

    #[test]
    fn actual_interface_replacement_copies_once_and_preserves_old_snapshot_last_owner() {
        struct NoCallbacks;
        impl CompileTimeResolver for NoCallbacks {
            fn include(
                &mut self,
                _: IncludeResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                panic!("this interface has no external include");
            }
            fn unit(
                &mut self,
                _: UnitResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                panic!("this interface has no external unit");
            }
        }
        let text = "unit P; interface type R=record X:Byte; end; implementation end.";
        let root = LoadedSource {
            id: SourceId::new("P.pas"),
            path: "P.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            compile_time_backing: None,
        };
        let context = ConditionalContext::default();
        let mut resolver = NoCallbacks;
        let analysis = analyze_source(
            &root,
            &context,
            &mut resolver,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
        .unwrap();
        let view = analysis.budget.clone();
        let mut budget = view.store_budget(CompileTimeLimits::default(), &NoCancellation);
        let old = analysis
            .facts
            .try_clone(&mut budget)
            .unwrap()
            .into_arc(&mut budget)
            .unwrap();
        let old_snapshot = old.clone();
        let mut payload = 0;
        old.visit_snapshot_payload(&mut |n| {
            payload += n;
            Ok(())
        })
        .unwrap();
        drop(analysis);
        let mut providers = ProviderSession::new(&mut budget).unwrap();
        let mut observations = budget.admit_owned(0, 0, 0, Vec::new).unwrap();
        let mut warnings = budget.admit_owned(0, 0, 0, Vec::new).unwrap();
        let before = view.budget_snapshot();
        let mut engine = ProviderEngine {
            resolver: &mut resolver,
            providers: &mut providers,
            cancel: &NoCancellation,
            budget: &mut budget,
            observations: &mut observations,
            warnings: &mut warnings,
        };
        let replacement = engine.replace_interface_graph(&old, "R", false, 1).unwrap();
        let after = view.budget_snapshot();
        // This is the actual replacement boundary, not a primitive stand-in:
        // one mutable independent graph copy is required; sharing it performs
        // no second payload traversal/copy or payload-byte-work admission.
        assert_eq!(after.byte_work - before.byte_work, payload);
        assert!(!Arc::ptr_eq(&old, &replacement));
        assert_eq!(old.fingerprint(), replacement.fingerprint());
        assert!(
            replacement
                .declarations
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case("R"))
        );
        struct CancelAt {
            calls: std::cell::Cell<usize>,
            allowed: usize,
        }
        impl CancellationToken for CancelAt {
            fn is_cancelled(&self) -> bool {
                let n = self.calls.get();
                self.calls.set(n + 1);
                n >= self.allowed
            }
        }
        let mut visits = 0;
        old.visit_snapshot_payload(&mut |_| {
            visits += 1;
            Ok(())
        })
        .unwrap();
        // Refusal before construction, during snapshot traversal, after the
        // mutable copy, and after shared promotion must destroy only the new
        // owner. Existing old and newly published snapshots remain independent.
        for allowed in [0, 1, 2, visits / 2 + 2, visits + 2, visits + 3] {
            let cancel = CancelAt {
                calls: std::cell::Cell::new(0),
                allowed,
            };
            let mut cancelled_budget = view.store_budget(CompileTimeLimits::default(), &cancel);
            let before_error = view.budget_snapshot();
            let mut engine = ProviderEngine {
                resolver: &mut resolver,
                providers: &mut providers,
                cancel: &cancel,
                budget: &mut cancelled_budget,
                observations: &mut observations,
                warnings: &mut warnings,
            };
            assert!(
                matches!(
                    engine.replace_interface_graph(&old, "R", false, 1),
                    Err(ResolverError::Cancelled)
                ),
                "cancel boundary {allowed}"
            );
            let after_error = view.budget_snapshot();
            assert_eq!(
                after_error.retained_bytes, before_error.retained_bytes,
                "temporary replacement owner must die at cancel boundary {allowed}"
            );
            assert!(after_error.work >= before_error.work);
            assert!(after_error.byte_work >= before_error.byte_work);
        }
        drop(old);
        assert_eq!(
            view.budget_snapshot().retained_bytes,
            after.retained_bytes,
            "another continuation still holds the old graph snapshot"
        );
        drop(old_snapshot);
        assert!(view.budget_snapshot().retained_bytes < after.retained_bytes);
        drop(providers);
        drop(observations);
        drop(warnings);
        drop(budget);
        // The replacement has no borrowed-budget/token lifetime dependency.
        assert!(
            replacement
                .declarations
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case("R"))
        );
        let live = view.budget_snapshot();
        drop(replacement);
        let dead = view.budget_snapshot();
        assert!(dead.retained_bytes < live.retained_bytes);
        assert_eq!(dead.work, live.work);
        assert_eq!(dead.byte_work, live.byte_work);
    }

    #[test]
    fn actual_declaration_mutation_survives_unwind_with_immediate_owner_credits() {
        let text = "unit T; interface ";
        let root = LoadedSource {
            id: SourceId::new("T.pas"),
            path: "T.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            compile_time_backing: None,
        };
        let context = ConditionalContext::default();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let mut graph = budget
            .admit_compilation(0, |budget| {
                let mut state = CompileTimeState::new(&context, &root, budget)
                    .map_err(|r| resolver_error(r, budget))?;
                state
                    .feed(
                        SourceFragment {
                            source: &root.id,
                            occurrence: OccurrenceId(0),
                            physical_start: 0,
                            text,
                        },
                        Truth::True,
                        budget,
                    )
                    .map_err(|r| resolver_error(r, budget))?;
                Ok(state)
            })
            .unwrap();
        let before = observer.budget_snapshot();
        let extra = "type Persisted = record X: Byte; end; type Pending = record Y:";
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            graph
                .with_budget_mut(&mut budget, |state, budget| {
                    state
                        .feed(
                            SourceFragment {
                                source: &root.id,
                                occurrence: OccurrenceId(0),
                                physical_start: text.len(),
                                text: extra,
                            },
                            Truth::True,
                            budget,
                        )
                        .unwrap();
                    panic!("after actual declaration and pending-parser mutation");
                })
                .unwrap();
        }));
        assert!(unwind.is_err());
        assert!(
            graph
                .declarations
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case("Persisted"))
        );
        let live = observer.budget_snapshot();
        assert!(live.retained_bytes > before.retained_bytes);
        assert!(live.work > before.work);
        assert!(live.byte_work > before.byte_work);
        // Work after the externally caught unwind must not be attributed to the
        // old graph. A fresh real parser owner is independent of it.
        let another = budget
            .admit_compilation(0, |budget| {
                CompileTimeState::new(&context, &root, budget)
                    .map_err(|r| resolver_error(r, budget))
            })
            .unwrap();
        drop(another);
        assert_eq!(
            observer.budget_snapshot().retained_bytes,
            live.retained_bytes
        );
        drop(graph);
        let dead = observer.budget_snapshot();
        assert_eq!(dead.retained_bytes, baseline);
        assert!(dead.work >= live.work);
        assert!(dead.byte_work >= live.byte_work);
    }

    #[test]
    fn root_metadata_invoice_refunds_when_coordinator_control_admission_refuses() {
        let text = "unit T; interface implementation end.";
        let root = LoadedSource {
            id: SourceId::new("T.pas"),
            path: "T.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            compile_time_backing: None,
        };
        struct NoResources;
        impl CompileTimeResolver for NoResources {
            fn unit(
                &mut self,
                _: UnitResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                panic!("control refusal must precede callback");
            }
            fn include(
                &mut self,
                _: IncludeResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                panic!("control refusal must precede callback");
            }
        }
        let metadata = crate::resolver::loaded_source_metadata_retained(&root);
        // The original metadata fits exactly. No compilation control can fit.
        let mut budget = CompileTimeBudget::new(
            CompileTimeLimits {
                max_retained_bytes: 2 * metadata,
                ..Default::default()
            },
            &NoCancellation,
        );
        let result = analyze_source_in_session(
            &root,
            &ConditionalContext::default(),
            &mut NoResources,
            &mut budget,
            &NoCancellation,
        );
        assert!(matches!(
            result,
            Err(ResolverError::LimitExceeded {
                limit: "max_retained_bytes",
                ..
            })
        ));
        assert_eq!(budget.snapshot().retained_bytes, 0);
        assert!(budget.snapshot().work > 0);
        assert_eq!(budget.snapshot().byte_work, metadata);
    }

    #[test]
    fn typed_environment_snapshot_retains_partial_graph_maps_options_and_provenance() {
        let text = "unit T; interface type R=packed record X:";
        let root = LoadedSource {
            id: SourceId::new("/fixture/snapshot.pas"),
            path: "/fixture/snapshot.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            compile_time_backing: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
        };
        let context = ConditionalContext::default()
            .with_layout(pascal_project::LayoutContext {
                platform: Some(pascal_project::LayoutPlatform::Win32),
                ..Default::default()
            })
            .with_compiler_version(pascal_project::CompilerVersion::new(37, 0));
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut state = CompileTimeState::new(&context, &root, &mut budget).unwrap();
        state
            .feed(
                SourceFragment {
                    source: &root.id,
                    occurrence: OccurrenceId(3),
                    physical_start: 0,
                    text,
                },
                Truth::True,
                &mut budget,
            )
            .unwrap();
        let mut environment = ConditionalEnvironment::from_context(&context);
        environment.typed_state = Some(Box::new(state));
        let mut path = ReachablePath { environment };
        for body in ["B+", "PUSHOPT", "B-"] {
            apply_path_directive(
                &mut path,
                &ConditionalDirective {
                    kind: DirectiveKind::Other,
                    body: body.into(),
                    start: text.len(),
                    end: text.len(),
                    activity: Truth::True,
                    condition: None,
                },
                &mut budget,
            )
            .unwrap();
        }
        let identity = path.environment.fingerprint();
        let bytes = path.environment.recovery_payload_bytes();
        let before = budget.snapshot();
        let mut other = path.snapshot(&mut budget).unwrap();
        assert_eq!(identity, other.environment.fingerprint());
        assert_eq!(bytes, other.environment.recovery_payload_bytes());
        assert!(budget.snapshot().work > before.work);
        assert!(budget.snapshot().byte_work > before.byte_work);
        assert!(budget.snapshot().retained_bytes >= before.retained_bytes + bytes);
        assert_eq!(other.environment.to_context().layout, context.layout);
        assert!(other.environment.to_context().layout_explicit);
        let projected = ConditionalEnvironment::from_context(&other.environment.to_context());
        assert!(projected.typed_state.is_none());
        assert_ne!(identity, projected.fingerprint());
        for (path, suffix) in [(&mut path, "Byte; end;"), (&mut other, "Int64; end;")] {
            path.state_mut()
                .feed(
                    SourceFragment {
                        source: &root.id,
                        occurrence: OccurrenceId(3),
                        physical_start: text.len(),
                        text: suffix,
                    },
                    Truth::True,
                    &mut budget,
                )
                .unwrap();
        }
        assert_ne!(
            path.environment.fingerprint(),
            other.environment.fingerprint()
        );
        for (path, size) in [(&path, 1), (&other, 8)] {
            let state = path.state();
            let layout = state
                .layout_name(state.site(), "R", &mut budget)
                .known()
                .unwrap();
            assert_eq!(layout.storage.size, size);
            let TypeIdentity::Declared(key) = layout.identity else {
                panic!("declared graph lost");
            };
            assert_eq!(key.anchor.occurrence, OccurrenceId(3));
            assert_eq!(key.anchor.source, root.id);
        }
        let mut denied = CompileTimeBudget::new(
            CompileTimeLimits {
                max_retained_bytes: 0,
                ..Default::default()
            },
            &NoCancellation,
        );
        let identity = path.environment.fingerprint();
        assert!(matches!(
            path.snapshot(&mut denied),
            Err(IncompleteReason::Limit {
                name: "max_retained_bytes",
                ..
            })
        ));
        assert_eq!(denied.snapshot().retained_bytes, 0);
        assert_eq!(path.environment.fingerprint(), identity);
        struct CancelDuringSnapshot(std::cell::Cell<usize>);
        impl CancellationToken for CancelDuringSnapshot {
            fn is_cancelled(&self) -> bool {
                let calls = self.0.get() + 1;
                self.0.set(calls);
                calls >= 3
            }
        }
        let cancel = CancelDuringSnapshot(std::cell::Cell::new(0));
        let mut cancelled = CompileTimeBudget::new(CompileTimeLimits::default(), &cancel);
        assert!(matches!(
            path.snapshot(&mut cancelled),
            Err(IncompleteReason::Cancelled)
        ));
        assert_eq!(cancelled.snapshot().retained_bytes, 0);
        assert!(cancelled.snapshot().work > 0);
        assert_eq!(path.environment.fingerprint(), identity);
    }

    #[test]
    fn sizeof_argument_ranges_map_to_original_brace_and_paren_occurrences() {
        for directive in [
            "{$IF   SizeOf( value.Field )=2}",
            "(*$IF   SizeOf( value.Field )=2*)",
        ] {
            let text = format!("unit T; interface {directive} implementation end.");
            let root = LoadedSource {
                id: SourceId::new("/fixture/physical.pas"),
                path: "/fixture/physical.pas".into(),
                bytes: text.as_bytes().into(),
                decoded_text: Some(text.clone().into()),
                compile_time_backing: None,
                revision: SourceRevision::Overlay {
                    version: 1,
                    content_hash: 1,
                },
            };
            let start = text.find(directive).unwrap();
            let prefix = if directive.starts_with('{') { 2 } else { 3 };
            let suffix = if directive.starts_with('{') { 1 } else { 2 };
            let body = &text[start + prefix..start + directive.len() - suffix];
            let expression = conditional::condition_expression(body).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let mut seen = 0;
            let result = conditional::evaluate_layout_expression(
                expression,
                &ConditionalEnvironment::default(),
                &mut budget,
                &mut |query, budget| {
                    if matches!(query, ExpressionQuery::LexicalName(_, _)) {
                        return Ok(None);
                    }
                    let ExpressionQuery::SizeOf { argument, range } = query else {
                        panic!("generic evaluator consumed the operand");
                    };
                    seen += 1;
                    assert_eq!(argument, " value.Field ");
                    assert_eq!(&expression[range.clone()], argument);
                    let anchor =
                        argument_anchor(&root, OccurrenceId(0), &text, body, start, range, budget)?;
                    assert_eq!(anchor.source, root.id);
                    assert_eq!(anchor.occurrence, OccurrenceId(0));
                    let physical = text.find(" value.Field ").unwrap();
                    assert_eq!(anchor.range, physical..physical + argument.len());
                    Ok(Some(BoundValue::Constant(ConstantValue::Integer(2))))
                },
                false,
                true,
            )
            .unwrap();
            assert!(matches!(
                result,
                Some(BoundValue::Constant(ConstantValue::Boolean(true)))
            ));
            assert_eq!(seen, 1);
        }
    }

    #[test]
    fn captured_initializer_maps_disjoint_root_fragments_without_wrapper_offsets() {
        let text = "unit T; interface const N=SizeOf( {$IF true}Byte{$ENDIF} ); {$IF N=1}{$DEFINE HIT}{$ENDIF} implementation end.";
        let root = LoadedSource {
            id: SourceId::new("/fixture/fragments.pas"),
            path: "/fixture/fragments.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            compile_time_backing: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
        };
        struct NoResources;
        impl CompileTimeResolver for NoResources {
            fn include(
                &mut self,
                _: IncludeResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                panic!("root-only session read resources");
            }
            fn unit(
                &mut self,
                _: UnitResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                panic!("root-only session read resources");
            }
        }
        let context = ConditionalContext::default()
            .with_compiler_version(pascal_project::CompilerVersion::new(37, 0))
            .with_layout(pascal_project::LayoutContext {
                platform: Some(pascal_project::LayoutPlatform::Win32),
                ..Default::default()
            });
        let result = analyze_source(
            &root,
            &context,
            &mut NoResources,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
        .unwrap();
        assert!(result.root.complete, "{:?}", result.reasons);
        assert_eq!(
            result
                .root
                .directives
                .iter()
                .find(|d| d.body == "DEFINE HIT")
                .unwrap()
                .activity,
            Truth::True
        );
        let decl = result
            .facts
            .declarations
            .iter()
            .find(|d| d.name == "n")
            .unwrap();
        let raw = decl.initializer.as_deref().unwrap();
        let expression = super::super::declarations::initializer_expression(raw);
        let offset = expression.as_ptr() as usize - raw.as_ptr() as usize;
        let arg_start = expression.find('(').unwrap() + 1;
        let arg_end = expression.find(')').unwrap();
        let logical = decl.initializer_logical.unwrap() + offset;
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let anchors = result
            .facts
            .provenance(logical + arg_start, logical + arg_end, &mut budget)
            .unwrap();
        assert!(anchors.len() >= 3, "{anchors:?}");
        let mapped = anchors
            .iter()
            .map(|a| {
                assert_eq!(a.source, root.id);
                assert_eq!(a.occurrence, OccurrenceId(0));
                &text[a.range.clone()]
            })
            .collect::<String>();
        assert_eq!(mapped, &expression[arg_start..arg_end]);
        let byte_anchor = anchors
            .iter()
            .find(|a| &text[a.range.clone()] == "Byte")
            .unwrap();
        let byte_start = text.find("Byte").unwrap();
        assert_eq!(byte_anchor.range, byte_start..byte_start + 4);
    }
}

/// Per-path bridge used by IF and declaration-captured constant expressions.
/// The session compares complete expression results across paths; the public
/// sizeof_operand projects returned graphs across alternatives. Captured bounds
/// must stay in their owning path, even when the primary returned graph also
/// carries alternatives. No literal typing, callable evaluation or memory reads.
pub(super) fn sizeof_value(
    state: &CompileTimeState,
    site: &BindingSite,
    argument: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<BoundValue>, IncompleteReason> {
    budget.with_layout_query(|budget| {
        let before = budget.manual_retained();
        let result = match parse_sizeof_operand(argument, budget) {
            Fact::Known(operand) => {
                match state.layout_operand_single(site.clone(), &operand, budget) {
                    Fact::Known(layout) => Ok(i64::try_from(layout.storage.size)
                        .ok()
                        .map(|n| BoundValue::Constant(ConstantValue::Integer(n)))),
                    Fact::Unknown(_) => Ok(None),
                    Fact::Incomplete(reason) => Err(reason),
                }
            }
            Fact::Unknown(_) => Ok(None),
            Fact::Incomplete(reason) => Err(reason),
        };
        // Operand strings/selectors and returned layout identities are scratch;
        // only a scalar integer can survive this bridge.
        budget.release(budget.manual_retained().saturating_sub(before));
        result
    })
}
