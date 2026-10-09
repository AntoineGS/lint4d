//! Source-backed root session with independent reachable typed continuations.
//! Textual includes, providers and verified headers have separate owners.
use super::*;
use crate::conditional::{
    self, BoundValue, ConditionalAnalysis, ConditionalDirective, ConditionalEnvironment,
    ConstantValue, DirectiveKind, ExpressionQuery, SourceOrder, Truth,
};
use crate::{
    AdmittedCapabilityOutcome, AdmittedCapabilityReports, CancellationToken, IncludeResolveRequest,
    LoadedSource, Resolution, ResolutionObservation, ResolutionOutcome, ResolvedUnit,
    ResolverError, UnitResolveRequest,
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

    /// Promote the actual selected provider without reconstructing its names
    /// or context. Original field receipts are already installed in this owner.
    pub(crate) fn promote_owned(
        mut provider: AdmittedOwned<Self>,
        backing: AdmittedSourceBacking,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedProviderSource, ResolverError> {
        if !provider.is_admitted_by(budget)
            || !backing.is_admitted_by(budget)
            || backing.id != provider.unit.source.id
            || backing.revision != provider.unit.source.revision
        {
            return Err(ResolverError::InvalidRequest(
                "provider promotion requires same-root source and field owners".into(),
            ));
        }
        provider.value_mut().backing = Some(backing);
        provider
            .into_shared(budget)
            .and_then(|provider| provider.into_arc(budget))
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
    /// Returned reports are an explicit owned capability. Defaults never call
    /// legacy callbacks; bounded adapters opt in with admitted constructors.
    fn unit_report_capability(
        &mut self,
        _request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError> {
        let reason = "no admitted provider capability";
        let owner = budget
            .admit_owned(1, reason.len(), reason.len(), || reason.to_string())
            .map_err(|r| resolver_error(r, budget))?;
        AdmittedCapabilityOutcome::from_reason(
            owner,
            false,
            AdmittedCapabilityReports::copy(&[], &[], budget)?,
            budget,
        )
    }
    fn include_capability(
        &mut self,
        _request: IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
        let reason = "no admitted include capability";
        let owner = budget
            .admit_owned(1, reason.len(), reason.len(), || reason.to_string())
            .map_err(|r| resolver_error(r, budget))?;
        AdmittedCapabilityOutcome::from_reason(
            owner,
            false,
            AdmittedCapabilityReports::copy(&[], &[], budget)?,
            budget,
        )
    }
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
    // Root backing remains independently transferable even after publication.
    // Declared after root so payload dies before its credit on ordinary Drop.
    root_credit: Option<AdmittedOwned<()>>,
    root_limits: CompileTimeLimits,
}

pub type CompileTimeAnalysis = AdmittedOwned<CompileTimeOutput>;

impl AdmittedOwned<CompileTimeOutput> {
    // Resolver integration remains gated on the callback/cache ownership split.
    #[allow(dead_code)]
    pub(crate) fn into_root_projection(
        mut self,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedOwned<ConditionalAnalysis>, ResolverError> {
        let view = self.budget.clone();
        let mut budget = view.store_budget(self.root_limits, cancel);
        let credit = self
            .value_mut()
            .root_credit
            .take()
            .expect("independent root receipt");
        let root = credit
            .map_value(&mut budget, |()| {
                std::mem::replace(&mut self.value_mut().root, conditional::empty_output())
            })
            .map_err(|r| resolver_error(r, &budget))?;
        // All other output/facts/occurrence owners die normally, not by an
        // aggregate retained snapshot subtraction. Root backing never clones.
        drop(self);
        Ok(root)
    }
}

#[derive(Debug)]
struct SessionOutput {
    root: AdmittedOwned<ConditionalAnalysis>,
    occurrences: AdmittedOwned<Vec<OccurrenceAnalysis>>,
    includes: AdmittedOwned<Vec<IncludeOccurrence>>,
    facts: Option<AdmittedOwned<CompileTimeState>>,
    header: AdmittedOwned<Fact<VerifiedUnitHeader>>,
    reasons: AdmittedOwned<Vec<IncompleteReason>>,
    observations: AdmittedOwned<Vec<ResolutionObservation>>,
    warnings: AdmittedOwned<Vec<String>>,
    directive_fragments: AdmittedOwned<Vec<DirectiveFragments>>,
}

impl SessionOutput {
    fn facts(&self) -> &AdmittedOwned<CompileTimeState> {
        self.facts.as_ref().expect("unpublished facts owner")
    }
    fn facts_mut(&mut self) -> &mut AdmittedOwned<CompileTimeState> {
        self.facts.as_mut().expect("unpublished facts owner")
    }
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
    compilation: AdmittedOwned<SessionOutput>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<CompileTimeAnalysis, ResolverError> {
    // SessionOutput consists solely of independently owned fields. Admit its
    // temporary receiving carrier before consuming the compilation wrapper;
    // move those owners themselves, not the opaque compilation reservation.
    // Dead frames/text/coordinator invoices die before destination publication.
    let mut receiving = budget
        .admit_owned(0, 0, 0, || None)
        .map_err(|r| resolver_error(r, budget))?;
    let scratch = compilation
        .map_value(budget, |fields| {
            *receiving.value_mut() = Some(fields);
        })
        .map_err(|r| resolver_error(r, budget))?;
    drop(scratch);
    let analysis = receiving.value_mut().as_mut().expect("moved output owners");
    let facts = analysis
        .facts
        .take()
        .expect("unpublished facts owner")
        .into_shared(budget)
        .map_err(|r| resolver_error(r, budget))?;
    let view = budget
        .admit_owned(0, 0, 0, || ())
        .and_then(|v| v.into_shared(budget))
        .and_then(|v| v.into_arc(budget))
        .map_err(|r| resolver_error(r, budget))?;
    let root_limits = budget.limits();
    let mut output = budget
        .admit_owned(1, 0, 0, || CompileTimeOutput {
            root: conditional::empty_output(),
            occurrences: Vec::new(),
            includes: Vec::new(),
            facts,
            header: Fact::Unknown(UnknownReason::HeaderNotAnalyzed),
            reasons: Vec::new(),
            observations: Vec::new(),
            warnings: Vec::new(),
            directive_fragments: Vec::new(),
            budget: view,
            root_credit: None,
            root_limits,
        })
        .map_err(|r| resolver_error(r, budget))?;
    macro_rules! transfer {
        ($field:ident, $empty:expr) => {{
            let empty = budget
                .admit_owned(0, 0, 0, $empty)
                .map_err(|r| resolver_error(r, budget))?;
            let field = std::mem::replace(&mut analysis.$field, empty);
            output
                .move_field(field, budget, |output, field| output.$field = field)
                .map_err(|r| resolver_error(r, budget))?;
        }};
    }
    let empty_root = budget
        .admit_owned(0, 0, 0, conditional::empty_output)
        .map_err(|r| resolver_error(r, budget))?;
    let root = std::mem::replace(&mut analysis.root, empty_root);
    let credit = root
        .map_value(budget, |root| output.value_mut().root = root)
        .map_err(|r| resolver_error(r, budget))?;
    output.value_mut().root_credit = Some(credit);
    transfer!(occurrences, Vec::new);
    transfer!(includes, Vec::new);
    transfer!(header, || Fact::Unknown(UnknownReason::HeaderNotAnalyzed));
    transfer!(reasons, Vec::new);
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
    let observations = std::mem::replace(&mut analysis.observations, empty_observations);
    let warnings = std::mem::replace(&mut analysis.warnings, empty_warnings);
    let fragments = std::mem::replace(&mut analysis.directive_fragments, empty_fragments);
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

fn failed_projection(
    analysis: &mut AdmittedOwned<ConditionalAnalysis>,
    text: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    analysis
        .with_budget_mut(budget, |analysis, budget| {
            // Original projection owns a source-sized backing. Reset it in place;
            // retained range capacities remain live even when their lengths clear.
            budget.charge(1, text.len(), 0)?;
            analysis.complete = false;
            analysis.inactive_spans.clear();
            analysis.unknown_spans.clear();
            if !text.is_empty() {
                if analysis.unknown_spans.capacity() == 0 {
                    budget.charge(1, 0, size_of::<Range<usize>>())?;
                    analysis.unknown_spans.reserve_exact(1);
                }
                analysis.unknown_spans.push(0..text.len());
            }
            analysis.projected_source.clear();
            analysis.projected_source.push_str(text);
            Ok(())
        })
        .map_err(|r| resolver_error(r, budget))?
        .map_err(|r| resolver_error(r, budget))
}

fn resolution_output_payload(result: &Resolution<LoadedSource>) -> usize {
    match result {
        Resolution::Found(source) => crate::resolver::loaded_source_metadata_retained(source),
        Resolution::Unavailable { reason } => reason.len(),
        Resolution::Ambiguous { candidates } | Resolution::Incomplete { candidates, .. } => {
            let reason = if let Resolution::Incomplete { reason, .. } = result {
                reason.len()
            } else {
                0
            };
            reason
                + candidates
                    .iter()
                    .map(|c| {
                        size_of::<crate::ResolutionCandidate>()
                            + c.path.as_os_str().len()
                            + c.declared_name.as_ref().map_or(0, String::len)
                    })
                    .sum::<usize>()
        }
    }
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
    budget: &mut CompileTimeBudget<'_>,
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
    // Root/source metadata has already crossed the root-first admission. Text
    // feeds borrow its existing shared backing; only the active identity slot
    // is new scratch. Public projection backing is independently constructed.
    let reserve = size_of::<crate::SourceId>() + root.id.as_str().len();
    budget
        .charge(1, root.id.as_str().len(), reserve)
        .map_err(|r| resolver_error(r, &budget))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ResolverError::InvalidRequest("root coordinate text is not UTF-8".into()))?;
    let mut state = CompileTimeState::admitted_new(context, root, budget)?;
    state.value_mut().interface_only = interface_only;
    super::declarations::limit(1, limits.max_branch_states.min(64), "max_branch_states")
        .map_err(|r| resolver_error(r, &budget))?;
    // Graph payload and its final conditional environment belong to this path
    // from construction; frame/current Vec slots remain coordinator storage.
    let first = budget.admit_compilation(0, |budget| {
        let entries = context
            .defines
            .len()
            .checked_add(context.options.len())
            .and_then(|n| n.checked_add(context.constants.len()))
            .ok_or_else(|| ResolverError::InvalidRequest("context admission overflow".into()))?;
        // The path carrier already admits the current inline environment.
        // Reserve its eventual conditional Box here, before context allocation;
        // collapse can then move these maps without a new copy/invoice.
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
        budget
            .charge(1, 0, size_of::<Sequence>())
            .map_err(|r| resolver_error(r, &budget))?;
        let mut environment = ConditionalEnvironment::from_context(context);
        environment.typed_state = Some(Box::new(
            state
                .snapshot(budget)
                .map_err(|r| resolver_error(r, &budget))?,
        ));
        let scalar = environment
            .snapshot_scalar(budget)
            .map_err(|r| resolver_error(r, &budget))?;
        environment
            .typed_state
            .as_deref_mut()
            .expect("typed root")
            .scalar_history
            .push((Sequence(0), scalar));
        Ok(ReachablePath {
            environment,
            boxed_state_invoice: size_of::<CompileTimeState>(),
        })
    })?;
    budget
        .charge(1, 0, size_of::<AdmittedOwned<ReachablePath>>())
        .map_err(|r| resolver_error(r, budget))?;
    budget
        .charge(
            1,
            root.path.as_os_str().len(),
            size_of::<(OccurrenceId, std::path::PathBuf)>() + root.path.as_os_str().len(),
        )
        .map_err(|r| resolver_error(r, budget))?;
    let mut session = RootSession {
        root: root.clone(),
        resolver,
        cancel,
        context,
        occurrence: root_occurrence,
        active_sources: vec![root.id.clone()],
        occurrences: budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?,
        occurrence_slots: 0,
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
        reasons: budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?,
        reason_slots: 0,
        includes: budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, budget))?,
        include_slots: 0,
        budget,
        fatal: None,
        current: vec![first],
        frames: Vec::new(),
        last_condition: Truth::Unknown,
        providers,
        physical_paths: vec![(root_occurrence, root.path.clone())],
    };
    let result = conditional::analyze_with_source_order(text, context, cancel, &mut session);
    let mut analysis = result.map_err(|()| {
        session.resource_error.take().unwrap_or_else(|| {
            resolver_error(
                session
                    .fatal
                    .take()
                    .unwrap_or(IncompleteReason::MalformedSource),
                session.budget,
            )
        })
    })?;
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
            .charge(paths, 0, paths * size_of::<AdmittedOwned<ReachablePath>>())
            .map_err(|r| resolver_error(r, &session.budget))?;
        for mut frame in std::mem::take(&mut session.frames) {
            session.current.append(&mut frame.pending);
            session.current.append(&mut frame.joined);
        }
    }
    if session.reasons.is_empty() {
        let mut failure = None;
        for path in &mut session.current {
            if let Err(reason) = path.finish_owned(&mut session.budget) {
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
        let mut paths = std::mem::take(&mut session.current).into_iter();
        let mut state = paths
            .next()
            .expect("nonempty")
            .into_state_owner(session.budget)
            .map_err(|r| resolver_error(r, session.budget))?;
        let slots = (count - 1)
            .checked_mul(size_of::<CompileTimeState>())
            .ok_or_else(|| ResolverError::InvalidRequest("alternative slots overflow".into()))?;
        state
            .grow(session.budget, count, 0, slots, |state| {
                state.alternatives.reserve_exact(count - 1);
            })
            .map_err(|r| resolver_error(r, session.budget))?;
        for path in paths {
            let alternative = path
                .into_state_owner(session.budget)
                .map_err(|r| resolver_error(r, session.budget))?;
            state
                .move_field(alternative, session.budget, |state, alternative| {
                    state.alternatives.push(alternative)
                })
                .map_err(|r| resolver_error(r, session.budget))?;
        }
        session.state = state;
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
            .state
            .with_budget_mut(session.budget, |state, budget| {
                budget
                    .charge(
                        count,
                        dynamic.saturating_mul(count),
                        dynamic.saturating_mul(count),
                    )
                    .map_err(|r| resolver_error(r, budget))?;
                // Every retained graph remains incomplete; never publish the primary
                // healthy path merely because it happened to be the first alternative.
                state.failure = Some(reason.clone());
                for alternative in &mut state.alternatives {
                    alternative.failure = Some(reason.clone());
                }
                Ok(())
            })
            .map_err(|r| resolver_error(r, session.budget))??;
    }
    if let Some(reason) = session.fatal.take() {
        return Err(resolver_error(reason, &session.budget));
    }
    if !session.reasons.is_empty() {
        failed_projection(&mut analysis, text, session.budget)?;
    }
    // Complete output cloning is admitted before allocation. Source byte/text
    // Arcs are shared; source identity/path metadata and analysis are copied.
    let payload = conditional_output_payload(&analysis)? - size_of::<ConditionalAnalysis>()
        + crate::resolver::loaded_source_metadata_retained(root);
    let mut occurrence = session
        .budget
        .admit_owned(analysis.directives.len() + 1, payload, payload, || {
            OccurrenceAnalysis {
                id: root_occurrence,
                source: root.clone(),
                analysis: (*analysis).clone(),
            }
        })
        .map_err(|r| resolver_error(r, session.budget))?;
    // Projection only: finalized logical directives mask their actual physical
    // fragments, including child fragments whose EOF preceded the terminator.
    // No parser, evaluator or read decision is run again.
    for occurrence in
        std::iter::once(occurrence.value_mut()).chain(session.occurrences.value_mut().iter_mut())
    {
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
            session
                .occurrences
                .with_budget_mut(session.budget, |values, budget| {
                    conditional::output_slot(values, &mut session.occurrence_slots, budget)
                })
                .map_err(|r| resolver_error(r, session.budget))?
                .map_err(|r| resolver_error(r, session.budget))?;
            session
                .occurrences
                .move_field(occurrence, session.budget, |values, occurrence| {
                    values.insert(0, occurrence)
                })
                .map_err(|r| resolver_error(r, session.budget))?;
            session.occurrences
        },
        includes: session.includes,
        facts: Some(session.state),
        header: session
            .budget
            .admit_owned(0, 0, 0, || Fact::Unknown(UnknownReason::HeaderNotAnalyzed))
            .map_err(|r| resolver_error(r, session.budget))?,
        reasons: session.reasons,
        observations: session.observations,
        warnings: session.warnings,
        directive_fragments: session.directive_fragments,
    })
}

pub(crate) fn observation_payload(
    observation: &ResolutionObservation,
) -> Result<usize, ResolverError> {
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

fn move_report_vector<T>(
    target: &mut AdmittedOwned<Vec<T>>,
    incoming: AdmittedOwned<Vec<T>>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    if !target.is_admitted_by(budget) || !incoming.is_admitted_by(budget) {
        return Err(ResolverError::InvalidRequest(
            "foreign-root child report".into(),
        ));
    }
    budget
        .charge(incoming.len(), 0, 0)
        .map_err(|r| resolver_error(r, budget))?;
    if incoming.is_empty() {
        return Ok(());
    }
    if target.is_empty() && target.capacity() == 0 {
        return target
            .move_field(incoming, budget, |values, incoming| *values = incoming)
            .map_err(|r| resolver_error(r, budget));
    }
    let capacity_bytes = |count: usize| {
        count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| ResolverError::InvalidRequest("child report backing overflow".into()))
    };
    let source_backing = capacity_bytes(incoming.capacity())?;
    let needed = target
        .len()
        .checked_add(incoming.len())
        .ok_or_else(|| ResolverError::InvalidRequest("child report count overflow".into()))?;
    if needed > target.capacity() {
        let old_backing = capacity_bytes(target.capacity())?;
        let new_backing = capacity_bytes(needed)?;
        target
            .grow(budget, 0, 0, new_backing, |values| {
                let mut receiving = Vec::with_capacity(needed);
                receiving.append(values);
                let old = std::mem::replace(values, receiving);
                drop(old);
            })
            .map_err(|r| resolver_error(r, budget))?;
        target
            .retire_storage(old_backing)
            .map_err(|r| resolver_error(r, budget))?;
    }
    target
        .move_field(incoming, budget, |values, mut incoming| {
            values.append(&mut incoming)
        })
        .map_err(|r| resolver_error(r, budget))?;
    // This source field's named Vec backing died inside the consuming append;
    // payload credits stay in target. No payload-sized or ledger-delta refund.
    target
        .retire_storage(source_backing)
        .map_err(|r| resolver_error(r, budget))
}

#[cfg(test)]
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

#[cfg(test)]
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
    resolver: &'a mut dyn CompileTimeResolver,
    cancel: &'a dyn CancellationToken,
    context: &'a ConditionalContext,
    occurrence: OccurrenceId,
    active_sources: Vec<crate::SourceId>,
    occurrences: AdmittedOwned<Vec<OccurrenceAnalysis>>,
    occurrence_slots: usize,
    observations: AdmittedOwned<Vec<ResolutionObservation>>,
    warnings: AdmittedOwned<Vec<String>>,
    scanner: conditional::DirectiveScanner,
    resource_error: Option<ResolverError>,
    directive_count: usize,
    directive_fragments: AdmittedOwned<Vec<DirectiveFragments>>,
    directive_fragment_slots: usize,
    current_directive: usize,
    state: AdmittedOwned<CompileTimeState>,
    budget: &'a mut CompileTimeBudget<'b>,
    reasons: AdmittedOwned<Vec<IncompleteReason>>,
    reason_slots: usize,
    includes: AdmittedOwned<Vec<IncludeOccurrence>>,
    include_slots: usize,
    fatal: Option<IncompleteReason>,
    current: Vec<AdmittedOwned<ReachablePath>>,
    frames: Vec<ReachableFrame>,
    last_condition: Truth,
    providers: &'a mut ProviderSession,
    physical_paths: Vec<(OccurrenceId, std::path::PathBuf)>,
}

struct ReachablePath {
    environment: ConditionalEnvironment,
    // snapshot/new admitted the receiving Box<State>'s exact inline storage.
    // Collapse destroys that Box; only this named construction invoice retires.
    boxed_state_invoice: usize,
}

#[derive(Default)]
struct ReachableFrame {
    pending: Vec<AdmittedOwned<ReachablePath>>,
    joined: Vec<AdmittedOwned<ReachablePath>>,
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
                let operand = self
                    .budget
                    .admit_compilation(0, |budget| Ok(parse_sizeof_operand(argument, budget)))?;
                match &*operand {
                    Fact::Known(operand) => {
                        let leading = self.budget.admit_compilation(0, |budget| {
                            operand
                                .leading_name(budget)
                                .map_err(|r| resolver_error(r, budget))
                        })?;
                        self.prepare_name(state, site, &leading.0, leading.1, depth + 1)
                    }
                    Fact::Incomplete(r) => Err(resolver_error(r.clone(), self.budget)),
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
            let copy = self.budget.admit_compilation(0, |budget| {
                state
                    .snapshot(budget)
                    .map_err(|r| resolver_error(r, budget))
            });
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
                    self.resolver.unit_report_capability(
                        UnitResolveRequest {
                            requested_name: &pending.identity.name,
                            importer_path: &pending.importer_path,
                            legacy_route: None,
                        },
                        budget,
                        self.cancel,
                    )
                })?;
                let (result, reports) = outcome.into_parts();
                reports.consume_into(self.observations, self.warnings, self.budget)?;
                state.interfaces[index].attempted = true;
                let result = if result.owns_reason() {
                    // Failed provider reason is discarded, not detached into a
                    // plain field. Its actual storage dies before its credit.
                    drop(result);
                    state.interfaces[index].failure = Some(UnknownReason::UnresolvedName);
                    continue;
                } else {
                    result.into_unmigrated()?
                };
                match result {
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
        let environment = self.budget.admit_compilation(0, |budget| {
            let environment = if let Some((_, environment)) = state
                .scalar_history
                .iter()
                .rev()
                .find(|(seq, _)| seq.0 <= site.sequence.0)
            {
                environment.snapshot_scalar(budget)
            } else {
                ConditionalEnvironment::context_snapshot(&state.context, budget)
            }
            .map_err(|r| resolver_error(r, budget))?;
            Ok(environment)
        })?;
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
        drop(environment);
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
                .value_mut()
                .facts_mut()
                .with_budget_mut(self.budget, |facts, budget| {
                    let site = facts.site();
                    let mut engine = ProviderEngine {
                        resolver: self.resolver,
                        providers: self.providers,
                        cancel: self.cancel,
                        budget,
                        observations: self.observations,
                        warnings: self.warnings,
                    };
                    engine.prepare_name(facts, &site, name, pointers, depth + 1)
                })
                .map_err(|r| resolver_error(r, self.budget))?;
            result?;
            Ok(analysis)
        });
        self.providers.active.value_mut().pop();
        let mut analysis = result?;
        let empty_observations = self
            .budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, self.budget))?;
        let empty_warnings = self
            .budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| resolver_error(r, self.budget))?;
        let observations =
            std::mem::replace(&mut analysis.value_mut().observations, empty_observations);
        let warnings = std::mem::replace(&mut analysis.value_mut().warnings, empty_warnings);
        move_report_vector(self.observations, observations, self.budget)?;
        move_report_vector(self.warnings, warnings, self.budget)?;
        if !analysis.facts().interface_complete {
            return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
        }
        let output = analysis.value_mut();
        let occurrences = &output.occurrences;
        output
            .facts
            .as_mut()
            .expect("provider facts owner")
            .with_budget_mut(self.budget, |facts, budget| {
                let metadata = occurrences
                    .iter()
                    .map(|o| crate::resolver::loaded_source_metadata_retained(&o.source))
                    .sum::<usize>();
                budget
                    .charge(
                        1,
                        metadata,
                        metadata
                            + size_of::<AdmittedSourceBacking>()
                            + occurrences.len() * size_of::<LoadedSource>(),
                    )
                    .map_err(|r| resolver_error(r, budget))?;
                facts.provider_sources = occurrences.iter().map(|o| o.source.clone()).collect();
                facts.provider_backings.push(
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
            .value_mut()
            .facts
            .take()
            .expect("provider facts owner")
            .into_shared(self.budget)
            .and_then(|v| v.into_arc(self.budget))
            .map_err(|r| resolver_error(r, self.budget))?;
        drop(analysis);
        Ok(Fact::Known(graph))
    }
}

impl RootSession<'_, '_> {
    fn resolve_pending_imports(&mut self, activity: Truth) -> Result<(), ResolverError> {
        for index in 0..self.current.len() {
            let imports =
                std::mem::take(&mut self.current[index].value_mut().state_mut().pending_imports);
            for identity in imports {
                self.current[index]
                    .with_budget_mut(self.budget, |path_state, budget| {
                        budget
                            .charge(self.physical_paths.len(), 0, 0)
                            .map_err(|r| resolver_error(r, budget))?;
                        let path = &self
                            .physical_paths
                            .iter()
                            .find(|(id, _)| *id == identity.opener.occurrence)
                            .ok_or_else(|| {
                                ResolverError::InvalidRequest(
                                    "missing import opener occurrence".into(),
                                )
                            })?
                            .1;
                        budget
                            .charge(
                                1,
                                identity.name.len() + path.as_os_str().len(),
                                size_of::<super::bindings::InterfaceFacts>()
                                    + size_of::<String>()
                                    + identity.name.len()
                                    + path.as_os_str().len(),
                            )
                            .map_err(|r| resolver_error(r, budget))?;
                        let qualifiers = vec![identity.name.clone()];
                        let site = identity.site.clone();
                        let pending = super::bindings::PendingProvider {
                            identity,
                            importer_path: path.clone(),
                            authorized: activity == Truth::True,
                        };
                        path_state.state_mut().interfaces.reserve_exact(1);
                        path_state
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
                        Ok(())
                    })
                    .map_err(|r| resolver_error(r, self.budget))??;
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
        let (mut result, reports) = if directive.activity != Truth::True {
            (
                crate::resolver::AdmittedCapabilityResult::from(Resolution::Unavailable {
                    reason: "include conditional activity is unknown".into(),
                }),
                None,
            )
        } else if self.active_sources.len() > self.resolver.max_include_depth() {
            (
                crate::resolver::AdmittedCapabilityResult::from(Resolution::Incomplete {
                    reason: "include depth limit reached".into(),
                    candidates: vec![],
                }),
                None,
            )
        } else if let Some(name) = name.as_deref() {
            let outcome = self.budget.external(|budget| {
                self.resolver.include_capability(
                    IncludeResolveRequest {
                        including_path,
                        byte_range: anchor.range.clone(),
                        requested_name: name,
                        legacy_route: None,
                    },
                    budget,
                    self.cancel,
                )
            })?;
            let (result, reports) = outcome.into_parts();
            (result, Some(reports))
        } else {
            (
                crate::resolver::AdmittedCapabilityResult::from(Resolution::Unavailable {
                    reason: "include has no statically known name".into(),
                }),
                None,
            )
        };
        if let Some(reports) = reports {
            reports.consume_into(&mut self.observations, &mut self.warnings, self.budget)?;
        }
        if let Resolution::Found(child) = result.result() {
            if self.active_sources.contains(&child.id) {
                result = Resolution::Incomplete {
                    reason: "textual include cycle".into(),
                    candidates: vec![],
                }
                .into();
            }
        }
        let index = self.includes.len();
        self.budget
            .charge(mapping.fragments.len(), 0, 0)
            .map_err(|r| resolver_error(r, self.budget))?;
        let fragments_bytes = mapping.fragments.iter().try_fold(0usize, |sum, a| {
            if self.cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            sum.checked_add(std::mem::size_of::<SourceAnchor>())
                .and_then(|n| n.checked_add(a.source.as_str().len()))
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("include fragment sizing overflow".into())
                })
        })?;
        let owns_reason = result.owns_reason();
        let target_payload = if owns_reason {
            0
        } else {
            resolution_output_payload(result.result())
        };
        let mut include = self
            .budget
            .admit_owned(
                mapping.fragments.len() + 1,
                fragments_bytes + anchor.source.as_str().len() + target_payload,
                fragments_bytes + anchor.source.as_str().len() + target_payload,
                || IncludeOccurrence {
                    owner,
                    directive: anchor.clone(),
                    fragments: mapping.fragments.clone(),
                    target: if owns_reason {
                        Resolution::Unavailable {
                            reason: String::new(),
                        }
                    } else {
                        result.result().clone()
                    },
                    child: None,
                },
            )
            .map_err(|r| resolver_error(r, self.budget))?;
        if owns_reason {
            result.move_reason_into(&mut include, self.budget, |include, target| {
                include.target = target
            })?;
        } else {
            drop(result);
        }
        self.includes
            .with_budget_mut(self.budget, |values, budget| {
                conditional::output_slot(values, &mut self.include_slots, budget)
            })
            .map_err(|r| resolver_error(r, self.budget))?
            .map_err(|r| resolver_error(r, self.budget))?;
        self.includes
            .move_field(include, self.budget, |values, include| values.push(include))
            .map_err(|r| resolver_error(r, self.budget))?;
        match &self.includes[index].target {
            Resolution::Found(child) => {
                // The child output source metadata is copied once into its
                // own carrier, then moves with the child conditional output.
                let metadata = crate::resolver::loaded_source_metadata_retained(child);
                let mut output = self
                    .budget
                    .admit_owned(1, metadata, metadata, || OccurrenceAnalysis {
                        id: OccurrenceId(self.providers.next_occurrence),
                        source: child.clone(),
                        analysis: conditional::empty_output(),
                    })
                    .map_err(|r| resolver_error(r, self.budget))?;
                self.budget
                    .charge(
                        1,
                        metadata + child.id.as_str().len(),
                        metadata + child.id.as_str().len() + size_of::<crate::SourceId>(),
                    )
                    .map_err(|r| resolver_error(r, &self.budget))?;
                let bytes = output.source.analysis_bytes();
                let text = std::str::from_utf8(&bytes).map_err(|_| {
                    ResolverError::InvalidRequest("include coordinate text is not UTF-8".into())
                })?;
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
                self.includes.value_mut()[index].child = Some(occurrence);
                let parent = std::mem::replace(&mut self.root, child);
                let parent_occurrence = std::mem::replace(&mut self.occurrence, occurrence);
                self.active_sources.push(self.root.id.clone());
                let result =
                    conditional::analyze_with_source_order(&text, self.context, self.cancel, self);
                let mut analysis = result.map_err(|()| {
                    self.resource_error.take().unwrap_or_else(|| {
                        resolver_error(
                            self.fatal
                                .take()
                                .unwrap_or(IncompleteReason::MalformedSource),
                            self.budget,
                        )
                    })
                })?;
                if !analysis.complete {
                    self.record(IncompleteReason::MalformedSource);
                }
                if !self.reasons.is_empty() {
                    failed_projection(&mut analysis, &text, self.budget)?;
                }
                // Only physical coordinates change. Typed paths, scopes, lexer,
                // scalar history, settings and conditional frames stay alive.
                let source = std::mem::replace(&mut self.root, parent);
                drop(source);
                self.occurrence = parent_occurrence;
                self.active_sources.pop();
                output
                    .move_field(analysis, self.budget, |output, analysis| {
                        output.analysis = analysis
                    })
                    .map_err(|r| resolver_error(r, self.budget))?;
                self.occurrences
                    .with_budget_mut(self.budget, |values, budget| {
                        conditional::output_slot(values, &mut self.occurrence_slots, budget)
                    })
                    .map_err(|r| resolver_error(r, self.budget))?
                    .map_err(|r| resolver_error(r, self.budget))?;
                self.occurrences
                    .move_field(output, self.budget, |values, output| values.push(output))
                    .map_err(|r| resolver_error(r, self.budget))?;
            }
            failed => {
                let reason = match failed {
                    Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                        reason.as_str()
                    }
                    Resolution::Ambiguous { .. } => "ambiguous required include",
                    Resolution::Found(_) => unreachable!(),
                };
                let bytes = self
                    .root
                    .id
                    .as_str()
                    .len()
                    .checked_add(reason.len())
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("include failure provenance overflow".into())
                    })?;
                let reason = self
                    .budget
                    .admit_owned(1, bytes, bytes, || IncompleteReason::RequiredSource {
                        source: self.root.id.clone(),
                        reason: reason.to_string(),
                    })
                    .map_err(|r| resolver_error(r, self.budget))?;
                self.record_ref(&reason);
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
    ) -> Result<Vec<AdmittedOwned<ReachablePath>>, IncompleteReason> {
        let mut remaining = Vec::new();
        let total = self.reachable_count();
        let mut selected = Vec::new();
        let mut aggregate = None;
        self.budget.charge(
            self.current.len(),
            0,
            self.current.len()
                * (size_of::<Truth>() + 2 * size_of::<AdmittedOwned<ReachablePath>>()),
        )?;
        let mut decisions = Vec::new();
        let mut paths = std::mem::take(&mut self.current);
        for path in &mut paths {
            let condition = path.with_budget_mut(self.budget, |path, budget| {
                evaluate_path_condition(
                    &self.directive_fragments[self.current_directive].body_fragments,
                    path,
                    directive,
                    budget,
                    self.resolver,
                    self.providers,
                    self.cancel,
                    &mut self.observations,
                    &mut self.warnings,
                    &mut self.resource_error,
                )
            })??;
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
                Some(
                    match self.budget.admit_compilation(0, |budget| {
                        path.snapshot(budget).map_err(|r| resolver_error(r, budget))
                    }) {
                        Ok(path) => path,
                        Err(error) => {
                            self.resource_error = Some(error);
                            return Err(IncompleteReason::MalformedSource);
                        }
                    },
                )
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
        self.record_ref(&reason);
    }

    fn record_ref(&mut self, reason: &IncompleteReason) {
        if self.fatal.is_some() {
            return;
        }
        let dynamic = match reason {
            IncompleteReason::RequiredSource { source, reason } => {
                source.as_str().len() + reason.len()
            }
            _ => 0,
        };
        let result = self
            .reasons
            .with_budget_mut(self.budget, |reasons, budget| {
                budget.charge(reasons.len() + 1, 0, 0)?;
                if !reasons.contains(reason) {
                    conditional::output_slot(reasons, &mut self.reason_slots, budget)?;
                    budget.charge(1, dynamic, dynamic)?;
                    reasons.push(reason.clone());
                }
                Ok(())
            })
            .and_then(|r| r);
        if let Err(reason) = result {
            self.fatal = Some(reason);
            return;
        }
        if self.state.failure.is_none() {
            if let Err(reason) = self.state.grow(self.budget, 1, dynamic, dynamic, |state| {
                state.failure = Some(reason.clone());
            }) {
                self.fatal = Some(reason);
            }
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
                let invoice = saved.recovery_payload_bytes();
                drop(saved);
                budget.release(invoice);
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
        // snapshot_typed admits the Box<State> and scalar inline footprint before
        // cloning; the latter reserves this path's eventual conditional Box.
        let before = budget.manual_retained();
        match self.environment.snapshot_typed(budget) {
            Ok(environment) => Ok(Self {
                environment,
                boxed_state_invoice: size_of::<CompileTimeState>(),
            }),
            Err(reason) => {
                budget.release(budget.manual_retained().saturating_sub(before));
                Err(reason)
            }
        }
    }
}

impl AdmittedOwned<ReachablePath> {
    fn feed_owned(
        &mut self,
        fragment: SourceFragment<'_>,
        activity: Truth,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        self.with_budget_mut(budget, |path, budget| {
            path.state_mut().feed(fragment, activity, budget)
        })?
    }
    fn finish_owned(&mut self, budget: &mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason> {
        self.with_budget_mut(budget, |path, budget| path.state_mut().finish_root(budget))?
    }
    fn into_state_owner(
        self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedOwned<CompileTimeState>, IncompleteReason> {
        let boxed_state_invoice = self.boxed_state_invoice;
        // Scalar construction/snapshot already reserved its future conditional
        // Box's inline storage. The path carrier owns the current inline scalar
        // environment, so no second context copy or payload traversal occurs.
        let mut state = self.map_value(budget, ReachablePath::into_state)?;
        // into_state destroyed the original, separately admitted Box<State>.
        state.retire_storage(boxed_state_invoice)?;
        Ok(state)
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
    fn output_budget(
        &mut self,
        action: &mut dyn FnMut(&mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason>,
    ) -> Result<(), ()> {
        action(self.budget).map_err(|reason| {
            self.fatal = Some(reason);
        })
    }
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
                    if let Err(reason) = path.feed_owned(
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
        let directive = self
            .budget
            .admit_owned(1, physical.body.len(), physical.body.len(), || {
                physical.clone()
            })
            .map_err(|r| {
                self.fatal = Some(r);
            })?;
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
        // Classification work is cumulative. Actual scanner/output bodies,
        // directive/mask/activity slots and conditional frames are now admitted
        // at their independent constructors; no conservative output shadow
        // reservation belongs to dead compilation scratch.
        match self.budget.charge(1, body_bytes, 0) {
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
        let bytes = self.root.analysis_bytes();
        let text = std::str::from_utf8(&bytes).expect("validated physical source");
        for path in &mut self.current {
            if let Err(reason) = path.feed_owned(
                SourceFragment {
                    source: &self.root.id,
                    occurrence: self.occurrence,
                    physical_start: range.start,
                    text: &text[range.clone()],
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
                let result = path.with_budget_mut(self.budget, |path, budget| {
                    apply_path_directive(path, directive, budget)
                });
                if let Err(reason) = result.and_then(|r| r) {
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
        if path.state().option_stack.len() == path.state().option_stack.capacity() {
            let slots = (path.state().option_stack.len() + 1)
                .checked_mul(size_of::<Option<ConditionalEnvironment>>())
                .ok_or(IncompleteReason::MalformedSource)?;
            let old_slots = path.state().option_stack_slots;
            budget.charge(1, old_slots, slots)?;
            path.state_mut().option_stack.reserve_exact(1);
            path.state_mut().option_stack_slots = slots;
            budget.release(old_slots);
        }
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
            path.state_mut().option_stack.pop().flatten()
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
        let saved_missing = saved.is_none();
        let saved_invoice = saved
            .as_ref()
            .map(ConditionalEnvironment::recovery_payload_bytes);
        drop(saved);
        if let Some(invoice) = saved_invoice {
            budget.release(invoice);
        }
        restored?;
        if !supported || saved_missing {
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

    #[test]
    fn found_unit_name_owner_provider_engine_preserves_original_names_and_captured_alias() {
        struct Recording {
            inner: crate::UnitResolver<crate::FilesystemSourceStore>,
            requested: Option<usize>,
            declared: Option<usize>,
            last: Option<AdmittedProviderSource>,
        }
        impl CompileTimeResolver for Recording {
            fn unit_report_capability(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError>
            {
                let out = self.inner.unit_report_capability(r, b, c)?;
                if let Resolution::Found(provider) = out.result() {
                    self.requested = Some(provider.unit.requested_name.as_ptr() as usize);
                    self.declared = Some(provider.unit.declared_name.as_ptr() as usize);
                    self.last = Some(provider.clone());
                }
                Ok(out)
            }
            fn include_capability(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
                self.inner.include_capability(r, b, c)
            }
            fn unit(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                self.inner.unit(r, b, c)
            }
            fn include(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                self.inner.include(r, b, c)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Main.pas");
        std::fs::write(&path,"unit Main; interface uses Alias; {$IF SizeOf(Alias.V)=8}{$DEFINE HIT}{$ENDIF} implementation end.").unwrap();
        std::fs::write(
            dir.path().join("Space.Other.pas"),
            "unit Space . Other; interface type T=Int64; var V:T; implementation end.",
        )
        .unwrap();
        let mut project = pascal_project::ProjectContext::discover(
            &path,
            &[dir.path().into()],
            &Default::default(),
        )
        .unwrap();
        project
            .unit_aliases
            .insert("Alias".into(), "Space.Other".into());
        project.conditional_context = ConditionalContext::default()
            .with_compiler_version(pascal_project::CompilerVersion::new(37, 0))
            .with_layout(pascal_project::LayoutContext {
                platform: Some(pascal_project::LayoutPlatform::Win32),
                defaults: Default::default(),
            });
        project.library_conditional_context = project.conditional_context.clone();
        let context = project.conditional_context.clone();
        let mut inner = crate::UnitResolver::new(
            project,
            vec![dir.path().into()],
            crate::FilesystemSourceStore::new(),
            Default::default(),
        );
        let root = inner
            .load_source(&path, None, crate::SourceKind::Unit, &crate::NoCancellation)
            .unwrap();
        let mut recording = Recording {
            inner,
            requested: None,
            declared: None,
            last: None,
        };
        let mut budget =
            CompileTimeBudget::new(CompileTimeLimits::default(), &crate::NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let output = analyze_source_in_session(
            &root,
            &context,
            &mut recording,
            &mut budget,
            &crate::NoCancellation,
        )
        .unwrap();
        assert!(
            output
                .root
                .directives
                .iter()
                .any(|d| d.body == "DEFINE HIT" && d.activity == Truth::True),
            "actual imported variable/type layout remains positive"
        );
        let provider = recording.last.as_ref().unwrap();
        assert_eq!(provider.unit.requested_name, "Alias");
        assert_eq!(provider.unit.declared_name, "Space.Other");
        assert_eq!(
            Some(provider.unit.requested_name.as_ptr() as usize),
            recording.requested
        );
        assert_eq!(
            Some(provider.unit.declared_name.as_ptr() as usize),
            recording.declared
        );
        assert_eq!(output.facts.interfaces[0].pending.identity.name, "Alias");
        let provider = recording.last.take().unwrap();
        drop(recording);
        drop(root);
        drop(budget);
        let live = observer.budget_snapshot();
        assert!(live.retained_bytes > baseline);
        assert_eq!(provider.unit.declared_name, "Space.Other");
        drop(output);
        assert!(
            observer.budget_snapshot().retained_bytes > baseline,
            "original returned provider names/source outlive compiled output when a real owner remains"
        );
        assert_eq!(provider.unit.requested_name, "Alias");
        drop(provider);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, live.work);
        assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
    }
    use crate::{NoCancellation, SourceId, SourceRevision};

    struct NoGraphCallbacks;
    impl CompileTimeResolver for NoGraphCallbacks {
        fn include(
            &mut self,
            _: IncludeResolveRequest<'_>,
            _: &mut CompileTimeBudget<'_>,
            _: &dyn CancellationToken,
        ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
            panic!("graph fixture has no include");
        }
        fn unit(
            &mut self,
            _: UnitResolveRequest<'_>,
            _: &mut CompileTimeBudget<'_>,
            _: &dyn CancellationToken,
        ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
            panic!("graph fixture has no unit");
        }
    }
    fn graph_source(text: &str) -> LoadedSource {
        LoadedSource {
            id: SourceId::new("graph.pas"),
            path: "graph.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            compile_time_backing: None,
        }
    }

    #[test]
    fn actual_root_projection_moves_credit_and_backing_without_other_output_storage() {
        let root =
            graph_source("unit T; interface {$H+} type R=record X:Byte; end; implementation end.");
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let output = analyze_source_in_session(
            &root,
            &ConditionalContext::default(),
            &mut NoGraphCallbacks,
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let text = output.root.projected_source.as_ptr();
        let directives = output.root.directives.as_ptr();
        let body = output.root.directives[0].body.as_ptr();
        let sibling = output.facts.try_clone(&mut budget).unwrap();
        let before = observer.budget_snapshot();
        let projection = output.into_root_projection(&NoCancellation).unwrap();
        assert_eq!(projection.projected_source.as_ptr(), text);
        assert_eq!(projection.directives.as_ptr(), directives);
        assert_eq!(projection.directives[0].body.as_ptr(), body);
        let after = observer.budget_snapshot();
        assert_eq!(after.byte_work, before.byte_work);
        assert!(
            after.retained_bytes < before.retained_bytes,
            "dead occurrences/output controls drop, live sibling facts remain"
        );
        drop(budget);
        assert!(projection.complete);
        assert_eq!(projection.directives[0].body, "H+");
        drop(projection);
        assert!(observer.budget_snapshot().retained_bytes > baseline);
        assert!(sibling.declarations.iter().any(|d| d.name == "r"));
        drop(sibling);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().byte_work, before.byte_work);
    }

    #[test]
    fn actual_root_projection_cancel_drops_only_consumed_output_not_live_sibling() {
        struct Cancel;
        impl CancellationToken for Cancel {
            fn is_cancelled(&self) -> bool {
                true
            }
        }
        let root = graph_source("unit T; interface {$H+} implementation end.");
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let sibling = analyze_source_in_session(
            &root,
            &ConditionalContext::default(),
            &mut NoGraphCallbacks,
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let before = observer.budget_snapshot();
        let output = analyze_source_in_session(
            &root,
            &ConditionalContext::default(),
            &mut NoGraphCallbacks,
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        assert!(matches!(
            output.into_root_projection(&Cancel),
            Err(ResolverError::Cancelled)
        ));
        let after = observer.budget_snapshot();
        assert_eq!(after.retained_bytes, before.retained_bytes);
        assert!(after.byte_work > before.byte_work);
        assert_eq!(sibling.root.directives[0].body, "H+");
    }

    #[test]
    fn actual_output_publication_cancellation_covers_every_field_handoff() {
        struct CancelAt(std::cell::Cell<usize>, usize);
        impl CancellationToken for CancelAt {
            fn is_cancelled(&self) -> bool {
                let n = self.0.get();
                self.0.set(n + 1);
                n >= self.1
            }
        }
        let root =
            graph_source("unit T; interface {$H+} type R=record X:Byte; end; implementation end.");
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let mut providers = ProviderSession::new(&mut budget).unwrap();
        let mut cancelled_count = 0;
        let mut completed_calls = None;
        for allowed in 0..64 {
            let before = observer.budget_snapshot();
            let analysis = analyze_source_with_providers(
                &root,
                &ConditionalContext::default(),
                &mut NoGraphCallbacks,
                &mut budget,
                &NoCancellation,
                &mut providers,
                false,
                OccurrenceId(0),
            )
            .unwrap();
            let cancel = CancelAt(std::cell::Cell::new(0), allowed);
            let mut cancelled = observer.store_budget(CompileTimeLimits::default(), &cancel);
            let result = publish_analysis(analysis, &mut cancelled);
            let success = result.is_ok();
            match result {
                Ok(output) => drop(output),
                Err(ResolverError::Cancelled) => cancelled_count += 1,
                Err(error) => panic!("unexpected handoff error: {error:?}"),
            }
            let after = observer.budget_snapshot();
            assert_eq!(
                after.retained_bytes, before.retained_bytes,
                "actual final output field cancellation {allowed}"
            );
            assert!(after.work >= before.work);
            assert!(after.byte_work >= before.byte_work);
            if success {
                completed_calls = Some(cancel.0.get());
                break;
            }
        }
        assert_eq!(
            Some(cancelled_count),
            completed_calls,
            "every actual admission poll is exercised through the first successful publication"
        );
        assert!(cancelled_count > 0);
        drop(providers);
        drop(budget);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
    }

    #[test]
    fn actual_child_output_and_failed_include_buffers_move_with_source_credits() {
        struct Includes {
            child: LoadedSource,
        }
        impl CompileTimeResolver for Includes {
            fn include_capability(
                &mut self,
                request: IncludeResolveRequest<'_>,
                budget: &mut CompileTimeBudget<'_>,
                cancel: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
                // Explicit bounded source-only fixture, with no legacy report
                // allocation to wrap after the fact.
                let result = self.include(request, budget, cancel)?.result;
                Ok(AdmittedCapabilityOutcome::new(
                    result,
                    AdmittedCapabilityReports::copy(&[], &[], budget)?,
                ))
            }
            fn include(
                &mut self,
                request: IncludeResolveRequest<'_>,
                budget: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                let result = if request.requested_name == "child.inc" {
                    let backing = ProviderSource::admit_cached_backing(&self.child, budget)?;
                    let mut child = self.child.clone();
                    child.compile_time_backing = Some(backing);
                    Resolution::Found(
                        child
                            .admitted_copy(budget)
                            .expect("admitted fixture source envelope"),
                    )
                } else {
                    Resolution::Unavailable {
                        reason: "missing child fixture".into(),
                    }
                };
                Ok(ResolutionOutcome {
                    result,
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
                panic!("no provider");
            }
        }
        let root =
            graph_source("unit T; interface {$I child.inc}{$I absent.inc} implementation end.");
        let mut child = graph_source("type R=record X:Byte; end; {$H+}");
        child.id = SourceId::new("child.inc");
        child.path = "child.inc".into();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let mut providers = ProviderSession::new(&mut budget).unwrap();
        let analysis = analyze_source_with_providers(
            &root,
            &ConditionalContext::default(),
            &mut Includes { child },
            &mut budget,
            &NoCancellation,
            &mut providers,
            false,
            OccurrenceId(0),
        )
        .unwrap();
        assert_eq!(analysis.occurrences.len(), 2);
        assert_eq!(analysis.includes.len(), 2);
        let occurrences_ptr = analysis.occurrences.as_ptr();
        let child_text_ptr = analysis.occurrences[1].analysis.projected_source.as_ptr();
        let child_body_ptr = analysis.occurrences[1].analysis.directives[0].body.as_ptr();
        let includes_ptr = analysis.includes.as_ptr();
        let anchors_ptr = analysis.includes[0].fragments.as_ptr();
        let reasons_ptr = analysis.reasons.as_ptr();
        let reason_text_ptr = match &analysis.reasons[0] {
            IncompleteReason::RequiredSource { reason, .. } => reason.as_ptr(),
            _ => panic!("required source reason"),
        };
        let missing_ptr = match &analysis.includes[1].target {
            Resolution::Unavailable { reason } => reason.as_ptr(),
            _ => panic!("unavailable target"),
        };
        let source_ptr = analysis.occurrences[1].source.bytes.as_ptr();
        let source_id_ptr = analysis.occurrences[1].source.id.as_str().as_ptr();
        let source_path_ptr = analysis.occurrences[1]
            .source
            .path
            .as_os_str()
            .as_encoded_bytes()
            .as_ptr();
        let unknown_ptr = analysis.root.unknown_spans.as_ptr();
        let before = budget.snapshot();
        let output = publish_analysis(analysis, &mut budget).unwrap();
        assert_eq!(
            budget.snapshot().byte_work,
            before.byte_work,
            "no final child/root/include/reason copy invoice"
        );
        assert_eq!(output.occurrences.as_ptr(), occurrences_ptr);
        assert_eq!(
            output.occurrences[1].analysis.projected_source.as_ptr(),
            child_text_ptr
        );
        assert_eq!(
            output.occurrences[1].analysis.directives[0].body.as_ptr(),
            child_body_ptr
        );
        assert_eq!(output.includes.as_ptr(), includes_ptr);
        assert_eq!(output.includes[0].fragments.as_ptr(), anchors_ptr);
        assert_eq!(output.reasons.as_ptr(), reasons_ptr);
        match &output.reasons[0] {
            IncompleteReason::RequiredSource { reason, .. } => {
                assert_eq!(reason.as_ptr(), reason_text_ptr)
            }
            _ => unreachable!(),
        }
        match &output.includes[1].target {
            Resolution::Unavailable { reason } => assert_eq!(reason.as_ptr(), missing_ptr),
            _ => unreachable!(),
        }
        assert_eq!(output.occurrences[1].source.bytes.as_ptr(), source_ptr);
        assert_eq!(
            output.occurrences[1].source.id.as_str().as_ptr(),
            source_id_ptr
        );
        assert_eq!(
            output.occurrences[1]
                .source
                .path
                .as_os_str()
                .as_encoded_bytes()
                .as_ptr(),
            source_path_ptr
        );
        assert_eq!(output.root.unknown_spans.as_ptr(), unknown_ptr);
        let shared = output.into_shared(&mut budget).unwrap();
        let last = shared.try_clone(&mut budget).unwrap();
        drop(providers);
        drop(budget);
        drop(shared);
        assert_eq!(last.includes[0].child, Some(last.occurrences[1].id));
        assert_eq!(last.occurrences[1].source.id, SourceId::new("child.inc"));
        let physical = last
            .directive_fragments
            .iter()
            .find(|d| d.directive.body == "H+")
            .unwrap();
        assert_eq!(physical.completion, last.occurrences[1].id);
        assert_eq!(
            physical.body_fragments[0].source,
            SourceId::new("child.inc")
        );
        assert_eq!(
            physical.body_fragments[0].occurrence,
            last.occurrences[1].id
        );
        assert!(!last.root.complete);
        let live = observer.budget_snapshot();
        assert!(live.retained_bytes > baseline);
        drop(last);
        let dead = observer.budget_snapshot();
        assert_eq!(dead.retained_bytes, baseline);
        assert_eq!(dead.work, live.work);
        assert_eq!(dead.byte_work, live.byte_work);
    }

    #[test]
    fn actual_branch_facts_collapse_and_promotion_move_every_captured_backing() {
        let root = graph_source(
            "unit T; interface {$IF MISSING}{$DEFINE A}{$ELSE}{$UNDEF A}{$ENDIF}{$DEFINE COMMON} type R=record X:Byte; end; implementation end.",
        );
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let mut providers = ProviderSession::new(&mut budget).unwrap();
        let analysis = analyze_source_with_providers(
            &root,
            &ConditionalContext::default(),
            &mut NoGraphCallbacks,
            &mut budget,
            &NoCancellation,
            &mut providers,
            false,
            OccurrenceId(0),
        )
        .unwrap();
        assert_eq!(analysis.facts().reachable_states().count(), 2);
        let alternatives = analysis.facts().alternatives.as_ptr();
        let captures = analysis
            .facts()
            .reachable_states()
            .map(|state| {
                (
                    state.scopes.as_ptr(),
                    state.declarations.as_ptr(),
                    state.scalar_history.as_ptr(),
                    state.conditional_environment().unwrap() as *const ConditionalEnvironment,
                )
            })
            .collect::<Vec<_>>();
        let output = publish_analysis(analysis, &mut budget).unwrap();
        assert_eq!(output.facts.alternatives.as_ptr(), alternatives);
        for (state, (scopes, declarations, history, environment)) in
            output.facts.reachable_states().zip(captures)
        {
            assert_eq!(state.scopes.as_ptr(), scopes);
            assert_eq!(state.declarations.as_ptr(), declarations);
            assert_eq!(state.scalar_history.as_ptr(), history);
            assert_eq!(
                state.conditional_environment().unwrap() as *const _,
                environment
            );
        }
        let snapshot = output.facts.try_clone(&mut budget).unwrap();
        drop(providers);
        drop(budget);
        drop(output);
        let contexts = snapshot
            .reachable_states()
            .map(|state| {
                assert!(
                    state
                        .declarations
                        .iter()
                        .any(|d| d.name.eq_ignore_ascii_case("R"))
                );
                assert!(state.scalar_history.len() >= 3);
                state.conditional_environment().unwrap().to_context()
            })
            .collect::<Vec<_>>();
        assert!(
            contexts
                .iter()
                .any(|c| c.defines.get("a").or_else(|| c.defines.get("A")) == Some(&Truth::True))
        );
        assert!(
            contexts
                .iter()
                .any(|c| c.defines.get("a").or_else(|| c.defines.get("A")) == Some(&Truth::False))
        );
        let live = observer.budget_snapshot();
        assert!(live.retained_bytes > baseline);
        drop(snapshot);
        let dead = observer.budget_snapshot();
        assert_eq!(dead.retained_bytes, baseline);
        assert_eq!(dead.work, live.work);
        assert_eq!(dead.byte_work, live.byte_work);
    }

    #[test]
    fn actual_facts_owner_does_not_retain_root_output_or_dead_compilation_scratch() {
        let measure = |padding| {
            let root = graph_source(&format!(
                "unit T; interface {{{}}} type R=record X:Byte; end; implementation end.",
                "x".repeat(padding)
            ));
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = observer.budget_snapshot().retained_bytes;
            let output = analyze_source_in_session(
                &root,
                &ConditionalContext::default(),
                &mut NoGraphCallbacks,
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
            let graph = output.facts.try_clone(&mut budget).unwrap();
            drop(output);
            let graph_only = observer.budget_snapshot().retained_bytes - baseline;
            drop(graph);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            graph_only
        };
        assert_eq!(
            measure(0),
            measure(4096),
            "the same actual graph must not inherit root text/projection/parser/coordinator invoices"
        );
    }

    #[test]
    fn actual_partial_branch_cancellation_drops_new_graph_owners_not_live_sibling() {
        struct CancelAt(std::cell::Cell<usize>, usize);
        impl CancellationToken for CancelAt {
            fn is_cancelled(&self) -> bool {
                let n = self.0.get();
                self.0.set(n + 1);
                n >= self.1
            }
        }
        let root = graph_source(
            "unit T; interface {$IF MISSING}{$DEFINE A}{$ELSE}{$UNDEF A}{$ENDIF} type R=record X:Byte; end; {$IF OTHER}{$DEFINE B}{$ELSE}{$UNDEF B}{$ENDIF} implementation end.",
        );
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let sibling = analyze_source_in_session(
            &root,
            &ConditionalContext::default(),
            &mut NoGraphCallbacks,
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let sibling_ptr = sibling.facts.declarations.as_ptr();
        for allowed in [0, 1, 20, 100, 300, 600, 900] {
            let before = observer.budget_snapshot();
            let cancel = CancelAt(std::cell::Cell::new(0), allowed);
            let mut cancelled = observer.store_budget(CompileTimeLimits::default(), &cancel);
            assert!(
                matches!(
                    analyze_source_in_session(
                        &root,
                        &ConditionalContext::default(),
                        &mut NoGraphCallbacks,
                        &mut cancelled,
                        &cancel
                    ),
                    Err(ResolverError::Cancelled)
                ),
                "boundary {allowed}"
            );
            let after = observer.budget_snapshot();
            assert_eq!(
                after.retained_bytes, before.retained_bytes,
                "partial graph owner cleanup at boundary {allowed}"
            );
            assert!(after.work >= before.work);
            assert!(after.byte_work >= before.byte_work);
            assert_eq!(sibling.facts.declarations.as_ptr(), sibling_ptr);
        }
        drop(sibling);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
    }

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
        let scopes_ptr = analysis.facts().scopes.as_ptr();
        let declarations_ptr = analysis.facts().declarations.as_ptr();
        let scalar_history_ptr = analysis.facts().scalar_history.as_ptr();
        let root_text_ptr = analysis.root.projected_source.as_ptr();
        let root_directives_ptr = analysis.root.directives.as_ptr();
        let root_body_ptr = analysis.root.directives[0].body.as_ptr();
        let occurrences_ptr = analysis.occurrences.as_ptr();
        let occurrence_text_ptr = analysis.occurrences[0].analysis.projected_source.as_ptr();
        let occurrence_body_ptr = analysis.occurrences[0].analysis.directives[0].body.as_ptr();
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
        let before_publication = budget.snapshot();
        let output = publish_analysis(analysis, &mut budget).unwrap();
        assert_eq!(
            budget.snapshot().byte_work - before_publication.byte_work,
            0,
            "publication moves all actual public output buffers without a payload copy/traversal invoice"
        );
        assert_eq!(output.root.projected_source.as_ptr(), root_text_ptr);
        assert_eq!(output.root.directives.as_ptr(), root_directives_ptr);
        assert_eq!(output.root.directives[0].body.as_ptr(), root_body_ptr);
        assert_eq!(output.occurrences.as_ptr(), occurrences_ptr);
        assert_eq!(
            output.occurrences[0].analysis.projected_source.as_ptr(),
            occurrence_text_ptr
        );
        assert_eq!(
            output.occurrences[0].analysis.directives[0].body.as_ptr(),
            occurrence_body_ptr
        );
        assert_eq!(output.facts.scopes.as_ptr(), scopes_ptr);
        assert_eq!(output.facts.declarations.as_ptr(), declarations_ptr);
        assert_eq!(output.facts.scalar_history.as_ptr(), scalar_history_ptr);
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
        let mut path = ReachablePath {
            environment,
            boxed_state_invoice: size_of::<CompileTimeState>(),
        };
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
