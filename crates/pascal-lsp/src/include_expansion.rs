#![allow(dead_code)]

use lsp_types::Url;
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use crate::conditional::{self, DirectiveKind, Truth};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalSpan {
    pub(crate) uri: Url,
    pub(crate) range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VirtualMapping {
    Exact(PhysicalSpan),
    Many(Vec<PhysicalSpan>),
    Unmapped,
}

pub(crate) struct MappingBudget<'a> {
    cancel: &'a AtomicBool,
    work: usize,
    max_work: usize,
}

impl<'a> MappingBudget<'a> {
    pub(crate) fn new(cancel: &'a AtomicBool, max_work: usize) -> Self {
        Self {
            cancel,
            work: 0,
            max_work,
        }
    }

    fn charge(&mut self) -> Result<(), String> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        self.work = self
            .work
            .checked_add(1)
            .ok_or_else(|| "include source-map work accounting overflowed".to_string())?;
        if self.work > self.max_work {
            return Err(format!(
                "include source-map work limit ({}) reached",
                self.max_work
            ));
        }
        Ok(())
    }

    pub(crate) fn cancellation(&self) -> &AtomicBool {
        self.cancel
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExpandedSource {
    text: String,
    map: SourceMap,
}

#[derive(Debug, Clone, Default)]
struct SourceMap {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
struct Segment {
    virtual_range: Range<usize>,
    mapping: SegmentMapping,
}

#[derive(Debug, Clone)]
enum SegmentMapping {
    Physical { uri: Url, source_start: usize },
    Synthetic,
}

impl ExpandedSource {
    fn new() -> Self {
        Self::default()
    }

    fn push_physical(&mut self, uri: Url, source: &str) -> Range<usize> {
        self.push_physical_range(uri, source, 0)
    }

    fn push_physical_range(&mut self, uri: Url, source: &str, source_start: usize) -> Range<usize> {
        let range = self.push_text(source);
        if !range.is_empty() {
            self.map.segments.push(Segment {
                virtual_range: range.clone(),
                mapping: SegmentMapping::Physical { uri, source_start },
            });
        }
        range
    }

    fn push_synthetic(&mut self, source: &str) -> Range<usize> {
        let range = self.push_text(source);
        if !range.is_empty() {
            self.map.segments.push(Segment {
                virtual_range: range.clone(),
                mapping: SegmentMapping::Synthetic,
            });
        }
        range
    }

    pub(crate) fn map_range(&self, range: Range<usize>) -> VirtualMapping {
        let cancel = AtomicBool::new(false);
        let mut budget = MappingBudget::new(&cancel, usize::MAX);
        self.map_range_with_budget(range, &mut budget)
            .unwrap_or(VirtualMapping::Unmapped)
    }

    pub(crate) fn map_range_with_budget(
        &self,
        range: Range<usize>,
        budget: &mut MappingBudget<'_>,
    ) -> Result<VirtualMapping, String> {
        if range.start >= range.end || range.end > self.text.len() {
            return Ok(VirtualMapping::Unmapped);
        }

        let mut cursor = range.start;
        let mut spans = Vec::new();
        for segment in &self.map.segments {
            budget.charge()?;
            if segment.virtual_range.end <= range.start {
                continue;
            }
            if segment.virtual_range.start >= range.end {
                break;
            }
            let start = range.start.max(segment.virtual_range.start);
            if start > cursor {
                return Ok(VirtualMapping::Unmapped);
            }
            let end = range.end.min(segment.virtual_range.end);
            if end <= start {
                continue;
            }
            let span = match &segment.mapping {
                SegmentMapping::Physical { uri, source_start } => PhysicalSpan {
                    uri: uri.clone(),
                    range: source_start + (start - segment.virtual_range.start)
                        ..source_start + (end - segment.virtual_range.start),
                },
                SegmentMapping::Synthetic => return Ok(VirtualMapping::Unmapped),
            };
            spans.push(span);
            cursor = end;
        }
        if cursor != range.end || spans.is_empty() {
            return Ok(VirtualMapping::Unmapped);
        }
        if spans.len() == 1 {
            Ok(VirtualMapping::Exact(spans.remove(0)))
        } else {
            Ok(VirtualMapping::Many(spans))
        }
    }

    pub(crate) fn reverse_range(&self, uri: &Url, range: Range<usize>) -> Vec<Range<usize>> {
        let cancel = AtomicBool::new(false);
        let mut budget = MappingBudget::new(&cancel, usize::MAX);
        self.reverse_range_with_budget(uri, range, &mut budget)
            .unwrap_or_default()
    }

    pub(crate) fn reverse_range_with_budget(
        &self,
        uri: &Url,
        range: Range<usize>,
        budget: &mut MappingBudget<'_>,
    ) -> Result<Vec<Range<usize>>, String> {
        if range.start >= range.end {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for segment in &self.map.segments {
            budget.charge()?;
            let Some(mapped) = (|| {
                let SegmentMapping::Physical {
                    uri: segment_uri,
                    source_start,
                } = &segment.mapping
                else {
                    return None;
                };
                if segment_uri != uri {
                    return None;
                }
                let segment_source = *source_start
                    ..*source_start + (segment.virtual_range.end - segment.virtual_range.start);
                let start = range.start.max(segment_source.start);
                let end = range.end.min(segment_source.end);
                if start >= end {
                    return None;
                }
                Some(
                    segment.virtual_range.start + (start - segment_source.start)
                        ..segment.virtual_range.start + (end - segment_source.start),
                )
            })() else {
                continue;
            };
            result.push(mapped);
        }
        Ok(result)
    }

    fn push_text(&mut self, source: &str) -> Range<usize> {
        let start = self.text.len();
        self.text.push_str(source);
        start..self.text.len()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    fn append(&mut self, child: &ExpandedSource) {
        let offset = self.text.len();
        self.text.push_str(&child.text);
        self.map
            .segments
            .extend(child.map.segments.iter().cloned().map(|mut segment| {
                segment.virtual_range.start += offset;
                segment.virtual_range.end += offset;
                segment
            }));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExpansionLimits {
    pub(crate) max_depth: usize,
    pub(crate) max_sources: usize,
    pub(crate) max_directives: usize,
    pub(crate) max_expanded_bytes: usize,
    pub(crate) max_segments: usize,
    pub(crate) max_work: usize,
}

impl Default for ExpansionLimits {
    fn default() -> Self {
        Self {
            // Expansion is recursive by design, so keep the default well
            // below the thread stack's failure point. Callers still receive
            // a bounded, fail-closed result when a deeper chain is present.
            max_depth: 64,
            max_sources: 4_096,
            max_directives: 16_384,
            max_expanded_bytes: 256 * 1024 * 1024,
            max_segments: 1_000_000,
            max_work: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedInclude {
    pub(crate) uri: Url,
    pub(crate) text: String,
    /// The requester-scoped read authorization selected while resolving this
    /// include.  A physical dependency can be outside the workspace (for
    /// example through a legacy relative include), so reconstructing this
    /// entry later from its path alone would incorrectly reject a valid
    /// expansion or accidentally grant configured-path authority.
    pub(crate) path_entry: Option<pascal_project::ProjectPathEntry>,
    pub(crate) observations: Vec<IncludeObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncludeObservation {
    pub(crate) path: PathBuf,
    pub(crate) stamp: Option<pascal_project::ProjectReadStamp>,
    pub(crate) present: bool,
    pub(crate) overlay_version: Option<i32>,
    pub(crate) content_hash: Option<u64>,
}

pub(crate) trait IncludeResolver {
    fn resolve_include(
        &mut self,
        owner: &Url,
        directive: &str,
        cancel: &AtomicBool,
    ) -> Result<ResolvedInclude, String>;

    fn include_size_hint(
        &mut self,
        _owner: &Url,
        _directive: &str,
        _cancel: &AtomicBool,
    ) -> Result<Option<usize>, String> {
        Ok(None)
    }
}

#[derive(Debug)]
pub(crate) struct ExpansionResult {
    pub(crate) expanded: ExpandedSource,
    pub(crate) dependencies: Vec<ResolvedInclude>,
    pub(crate) complete: bool,
    pub(crate) errors: Vec<String>,
}

struct ExpansionState<'a> {
    result: ExpansionResult,
    limits: ExpansionLimits,
    sources: usize,
    directives: usize,
    work: usize,
    retained_source_bytes: usize,
    expanded_bytes: usize,
    segments: usize,
    active: HashSet<Url>,
    cancel: &'a AtomicBool,
}

impl ExpansionState<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn charge_work(&mut self, amount: usize) -> Result<(), String> {
        if self.cancelled() {
            return Err("request cancelled".to_string());
        }
        self.work = self
            .work
            .checked_add(amount)
            .ok_or_else(|| "include expansion work accounting overflowed".to_string())?;
        if self.work > self.limits.max_work {
            return Err(format!(
                "include expansion work limit ({}) reached",
                self.limits.max_work
            ));
        }
        Ok(())
    }

    fn append_physical(
        &mut self,
        output: &mut ExpandedSource,
        uri: Url,
        source: &str,
        source_start: usize,
    ) -> Result<(), String> {
        self.charge_work(source.len())?;
        let new_size = self
            .expanded_bytes
            .checked_add(source.len())
            .ok_or_else(|| "expanded include source size overflowed".to_string())?;
        if new_size > self.limits.max_expanded_bytes {
            return Err(format!(
                "expanded include byte limit ({}) reached",
                self.limits.max_expanded_bytes
            ));
        }
        if self.segments >= self.limits.max_segments {
            return Err(format!(
                "include source-map segment limit ({}) reached",
                self.limits.max_segments
            ));
        }
        output.push_physical_range(uri, source, source_start);
        self.expanded_bytes = new_size;
        self.segments += 1;
        Ok(())
    }

    fn append_synthetic(
        &mut self,
        output: &mut ExpandedSource,
        source: &str,
    ) -> Result<(), String> {
        self.charge_work(source.len())?;
        let new_size = self
            .expanded_bytes
            .checked_add(source.len())
            .ok_or_else(|| "expanded include source size overflowed".to_string())?;
        if new_size > self.limits.max_expanded_bytes {
            return Err(format!(
                "expanded include byte limit ({}) reached",
                self.limits.max_expanded_bytes
            ));
        }
        if self.segments >= self.limits.max_segments {
            return Err(format!(
                "include source-map segment limit ({}) reached",
                self.limits.max_segments
            ));
        }
        output.push_synthetic(source);
        self.expanded_bytes = new_size;
        self.segments += 1;
        Ok(())
    }

    fn retain_payload(&mut self, bytes: usize) -> Result<(), String> {
        self.retained_source_bytes = self
            .retained_source_bytes
            .checked_add(bytes)
            .ok_or_else(|| "retained include source size overflowed".to_string())?;
        if self.retained_source_bytes > self.limits.max_work {
            return Err(format!(
                "include retained-source work limit ({}) reached",
                self.limits.max_work
            ));
        }
        Ok(())
    }

    fn incomplete(&mut self, error: String) {
        self.result.complete = false;
        if self.result.errors.len() < 256 {
            self.result.errors.push(error);
        }
    }
}

pub(crate) fn expand_source<R: IncludeResolver>(
    root_uri: Url,
    source: &str,
    defines: &[String],
    resolver: &mut R,
    limits: ExpansionLimits,
    cancel: &AtomicBool,
) -> Result<ExpansionResult, String> {
    let mut state = ExpansionState {
        result: ExpansionResult {
            expanded: ExpandedSource::new(),
            dependencies: Vec::new(),
            complete: true,
            errors: Vec::new(),
        },
        limits,
        sources: 0,
        directives: 0,
        work: 0,
        retained_source_bytes: 0,
        expanded_bytes: 0,
        segments: 0,
        active: HashSet::new(),
        cancel,
    };
    let mut environment = conditional::ConditionalEnvironment::from_defines(defines);
    let (complete, expanded) =
        expand_file(&mut state, root_uri, source, &mut environment, resolver, 0)?;
    state.result.expanded = expanded;
    state.result.complete &= complete;
    Ok(state.result)
}

/// The first expansion pass must conservatively retain source from unknown
/// branches so a preceding include can establish the facts that resolve them.
/// Once the complete expanded buffer has been evaluated, discard only those
/// provisional unknown-activity errors that the second pass proved resolved.
pub(crate) fn reconcile_conditional_completeness(
    result: &mut ExpansionResult,
    analysis: &conditional::ConditionalAnalysis,
) {
    if !analysis.complete || analysis.unknown_activity_requires_fail_closed() {
        result.complete = false;
        if result.errors.len() < 256 {
            result
                .errors
                .push("expanded conditional analysis did not complete".to_string());
        }
        return;
    }
    if !analysis.unknown_activity_requires_fail_closed() {
        result.errors.retain(|error| {
            !error.contains("include activity is unknown")
                && !error.contains("conditional activity is unknown")
        });
    }
    if result.errors.is_empty() {
        result.complete = true;
    }
}

fn expand_file<R: IncludeResolver>(
    state: &mut ExpansionState,
    uri: Url,
    source: &str,
    environment: &mut conditional::ConditionalEnvironment,
    resolver: &mut R,
    depth: usize,
) -> Result<(bool, ExpandedSource), String> {
    if state.cancelled() {
        return Err("request cancelled".to_string());
    }
    if depth > state.limits.max_depth {
        state.incomplete(format!(
            "include expansion depth limit ({}) reached at {}",
            state.limits.max_depth, uri
        ));
        return Ok((false, ExpandedSource::new()));
    }
    if state.sources >= state.limits.max_sources {
        state.incomplete(format!(
            "include expansion source limit ({}) reached",
            state.limits.max_sources
        ));
        return Ok((false, ExpandedSource::new()));
    }
    if !state.active.insert(uri.clone()) {
        state.incomplete(format!("include cycle detected at {uri}"));
        return Ok((false, ExpandedSource::new()));
    }
    state.sources += 1;

    // Count the directives before invoking the stateful evaluator.  This is a
    // syntax-only pass: it cannot resolve an include, retain a payload, or
    // mutate the shared define environment.  Exhaustion therefore stops the
    // request before the callback can perform any read.
    let directive_count = conditional::analyze_with_cancel(source, &[], state.cancel)
        .directives
        .len();
    if state.directives.saturating_add(directive_count) > state.limits.max_directives {
        state.incomplete(format!(
            "include directive limit ({}) reached",
            state.limits.max_directives
        ));
        state.active.remove(&uri);
        return Ok((false, ExpandedSource::new()));
    }
    state.directives = state.directives.saturating_add(directive_count);

    let mut include_results: Vec<Option<ChildExpansion>> = Vec::new();
    let mut callback_error = None;
    let analysis = conditional::analyze_with_include_callback(
        source,
        environment,
        state.cancel,
        &mut |directive, environment| {
            let mut transition = conditional::IncludeTransition {
                complete: false,
                environment_known: false,
            };
            if depth >= state.limits.max_depth {
                state.incomplete(format!(
                    "include expansion depth limit ({}) reached before resolving an include in {uri}",
                    state.limits.max_depth
                ));
                include_results.push(None);
                return transition;
            }
            if state.sources >= state.limits.max_sources {
                state.incomplete(format!(
                    "include expansion source limit ({}) reached before resolving an include in {uri}",
                    state.limits.max_sources
                ));
                include_results.push(None);
                return transition;
            }
            if state.segments >= state.limits.max_segments {
                state.incomplete(format!(
                    "include source-map segment limit ({}) reached before resolving an include in {uri}",
                    state.limits.max_segments
                ));
                include_results.push(None);
                return transition;
            }
            if let Err(error) = state.charge_work(1) {
                callback_error = Some(error);
                include_results.push(None);
                return transition;
            }
            let size_hint = match resolver.include_size_hint(&uri, &directive.body, state.cancel) {
                Ok(size_hint) => size_hint,
                Err(error) => {
                    callback_error = Some(error);
                    include_results.push(None);
                    return transition;
                }
            };
            if let Some(size) = size_hint {
                if state
                    .expanded_bytes
                    .checked_add(size)
                    .is_none_or(|size| size > state.limits.max_expanded_bytes)
                {
                    state.incomplete(format!(
                        "expanded include byte limit ({}) reached before reading an include",
                        state.limits.max_expanded_bytes
                    ));
                    include_results.push(None);
                    return transition;
                }
                if let Err(error) = state.charge_work(size) {
                    state.incomplete(error);
                    include_results.push(None);
                    return transition;
                }
            }

            match resolver.resolve_include(&uri, &directive.body, state.cancel) {
                Ok(resolved) => {
                    let actual_size = resolved.text.len();
                    if size_hint.is_none() {
                        if let Err(error) = state.charge_work(actual_size) {
                            state.incomplete(error);
                            include_results.push(None);
                            return transition;
                        }
                    } else if size_hint.is_some_and(|hint| actual_size > hint) {
                        if let Err(error) = state.charge_work(actual_size - size_hint.unwrap()) {
                            state.incomplete(error);
                            include_results.push(None);
                            return transition;
                        }
                    }
                    if state
                        .expanded_bytes
                        .checked_add(actual_size)
                        .is_none_or(|size| size > state.limits.max_expanded_bytes)
                    {
                        state.incomplete(format!(
                            "expanded include byte limit ({}) reached",
                            state.limits.max_expanded_bytes
                        ));
                        include_results.push(None);
                        return transition;
                    }
                    if let Err(error) = state.retain_payload(actual_size) {
                        state.incomplete(error);
                        include_results.push(None);
                        return transition;
                    }
                    if state.active.contains(&resolved.uri) {
                        state.incomplete(format!("include cycle detected at {}", resolved.uri));
                        include_results.push(None);
                        return transition;
                    }
                    state.result.dependencies.push(resolved.clone());
                    match expand_file(
                        state,
                        resolved.uri.clone(),
                        &resolved.text,
                        environment,
                        resolver,
                        depth + 1,
                    ) {
                        Ok((child_complete, expanded)) => {
                            transition.complete = child_complete;
                            transition.environment_known = child_complete;
                            include_results.push(Some(ChildExpansion {
                                expanded,
                                text: resolved.text,
                                complete: child_complete,
                            }));
                        }
                        Err(error) => {
                            callback_error = Some(error);
                            include_results.push(None);
                        }
                    }
                }
                Err(error) => {
                    state.incomplete(format!(
                        "could not resolve active include in {uri}: {error}"
                    ));
                    include_results.push(None);
                }
            }
            transition
        },
    );
    if let Some(error) = callback_error {
        state.active.remove(&uri);
        return Err(error);
    }
    let unknown_requires_fail_closed = analysis.unknown_activity_requires_fail_closed();
    let mut file_complete = analysis.complete && !unknown_requires_fail_closed;
    if !analysis.complete || unknown_requires_fail_closed {
        state.incomplete(format!("conditional analysis is incomplete for {uri}"));
    }

    let mut output = ExpandedSource::new();
    let mut cursor = 0;
    let mut include_index = 0;
    for directive in &analysis.directives {
        if state.cancelled() {
            state.active.remove(&uri);
            return Err("request cancelled".to_string());
        }
        if directive.start > cursor {
            append_region(
                state,
                &mut output,
                &uri,
                source,
                &analysis,
                cursor,
                directive.start,
            )?;
        }
        let directive_source = source
            .get(directive.start..directive.end)
            .ok_or_else(|| "conditional directive span was not on a UTF-8 boundary".to_string())?;
        // The include marker is only a source-map placeholder.  Keeping the
        // textual `{$I ...}` in the expanded buffer would make the shared
        // conditional analyzer conservatively clear the define environment
        // immediately before it sees the included file's directives.  Mask
        // that marker while retaining its exact byte/line footprint; all
        // actual conditional/define directives remain visible to the second
        // analysis pass over the expanded source.
        if directive.kind == DirectiveKind::Include {
            state.append_synthetic(&mut output, &mask_directive_bytes(directive_source))?;
        } else {
            state.append_synthetic(&mut output, directive_source)?;
        }
        if directive.kind == DirectiveKind::Include && directive.activity == Truth::True {
            let child = include_results
                .get_mut(include_index)
                .and_then(Option::take);
            include_index += 1;
            if let Some(child) = child {
                if !child.complete {
                    file_complete = false;
                }
                output.append(&child.expanded);
                if source
                    .get(directive.end..)
                    .and_then(|tail| tail.as_bytes().first())
                    .is_some_and(|byte| !byte.is_ascii_whitespace())
                    && !child.text.ends_with(['\n', '\r'])
                {
                    state.append_synthetic(&mut output, "\n")?;
                }
            } else {
                file_complete = false;
            }
        }
        cursor = directive.end;
    }
    if cursor < source.len() {
        append_region(
            state,
            &mut output,
            &uri,
            source,
            &analysis,
            cursor,
            source.len(),
        )?;
    }
    state.active.remove(&uri);
    Ok((file_complete, output))
}

struct ChildExpansion {
    expanded: ExpandedSource,
    text: String,
    complete: bool,
}

fn update_known_define(defines: &mut Vec<String>, directive: &conditional::ConditionalDirective) {
    let Some(symbol) = conditional::defined_symbol(&directive.body) else {
        return;
    };
    if directive.activity == Truth::False {
        return;
    }
    defines.retain(|define| !define.eq_ignore_ascii_case(&symbol));
    if directive.kind == DirectiveKind::Define && directive.activity == Truth::True {
        defines.push(symbol);
    }
}

fn mask_directive_bytes(source: &str) -> String {
    source
        .chars()
        .map(|character| match character {
            '\n' | '\r' => character,
            _ => ' ',
        })
        .collect()
}

fn append_region(
    state: &mut ExpansionState,
    output: &mut ExpandedSource,
    uri: &Url,
    source: &str,
    analysis: &conditional::ConditionalAnalysis,
    start: usize,
    end: usize,
) -> Result<(), String> {
    let unknown_requires_fail_closed = analysis.unknown_activity_requires_fail_closed();
    let mut boundaries = vec![start, end];
    boundaries.extend(
        analysis
            .inactive_spans
            .iter()
            .chain(analysis.unknown_spans.iter())
            .flat_map(|span| [span.start.max(start).min(end), span.end.max(start).min(end)]),
    );
    boundaries.sort_unstable();
    boundaries.dedup();
    for window in boundaries.windows(2) {
        let part_start = window[0];
        let part_end = window[1];
        if part_start >= part_end {
            continue;
        }
        let unknown = analysis
            .unknown_spans
            .iter()
            .any(|span| part_start >= span.start && part_end <= span.end);
        let original = source
            .get(part_start..part_end)
            .ok_or_else(|| "include source region split a UTF-8 scalar".to_string())?;
        if unknown && unknown_requires_fail_closed {
            state.incomplete(format!("conditional activity is unknown in {uri}"));
        }
        state.append_physical(output, uri.clone(), original, part_start)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ExpandedSource, ExpansionLimits, IncludeResolver, PhysicalSpan, ResolvedInclude,
        VirtualMapping, expand_source,
    };
    use lsp_types::Url;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    fn uri(path: &str) -> Url {
        Url::from_file_path(path).expect("file URI")
    }

    #[test]
    fn maps_unicode_crlf_and_synthetic_bytes_without_authorizing_edits() {
        let root = uri("/workspace/Main.pas");
        let mut expanded = ExpandedSource::new();
        let root_range = expanded.push_physical(root.clone(), "😀\r\nroot");
        let synthetic = expanded.push_synthetic("\n");
        let included = expanded.push_physical(uri("/workspace/Shared.inc"), "included");

        assert_eq!(root_range, 0..10);
        assert_eq!(synthetic, 10..11);
        assert_eq!(included, 11..19);
        assert_eq!(
            expanded.map_range(0..4),
            VirtualMapping::Exact(PhysicalSpan {
                uri: root.clone(),
                range: 0..4,
            })
        );
        assert_eq!(expanded.map_range(synthetic), VirtualMapping::Unmapped);
        assert_eq!(
            expanded.map_range(included),
            VirtualMapping::Exact(PhysicalSpan {
                uri: uri("/workspace/Shared.inc"),
                range: 0..8,
            })
        );
    }

    #[test]
    fn reverse_mapping_returns_all_repeated_physical_occurrences() {
        let include = uri("/workspace/Shared.inc");
        let mut expanded = ExpandedSource::new();
        expanded.push_physical(uri("/workspace/Main.pas"), "before");
        expanded.push_physical(include.clone(), "value");
        expanded.push_synthetic("\n");
        expanded.push_physical(include.clone(), "value");

        assert_eq!(expanded.reverse_range(&include, 0..5), vec![6..11, 12..17]);
    }

    #[test]
    fn reverse_mapping_ignores_ranges_before_a_physical_segment() {
        let root = uri("/workspace/Main.pas");
        let mut expanded = ExpandedSource::new();
        expanded.push_physical_range(root.clone(), "tail", 4);

        assert!(expanded.reverse_range(&root, 0..3).is_empty());
    }

    #[test]
    fn crossing_physical_segments_is_many_and_never_an_exact_edit_span() {
        let root = uri("/workspace/Main.pas");
        let include = uri("/workspace/Shared.inc");
        let mut expanded = ExpandedSource::new();
        expanded.push_physical(root.clone(), "root");
        expanded.push_physical(include.clone(), "include");

        assert_eq!(
            expanded.map_range(2..7),
            VirtualMapping::Many(vec![
                PhysicalSpan {
                    uri: root,
                    range: 2..4,
                },
                PhysicalSpan {
                    uri: include,
                    range: 0..3,
                },
            ])
        );
    }

    #[derive(Default)]
    struct FixtureResolver {
        sources: HashMap<String, String>,
        calls: Vec<String>,
    }

    impl IncludeResolver for FixtureResolver {
        fn resolve_include(
            &mut self,
            _owner: &Url,
            directive: &str,
            _cancel: &AtomicBool,
        ) -> Result<ResolvedInclude, String> {
            let name = directive
                .trim_start()
                .split_once(char::is_whitespace)
                .map_or("", |(_, name)| name.trim().trim_matches(['\'', '"']));
            self.calls.push(name.to_owned());
            let text = self
                .sources
                .get(name)
                .cloned()
                .ok_or_else(|| format!("missing {name}"))?;
            Ok(ResolvedInclude {
                uri: uri(&format!("/workspace/{name}")),
                text,
                path_entry: None,
                observations: Vec::new(),
            })
        }
    }

    #[test]
    fn expands_nested_and_repeated_active_includes_with_reversible_mapping() {
        let root = uri("/workspace/Main.pas");
        let mut resolver = FixtureResolver {
            sources: HashMap::from([
                ("A.inc".to_owned(), "{$I B.inc}\nconst A = 1;\n".to_owned()),
                ("B.inc".to_owned(), "const Included = 1;\n".to_owned()),
            ]),
            ..FixtureResolver::default()
        };
        let result = expand_source(
            root,
            "unit Main;\ninterface\n{$I A.inc}\n{$I A.inc}\nimplementation\nend.\n",
            &[],
            &mut resolver,
            ExpansionLimits::default(),
            &AtomicBool::new(false),
        )
        .expect("expansion");

        assert!(result.complete, "unexpected errors: {:?}", result.errors);
        assert_eq!(resolver.calls, ["A.inc", "B.inc", "A.inc", "B.inc"]);
        assert_eq!(result.expanded.text().matches("const Included").count(), 2);
        let include_uri = uri("/workspace/B.inc");
        let source_range = 0.."const Included".len();
        assert_eq!(
            result.expanded.reverse_range(&include_uri, source_range),
            vec![
                result.expanded.text().find("const Included").unwrap()
                    ..result.expanded.text().find("const Included").unwrap()
                        + "const Included".len(),
                result.expanded.text().rfind("const Included").unwrap()
                    ..result.expanded.text().rfind("const Included").unwrap()
                        + "const Included".len(),
            ]
        );
    }

    #[test]
    fn inactive_includes_are_not_loaded_but_unknown_activity_is_incomplete() {
        let root = uri("/workspace/Main.pas");
        let mut resolver = FixtureResolver {
            sources: HashMap::from([("Active.inc".to_owned(), "const Active = 1;\n".to_owned())]),
            ..FixtureResolver::default()
        };
        let result = expand_source(
            root,
            "unit Main;\ninterface\n{$IF False}\n{$I Missing.inc}\n{$ENDIF}\n{$IFDEF UNKNOWN}\n{$I Active.inc}\n{$ENDIF}\nimplementation\nend.\n",
            &[],
            &mut resolver,
            ExpansionLimits::default(),
            &AtomicBool::new(false),
        )
        .expect("bounded expansion");

        assert!(!result.complete);
        assert!(resolver.calls.is_empty(), "unknown includes must not load");
        assert!(result.errors.iter().any(|error| error.contains("unknown")));
    }

    #[test]
    fn expansion_limits_and_cancellation_fail_closed() {
        let root = uri("/workspace/Main.pas");
        let mut resolver = FixtureResolver {
            sources: HashMap::from([("A.inc".to_owned(), "const A = 1;\n".to_owned())]),
            ..FixtureResolver::default()
        };

        let depth_limits = ExpansionLimits {
            max_depth: 0,
            ..ExpansionLimits::default()
        };
        let depth = expand_source(
            root.clone(),
            "{$I A.inc}\n",
            &[],
            &mut resolver,
            depth_limits,
            &AtomicBool::new(false),
        )
        .expect("depth-bounded expansion");
        assert!(!depth.complete);
        assert!(depth.errors.iter().any(|error| error.contains("depth")));

        let source_limits = ExpansionLimits {
            max_sources: 1,
            ..ExpansionLimits::default()
        };
        let sources = expand_source(
            root.clone(),
            "{$I A.inc}\n",
            &[],
            &mut resolver,
            source_limits,
            &AtomicBool::new(false),
        )
        .expect("source-bounded expansion");
        assert!(!sources.complete);
        assert!(
            sources
                .errors
                .iter()
                .any(|error| error.contains("source limit"))
        );

        let directive_limits = ExpansionLimits {
            max_directives: 0,
            ..ExpansionLimits::default()
        };
        let directives = expand_source(
            root.clone(),
            "{$I A.inc}\n",
            &[],
            &mut resolver,
            directive_limits,
            &AtomicBool::new(false),
        )
        .expect("directive-bounded expansion");
        assert!(!directives.complete);
        assert!(
            directives
                .errors
                .iter()
                .any(|error| error.contains("directive limit"))
        );

        let byte_limits = ExpansionLimits {
            max_expanded_bytes: 1,
            ..ExpansionLimits::default()
        };
        let error = expand_source(
            root.clone(),
            "12",
            &[],
            &mut resolver,
            byte_limits,
            &AtomicBool::new(false),
        )
        .expect_err("expanded byte limit must stop expansion");
        assert!(error.contains("byte limit"));

        let work_limits = ExpansionLimits {
            max_work: 0,
            ..ExpansionLimits::default()
        };
        let error = expand_source(
            root.clone(),
            "x",
            &[],
            &mut resolver,
            work_limits,
            &AtomicBool::new(false),
        )
        .expect_err("work limit must stop expansion");
        assert!(error.contains("work limit"));

        let segment_limits = ExpansionLimits {
            max_segments: 1,
            ..ExpansionLimits::default()
        };
        let error = expand_source(
            root.clone(),
            "{$I A.inc}\n",
            &[],
            &mut resolver,
            segment_limits,
            &AtomicBool::new(false),
        )
        .expect_err("source-map segment limit must stop expansion");
        assert!(error.contains("segment limit"));

        let cancelled = AtomicBool::new(true);
        let error = expand_source(
            root,
            "x",
            &[],
            &mut resolver,
            ExpansionLimits::default(),
            &cancelled,
        )
        .expect_err("cancelled expansion must stop before reading source");
        assert_eq!(error, "request cancelled");
    }

    #[test]
    fn exhausted_admission_budgets_do_not_resolve_or_retain_includes() {
        let root = uri("/workspace/Main.pas");
        let mut resolver = FixtureResolver {
            sources: HashMap::from([("Empty.inc".to_owned(), String::new())]),
            ..FixtureResolver::default()
        };
        let source = "{$I Empty.inc}\n".repeat(100);

        let directive_limited = expand_source(
            root.clone(),
            &source,
            &[],
            &mut resolver,
            ExpansionLimits {
                max_directives: 0,
                ..ExpansionLimits::default()
            },
            &AtomicBool::new(false),
        )
        .expect("directive-bounded expansion");
        assert!(
            resolver.calls.is_empty(),
            "directive admission must stop before lookup"
        );
        assert!(directive_limited.dependencies.is_empty());

        resolver.calls.clear();
        let source_limited = expand_source(
            root.clone(),
            &source,
            &[],
            &mut resolver,
            ExpansionLimits {
                max_sources: 1,
                ..ExpansionLimits::default()
            },
            &AtomicBool::new(false),
        )
        .expect("source-bounded expansion");
        assert!(
            resolver.calls.is_empty(),
            "source admission must stop before lookup"
        );
        assert!(source_limited.dependencies.is_empty());

        resolver.calls.clear();
        let depth_limited = expand_source(
            root,
            &source,
            &[],
            &mut resolver,
            ExpansionLimits {
                max_depth: 0,
                ..ExpansionLimits::default()
            },
            &AtomicBool::new(false),
        )
        .expect("depth-bounded expansion");
        assert!(
            resolver.calls.is_empty(),
            "depth admission must stop before lookup"
        );
        assert!(depth_limited.dependencies.is_empty());
    }

    #[test]
    fn include_define_and_undef_facts_flow_in_source_order() {
        let root = uri("/workspace/Main.pas");
        let mut resolver = FixtureResolver {
            sources: HashMap::from([
                ("Defines.inc".to_owned(), "{$DEFINE ENABLED}\n".to_owned()),
                ("Disables.inc".to_owned(), "{$UNDEF ENABLED}\n".to_owned()),
                (
                    "Value.inc".to_owned(),
                    "const SharedValue = 1;\n".to_owned(),
                ),
            ]),
            ..FixtureResolver::default()
        };

        let enabled = expand_source(
            root.clone(),
            "{$I Defines.inc}\n{$IFDEF ENABLED}\n{$I Value.inc}\n{$ENDIF}\n",
            &[],
            &mut resolver,
            ExpansionLimits::default(),
            &AtomicBool::new(false),
        )
        .expect("sequential define expansion");
        assert!(enabled.complete, "unexpected errors: {:?}", enabled.errors);
        assert_eq!(resolver.calls, ["Defines.inc", "Value.inc"]);
        assert!(enabled.expanded.text().contains("const SharedValue"));

        resolver.calls.clear();
        let disabled = expand_source(
            root,
            "{$I Disables.inc}\n{$IFNDEF ENABLED}\n{$I Value.inc}\n{$ENDIF}\n",
            &["ENABLED".to_owned()],
            &mut resolver,
            ExpansionLimits::default(),
            &AtomicBool::new(false),
        )
        .expect("sequential undef expansion");
        assert!(
            disabled.complete,
            "unexpected errors: {:?}",
            disabled.errors
        );
        assert_eq!(resolver.calls, ["Disables.inc", "Value.inc"]);
        assert!(disabled.expanded.text().contains("const SharedValue"));
    }

    #[test]
    fn resolved_dependencies_retain_requester_scoped_provenance() {
        struct ProvenanceResolver;

        impl IncludeResolver for ProvenanceResolver {
            fn resolve_include(
                &mut self,
                _owner: &Url,
                _directive: &str,
                _cancel: &AtomicBool,
            ) -> Result<ResolvedInclude, String> {
                let path = std::path::PathBuf::from("/mapped/Shared.inc");
                Ok(ResolvedInclude {
                    uri: uri("/workspace/Shared.inc"),
                    text: "{$DEFINE SAFE}\n".to_owned(),
                    path_entry: Some(pascal_project::ProjectPathEntry {
                        path,
                        provenance: pascal_project::ProjectPathProvenance::Mapped {
                            root: std::path::PathBuf::from("/mapped"),
                        },
                    }),
                    observations: Vec::new(),
                })
            }
        }

        let mut resolver = ProvenanceResolver;
        let result = expand_source(
            uri("/workspace/Main.pas"),
            "{$I Shared.inc}\n",
            &[],
            &mut resolver,
            ExpansionLimits::default(),
            &AtomicBool::new(false),
        )
        .expect("provenance expansion");
        assert_eq!(result.dependencies.len(), 1);
        assert!(matches!(
            result.dependencies[0]
                .path_entry
                .as_ref()
                .expect("dependency provenance")
                .provenance,
            pascal_project::ProjectPathProvenance::Mapped { .. }
        ));
    }
}
