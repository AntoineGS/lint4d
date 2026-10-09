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
use std::{mem::size_of, ops::Range};

#[derive(Debug, Clone)]
pub struct ProviderSource {
    pub unit: ResolvedUnit,
    pub context: ConditionalContext,
    pub authorized_qualifiers: Vec<String>,
}

pub trait CompileTimeResolver {
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUnitHeader {
    pub name: String,
    pub declaration: SourceAnchor,
    pub name_parts: Vec<SourceAnchor>,
}

#[derive(Debug)]
pub struct CompileTimeAnalysis {
    pub root: ConditionalAnalysis,
    pub occurrences: Vec<OccurrenceAnalysis>,
    pub includes: Vec<IncludeOccurrence>,
    pub facts: CompileTimeState,
    pub header: Fact<VerifiedUnitHeader>,
    pub reasons: Vec<IncompleteReason>,
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
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
    let bytes = root.analysis_bytes();
    let mut budget = CompileTimeBudget::new(limits, cancel);
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
    let state = CompileTimeState::new(context, root, &mut budget)
        .map_err(|r| resolver_error(r, &budget))?;
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
    let mut session = RootSession {
        root: root.clone(),
        text: text.to_owned(),
        resolver,
        cancel,
        context,
        occurrence: OccurrenceId(0),
        next_occurrence: 1,
        active_sources: vec![root.id.clone()],
        occurrences: Vec::new(),
        observations: Vec::new(),
        warnings: Vec::new(),
        scanner: conditional::DirectiveScanner::default(),
        resource_error: None,
        directive_count: 0,
        state,
        budget,
        reasons: Vec::new(),
        includes: Vec::new(),
        fatal: None,
        current: vec![first],
        frames: Vec::new(),
        last_condition: Truth::Unknown,
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
    let occurrence = OccurrenceAnalysis {
        id: OccurrenceId(0),
        source: root.clone(),
        analysis: analysis.clone(),
    };
    if cancel.is_cancelled() {
        return Err(ResolverError::Cancelled);
    }
    Ok(CompileTimeAnalysis {
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
    })
}

fn admit_observation(
    observation: &ResolutionObservation,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    let mut failure = None;
    let mut visit = |n| {
        budget.charge(1, n, n).map_err(|r| {
            failure = Some(r);
            "observation admission".into()
        })
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
            source_id, path, ..
        } => visit(source_id.as_str().len()).and_then(|_| visit(path.as_os_str().len())),
        ResolutionObservation::Metadata(observation) => {
            observation.visit_recovery_payload(&mut visit)
        }
        ResolutionObservation::ProjectRead(observation) => {
            observation.visit_recovery_payload(&mut visit)
        }
    };
    result.map_err(|_| resolver_error(failure.unwrap_or(IncompleteReason::MalformedSource), budget))
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

struct RootSession<'a> {
    root: LoadedSource,
    text: String,
    resolver: &'a mut dyn CompileTimeResolver,
    cancel: &'a dyn CancellationToken,
    context: &'a ConditionalContext,
    occurrence: OccurrenceId,
    next_occurrence: u64,
    active_sources: Vec<crate::SourceId>,
    occurrences: Vec<OccurrenceAnalysis>,
    observations: Vec<ResolutionObservation>,
    warnings: Vec<String>,
    scanner: conditional::DirectiveScanner,
    resource_error: Option<ResolverError>,
    directive_count: usize,
    state: CompileTimeState,
    budget: CompileTimeBudget<'a>,
    reasons: Vec<IncompleteReason>,
    includes: Vec<IncludeOccurrence>,
    fatal: Option<IncompleteReason>,
    current: Vec<ReachablePath>,
    frames: Vec<ReachableFrame>,
    last_condition: Truth,
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

impl RootSession<'_> {
    fn walk_include(&mut self, directive: &ConditionalDirective) -> Result<(), ResolverError> {
        // A reachable hypothetical path is not an authorization to read bytes.
        let name = crate::resolver::include_name(&directive.body);
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
            self.resolver.include(
                IncludeResolveRequest {
                    including_path: &self.root.path,
                    byte_range: directive.start..directive.end,
                    requested_name: name,
                    legacy_route: None,
                },
                &mut self.budget,
                self.cancel,
            )?
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
        self.observations.extend(observations);
        self.warnings.extend(warnings);
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
        self.includes.push(IncludeOccurrence {
            owner: self.occurrence,
            directive: SourceAnchor {
                source: self.root.id.clone(),
                occurrence: self.occurrence,
                range: directive.start..directive.end,
            },
            target: result,
            child: None,
        });
        match &self.includes[index].target {
            Resolution::Found(child) => {
                let bytes = child.analysis_bytes();
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
                let occurrence = OccurrenceId(self.next_occurrence);
                self.next_occurrence = self
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
        for path in &self.current {
            let condition = evaluate_path_condition(
                &self.root,
                self.occurrence,
                &self.text,
                &path,
                directive,
                &mut self.budget,
            )?;
            aggregate = Some(aggregate.map_or(condition, |prior: Truth| prior.merge(condition)));
            decisions.push(condition);
        }
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
        let before = budget.snapshot().retained_bytes;
        budget.charge(1, 0, size_of::<Self>())?;
        match self.environment.snapshot_typed(budget) {
            Ok(environment) => Ok(Self { environment }),
            Err(reason) => {
                budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
                Err(reason)
            }
        }
    }
}

fn evaluate_path_condition(
    root: &LoadedSource,
    occurrence: OccurrenceId,
    text: &str,
    path: &ReachablePath,
    directive: &ConditionalDirective,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Truth, IncompleteReason> {
    let state = path.state();
    let site = state.site();
    let environment = &path.environment;
    conditional::evaluate_source_condition(
        &directive.body,
        environment,
        budget,
        &mut |query, budget| {
            if let ExpressionQuery::SizeOf { argument, range } = &query {
                let before = budget.snapshot().retained_bytes;
                let mapped = (|| {
                    let anchor = argument_anchor(
                        root,
                        occurrence,
                        text,
                        &directive.body,
                        directive.start,
                        range.clone(),
                        budget,
                    )?;
                    if text.get(anchor.range.clone()) != Some(*argument) {
                        return Err(IncompleteReason::MalformedSource);
                    }
                    Ok(())
                })();
                budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
                mapped?;
            }
            super::arrays::scalar_query(state, &site, environment, query, 0, budget)
        },
    )
}

impl SourceOrder for RootSession<'_> {
    fn next_directive(
        &mut self,
        source: &str,
        cursor: usize,
    ) -> Result<Option<ConditionalDirective>, ()> {
        let next = self.scanner.next(source, cursor, self.cancel)?;
        let Some((range, body)) = next else {
            return Ok(None);
        };
        if self.directive_count >= conditional::MAX_DIRECTIVES {
            self.record(IncompleteReason::Limit {
                name: "max_directives",
                maximum: conditional::MAX_DIRECTIVES,
            });
            return Err(());
        }
        self.directive_count += 1;
        if !self.admit_directive(body.len()) {
            return Err(());
        }
        let kind = conditional::directive_kind(body);
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
        Ok(Some(ConditionalDirective {
            kind,
            body: body.to_owned(),
            start: range.start,
            end: range.end,
            activity: Truth::Unknown,
            condition: None,
        }))
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
        // Lexer RawDirective, final directive/clone, mask/activity ranges, frame
        // metadata and possible DEFINE/option keys. Actual body length is known
        // before the lexer copies it; no worst-case whole-source prefix parse.
        let retained = body_bytes.saturating_mul(4).saturating_add(
            4 * size_of::<ConditionalDirective>()
                + 8 * size_of::<Range<usize>>()
                + 4 * size_of::<String>()
                + size_of::<ConditionalEnvironment>(),
        );
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
    fn typed_environment_snapshot_retains_partial_graph_maps_options_and_provenance() {
        let text = "unit T; interface type R=packed record X:";
        let root = LoadedSource {
            id: SourceId::new("/fixture/snapshot.pas"),
            path: "/fixture/snapshot.pas".into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
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
        let before = budget.snapshot().retained_bytes;
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
        budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
        result
    })
}
