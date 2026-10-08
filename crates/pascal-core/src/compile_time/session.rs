//! Source-backed root session. Textual includes, providers, verified headers,
//! and complete alternative snapshots are separate follow-on owners.
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
/// Resource capabilities are deliberately not invoked until their session owners
/// exist: a required include is an explicit unavailable barrier, never empty text.
pub fn analyze_source(
    root: &LoadedSource,
    context: &ConditionalContext,
    _resolver: &mut dyn CompileTimeResolver,
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
    let mut session = RootSession {
        root,
        text,
        state,
        budget,
        reasons: Vec::new(),
        includes: Vec::new(),
        fatal: None,
        option_restore_unanalyzed: false,
    };
    let mut analysis = conditional::analyze_with_source_order(text, context, cancel, &mut session);
    if let Some(reason) = session.fatal.take() {
        return Err(resolver_error(reason, &session.budget));
    }
    if session.state.failure.is_none() {
        if let Err(reason) = session.state.finish_root(&mut session.budget) {
            session.record(reason);
        } else if session.state.unknown {
            session.record(IncompleteReason::MalformedSource);
        }
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
        occurrences: vec![occurrence],
        includes: session.includes,
        facts: session.state,
        header: Fact::Unknown(UnknownReason::HeaderNotAnalyzed),
        reasons: session.reasons,
        observations: Vec::new(),
        warnings: Vec::new(),
    })
}

fn resolver_error(reason: IncompleteReason, budget: &CompileTimeBudget<'_>) -> ResolverError {
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
    root: &'a LoadedSource,
    text: &'a str,
    state: CompileTimeState,
    budget: CompileTimeBudget<'a>,
    reasons: Vec<IncompleteReason>,
    includes: Vec<IncludeOccurrence>,
    fatal: Option<IncompleteReason>,
    /// The layout stack is implemented, but restoring all conditional option
    /// facts requires the phase-B complete environment snapshot. Do not reuse
    /// post-push scalar options as though they were a restored snapshot.
    option_restore_unanalyzed: bool,
}

impl RootSession<'_> {
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

impl SourceOrder for RootSession<'_> {
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
        if activity == Truth::Unknown {
            self.record(IncompleteReason::BranchStateNotAnalyzed);
            return;
        }
        if let Err(reason) = self.state.feed(
            SourceFragment {
                source: &self.root.id,
                occurrence: OccurrenceId(0),
                physical_start: range.start,
                text: &self.text[range],
            },
            Truth::True,
            &mut self.budget,
        ) {
            self.record(reason);
        }
    }

    fn condition(
        &mut self,
        body: &str,
        start: usize,
        environment: &ConditionalEnvironment,
        complete: &mut bool,
    ) -> Truth {
        if self.state.failure.is_some() || self.fatal.is_some() {
            return Truth::Unknown;
        }
        let site = self.state.site();
        let state = &self.state;
        let result = conditional::evaluate_source_condition(
            body,
            environment,
            &mut self.budget,
            &mut |query, budget| match query {
                ExpressionQuery::LexicalName(name) => {
                    super::arrays::lexical_constant_name(state, &site, name, 0, budget)
                }
                ExpressionQuery::Name(name) => {
                    if super::bindings::lookup(state, &site, name, budget)?.is_some() {
                        return super::arrays::constant_name(state, &site, name, 0, budget);
                    }
                    if super::arrays::qualifier_blocked(state, &site, name, budget)? {
                        return Ok(None);
                    }
                    let value = super::arrays::constant_name(state, &site, name, 0, budget)?;
                    if value.is_some() {
                        Ok(value)
                    } else {
                        conditional::bound_define(environment, name, budget)
                    }
                }
                ExpressionQuery::Declared(name) => {
                    let mut known = super::bindings::lookup(state, &site, name, budget)?.is_some();
                    if !known && super::arrays::qualifier_blocked(state, &site, name, budget)? {
                        return Ok(None);
                    }
                    if !known {
                        for key in state.context.constants.keys() {
                            budget.charge(1, key.len().min(name.len()), 0)?;
                            if super::bindings::same_name(key, name) {
                                known = true;
                                break;
                            }
                        }
                    }
                    budget.charge(1, name.len(), 0)?;
                    Ok(if known {
                        Some(BoundValue::Constant(ConstantValue::Boolean(true)))
                    } else if state.context.rtl_constants_known
                        && ConditionalContext::is_rtl_version_constant(name)
                    {
                        Some(BoundValue::Constant(ConstantValue::Boolean(false)))
                    } else {
                        None
                    })
                }
                ExpressionQuery::SizeOf { argument, range } => {
                    // The original body slice includes its original whitespace.
                    // These coordinates are physical root bytes, not wrapper bytes.
                    let before = budget.snapshot().retained_bytes;
                    let anchor = argument_anchor(self.root, self.text, body, start, range, budget)?;
                    if self.text.get(anchor.range.clone()) != Some(argument) {
                        return Err(IncompleteReason::MalformedSource);
                    }
                    drop(anchor);
                    budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
                    sizeof_value(state, &site, argument, budget)
                }
            },
        );
        let result = result.map(|value| {
            if self.option_restore_unanalyzed && conditional::is_option_condition(body) {
                Truth::Unknown
            } else {
                value
            }
        });
        match result {
            Ok(Truth::Unknown) => {
                self.record(IncompleteReason::BranchStateNotAnalyzed);
                Truth::Unknown
            }
            Ok(value) => value,
            Err(reason) => {
                *complete = false;
                self.record(reason);
                Truth::Unknown
            }
        }
    }

    fn directive(&mut self, directive: &ConditionalDirective) {
        if self.fatal.is_some() || directive.activity == Truth::False {
            return;
        }
        if ["POPOPT", "POP"]
            .iter()
            .any(|key| layout_body_key(&directive.body).eq_ignore_ascii_case(key))
        {
            self.option_restore_unanalyzed = true;
        }
        if directive.kind == DirectiveKind::Include {
            const REASON: &str = "textual include analysis unavailable until Task8";
            let bytes = self.root.id.as_str().len() * 2
                + REASON.len() * 2
                + size_of::<IncludeOccurrence>()
                + size_of::<IncompleteReason>();
            if let Err(reason) = self.budget.charge(1, bytes, bytes) {
                self.fatal = Some(reason);
                return;
            }
            self.includes.push(IncludeOccurrence {
                owner: OccurrenceId(0),
                directive: SourceAnchor {
                    source: self.root.id.clone(),
                    occurrence: OccurrenceId(0),
                    range: directive.start..directive.end,
                },
                target: Resolution::Unavailable {
                    reason: REASON.into(),
                },
                child: None,
            });
            self.record(IncompleteReason::RequiredSource {
                source: self.root.id.clone(),
                reason: REASON.into(),
            });
        } else if self.state.failure.is_none() {
            if let Some(members) = conditional::switch_list_members(&directive.body) {
                for member in members {
                    if is_layout_body(member) {
                        if let Err(reason) = self.state.apply_layout_directive(
                            member,
                            directive.activity,
                            &mut self.budget,
                        ) {
                            self.record(reason);
                            break;
                        }
                    }
                }
            } else if let Some(members) = conditional::switch_list_candidates(&directive.body) {
                // Use the existing decoder's invalid-list candidate rules. An
                // invalid list cannot leave a dependent layout switch precise.
                for member in members {
                    if is_layout_body(member) {
                        if let Err(reason) = self.state.apply_layout_directive(
                            member,
                            Truth::Unknown,
                            &mut self.budget,
                        ) {
                            self.record(reason);
                            break;
                        }
                    }
                }
            } else if is_layout_body(&directive.body) {
                if directive.body.contains(',') {
                    // Numeric/mixed lists have no verified list grammar here;
                    // invalidate each named family, never keep a stale H/Z fact.
                    for member in directive.body.split(',') {
                        if is_layout_body(member) {
                            if let Err(reason) = self.state.apply_layout_directive(
                                member,
                                Truth::Unknown,
                                &mut self.budget,
                            ) {
                                self.record(reason);
                                break;
                            }
                        }
                    }
                } else if let Err(reason) = self.state.apply_layout_directive(
                    &directive.body,
                    directive.activity,
                    &mut self.budget,
                ) {
                    self.record(reason);
                }
            }
        }
    }
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
        occurrence: OccurrenceId(0),
        range: physical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NoCancellation, SourceId, SourceRevision};

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
                    if matches!(query, ExpressionQuery::LexicalName(_)) {
                        return Ok(None);
                    }
                    let ExpressionQuery::SizeOf { argument, range } = query else {
                        panic!("generic evaluator consumed the operand");
                    };
                    seen += 1;
                    assert_eq!(argument, " value.Field ");
                    assert_eq!(&expression[range.clone()], argument);
                    let anchor = argument_anchor(&root, &text, body, start, range, budget)?;
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

/// One layout bridge used by IF and declaration-captured constant expressions.
/// No numeric literal typing, callable evaluation or memory reads are introduced.
pub(super) fn sizeof_value(
    state: &CompileTimeState,
    site: &BindingSite,
    argument: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<BoundValue>, IncompleteReason> {
    budget.with_layout_query(|budget| {
        let before = budget.snapshot().retained_bytes;
        let result = match parse_sizeof_operand(argument, budget) {
            Fact::Known(operand) => match state.layout_operand(site.clone(), &operand, budget) {
                Fact::Known(layout) => Ok(i64::try_from(layout.storage.size)
                    .ok()
                    .map(|n| BoundValue::Constant(ConstantValue::Integer(n)))),
                Fact::Unknown(_) => Ok(None),
                Fact::Incomplete(reason) => Err(reason),
            },
            Fact::Unknown(_) => Ok(None),
            Fact::Incomplete(reason) => Err(reason),
        };
        // Operand strings/selectors and returned layout identities are scratch;
        // only a scalar integer can survive this bridge.
        budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
        result
    })
}
