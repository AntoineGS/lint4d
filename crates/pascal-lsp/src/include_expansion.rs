#![allow(dead_code)]

use lsp_types::Url;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use pascal_core::compile_time::OccurrenceId;
use pascal_core::conditional::{self, DirectiveKind};
use pascal_project::ConditionalContext;

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
    pub(crate) session: Option<std::sync::Arc<pascal_core::compile_time::CompileTimeAnalysis>>,
    pub(crate) conditionals: Option<conditional::ConditionalAnalysis>,
    pub(crate) observations: Vec<IncludeObservation>,
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
    Physical {
        uri: Url,
        source_start: usize,
        occurrence: Option<OccurrenceId>,
    },
    Synthetic,
}

/// Where one segment of analysed text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentSource {
    /// `occurrence` is the compile-time session's occurrence; `None` outside
    /// a session.
    Physical {
        occurrence: Option<OccurrenceId>,
        source_start: usize,
    },
    Synthetic,
}

impl ExpandedSource {
    pub(crate) fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        visit(self.text.len())?;
        if let Some(session) = &self.session {
            visit(session.budget.budget_snapshot().retained_bytes)?;
        }
        if let Some(analysis) = &self.conditionals {
            visit(analysis.projected_source.len())?;
            visit(std::mem::size_of_val(analysis.inactive_spans.as_slice()))?;
            visit(std::mem::size_of_val(analysis.unknown_spans.as_slice()))?;
            visit(std::mem::size_of_val(analysis.directives.as_slice()))?;
            for directive in &analysis.directives {
                visit(directive.body.len())?;
            }
        }
        visit(std::mem::size_of_val(self.observations.as_slice()))?;
        for observation in &self.observations {
            visit(observation.path.as_os_str().len())?;
        }
        for segment in &self.map.segments {
            let uri_bytes = match &segment.mapping {
                SegmentMapping::Physical { uri, .. } => uri.as_str().len(),
                SegmentMapping::Synthetic => 0,
            };
            visit(uri_bytes)?;
        }
        Ok(())
    }

    fn new() -> Self {
        Self::default()
    }

    fn push_physical(&mut self, uri: Url, source: &str) -> Range<usize> {
        self.push_physical_range(uri, source, 0, None)
    }

    pub(crate) fn push_physical_range(
        &mut self,
        uri: Url,
        source: &str,
        source_start: usize,
        occurrence: Option<OccurrenceId>,
    ) -> Range<usize> {
        let range = self.push_text(source);
        if !range.is_empty() {
            self.map.segments.push(Segment {
                virtual_range: range.clone(),
                mapping: SegmentMapping::Physical {
                    uri,
                    source_start,
                    occurrence,
                },
            });
        }
        range
    }

    pub(crate) fn push_synthetic(&mut self, source: &str) -> Range<usize> {
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
                SegmentMapping::Physical {
                    uri, source_start, ..
                } => PhysicalSpan {
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
                    ..
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

    pub(crate) fn segment_count(&self) -> usize {
        self.map.segments.len()
    }

    /// The source map in analysed order.
    pub(crate) fn segments(&self) -> impl Iterator<Item = (Range<usize>, SegmentSource)> + '_ {
        self.map.segments.iter().map(|segment| {
            let source = match &segment.mapping {
                SegmentMapping::Physical {
                    source_start,
                    occurrence,
                    ..
                } => SegmentSource::Physical {
                    occurrence: *occurrence,
                    source_start: *source_start,
                },
                SegmentMapping::Synthetic => SegmentSource::Synthetic,
            };
            (segment.virtual_range.clone(), source)
        })
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
    fn compile_time_include(
        &mut self,
        _request: pascal_core::resolver::IncludeResolveRequest<'_>,
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        _cancel: &dyn pascal_core::resolver::CancellationToken,
    ) -> Result<
        pascal_core::resolver::AdmittedCapabilityOutcome<pascal_core::resolver::LoadedSource>,
        pascal_core::resolver::ResolverError,
    > {
        let reports = pascal_core::resolver::AdmittedCapabilityReports::copy(&[], &[], budget)?;
        Ok(pascal_core::resolver::AdmittedCapabilityOutcome::new(
            pascal_core::resolver::Resolution::Unavailable {
                reason: String::new(),
            },
            reports,
        ))
    }
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

#[derive(Debug, Clone)]
pub(crate) struct ExpansionResult {
    pub(crate) expanded: ExpandedSource,
    pub(crate) dependencies: Vec<ResolvedInclude>,
    pub(crate) complete: bool,
    pub(crate) errors: Vec<String>,
}

pub(crate) fn expand_source<R: IncludeResolver>(
    root_uri: Url,
    source: &str,
    defines: &[String],
    resolver: &mut R,
    limits: ExpansionLimits,
    cancel: &AtomicBool,
) -> Result<ExpansionResult, String> {
    let context = ConditionalContext::from_defines(defines);
    expand_source_with_context(root_uri, source, &context, resolver, limits, cancel)
}

pub(crate) fn expand_source_with_context<R: IncludeResolver>(
    root_uri: Url,
    source: &str,
    context: &ConditionalContext,
    resolver: &mut R,
    limits: ExpansionLimits,
    cancel: &AtomicBool,
) -> Result<ExpansionResult, String> {
    expand_shared_source(root_uri, source, context, resolver, limits, cancel)
}

struct SharedIncludes<'a, R> {
    resolver: &'a mut R,
    limits: ExpansionLimits,
    sources: usize,
}

impl<R: IncludeResolver> pascal_core::compile_time::CompileTimeResolver for SharedIncludes<'_, R> {
    fn include_capability(
        &mut self,
        request: pascal_core::resolver::IncludeResolveRequest<'_>,
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn pascal_core::resolver::CancellationToken,
    ) -> Result<
        pascal_core::resolver::AdmittedCapabilityOutcome<pascal_core::resolver::LoadedSource>,
        pascal_core::resolver::ResolverError,
    > {
        self.resolver.compile_time_include(request, budget, cancel)
    }
    fn max_include_depth(&self) -> usize {
        self.limits.max_depth
    }
    fn admit_include(
        &mut self,
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn pascal_core::resolver::CancellationToken,
    ) -> Result<bool, pascal_core::resolver::ResolverError> {
        if cancel.is_cancelled() {
            return Err(pascal_core::resolver::ResolverError::Cancelled);
        }
        if self.sources >= self.limits.max_sources {
            return Ok(false);
        }
        budget.charge(1, 0, 0).map_err(|_| {
            pascal_core::resolver::ResolverError::InvalidRequest("include admission limit".into())
        })?;
        self.sources += 1;
        Ok(true)
    }
    fn include(
        &mut self,
        _request: pascal_core::resolver::IncludeResolveRequest<'_>,
        _budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        _cancel: &dyn pascal_core::resolver::CancellationToken,
    ) -> Result<
        pascal_core::resolver::ResolutionOutcome<pascal_core::resolver::LoadedSource>,
        pascal_core::resolver::ResolverError,
    > {
        Err(pascal_core::resolver::ResolverError::InvalidRequest(
            "legacy include callback is not a compile-time capability".into(),
        ))
    }
    fn unit(
        &mut self,
        _request: pascal_core::resolver::UnitResolveRequest<'_>,
        _budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        _cancel: &dyn pascal_core::resolver::CancellationToken,
    ) -> Result<
        pascal_core::resolver::ResolutionOutcome<pascal_core::compile_time::ProviderSource>,
        pascal_core::resolver::ResolverError,
    > {
        Err(pascal_core::resolver::ResolverError::InvalidRequest(
            "include adapter has no unit capability".into(),
        ))
    }
}

fn expand_shared_source<R: IncludeResolver>(
    root_uri: Url,
    source: &str,
    context: &ConditionalContext,
    resolver: &mut R,
    limits: ExpansionLimits,
    cancel: &AtomicBool,
) -> Result<ExpansionResult, String> {
    use pascal_core::resolver::{LoadedSource, SourceId, SourceRevision};
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("request cancelled".into());
    }
    if source.len() > limits.max_work {
        return Err("include expansion work limit reached".into());
    }
    if source.len() > limits.max_expanded_bytes {
        return Err("expanded include byte limit reached".into());
    }
    let directives = conditional::analyze_with_cancel(source, &[], cancel).directives;
    let admission = if directives.len() > limits.max_directives {
        Some("include directive limit reached")
    } else if directives.iter().any(|d| d.kind == DirectiveKind::Include) && limits.max_sources <= 1
    {
        Some("include source limit reached")
    } else if directives.iter().any(|d| d.kind == DirectiveKind::Include) && limits.max_depth == 0 {
        Some("include expansion depth limit reached")
    } else {
        None
    };
    if let Some(error) = admission {
        return Ok(ExpansionResult {
            expanded: ExpandedSource::default(),
            dependencies: vec![],
            complete: false,
            errors: vec![error.into()],
        });
    }
    let path = root_uri
        .to_file_path()
        .map_err(|_| "include root is not a file URI")?;
    let root = LoadedSource {
        id: SourceId::new(format!("source:{}", path.display())),
        path,
        bytes: std::sync::Arc::from(source.as_bytes()),
        decoded_text: Some(std::sync::Arc::from(source)),
        revision: SourceRevision::Overlay {
            version: 0,
            content_hash: pascal_project::content_hash_bytes(source.as_bytes()),
        },
        compile_time_backing: None,
    };
    let mut resolver = SharedIncludes {
        resolver,
        limits,
        sources: 1,
    };
    let analysis = pascal_core::compile_time::analyze_source(
        &root,
        context,
        &mut resolver,
        pascal_core::compile_time::CompileTimeLimits {
            max_retained_bytes: crate::workspace::compile_time::lsp_compatibility_retention(
                crate::workspace::ResourceLimits {
                    max_file_bytes: limits.max_expanded_bytes,
                    max_total_bytes: limits.max_expanded_bytes,
                    max_cache_bytes: 0,
                    ..Default::default()
                },
                limits,
            ),
            ..Default::default()
        },
        cancel,
    )
    .map_err(|e| e.to_string())?;
    crate::workspace::compile_time::mapped(std::sync::Arc::new(analysis), limits, cancel)
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

    /// A file URI for a fixture path written Unix-style; Windows needs a drive.
    fn uri(path: &str) -> Url {
        let path = if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.to_string()
        };
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
        expanded.push_physical_range(root.clone(), "tail", 4, None);

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
        fn compile_time_include(
            &mut self,
            request: pascal_core::resolver::IncludeResolveRequest<'_>,
            budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
            cancel: &dyn pascal_core::resolver::CancellationToken,
        ) -> Result<
            pascal_core::resolver::AdmittedCapabilityOutcome<pascal_core::resolver::LoadedSource>,
            pascal_core::resolver::ResolverError,
        > {
            let bytes = self
                .sources
                .get(request.requested_name)
                .map_or(0, String::len);
            fixture_capability(self, request, bytes, 0, budget, cancel)
        }

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

    fn fixture_capability<R: IncludeResolver>(
        resolver: &mut R,
        request: pascal_core::resolver::IncludeResolveRequest<'_>,
        text_bytes: usize,
        provenance_bytes: usize,
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn pascal_core::resolver::CancellationToken,
    ) -> Result<
        pascal_core::resolver::AdmittedCapabilityOutcome<pascal_core::resolver::LoadedSource>,
        pascal_core::resolver::ResolverError,
    > {
        use pascal_core::resolver::{
            AdmittedCapabilityOutcome, AdmittedCapabilityReports, LoadedSource, Resolution,
            SourceId, SourceRevision, SourceStoreError,
        };
        if cancel.is_cancelled() {
            return Err(pascal_core::resolver::ResolverError::Cancelled);
        }
        // The fixture's source path and UTF-8 payload bounds are known by
        // borrow; neither the legacy callback nor its copies run pre-admission.
        let path_bytes = "/workspace/".len() + request.requested_name.len();
        let metadata = std::mem::size_of::<LoadedSource>()
            + 4 * path_bytes
            + "source:".len()
            + provenance_bytes;
        let source =
            LoadedSource::admit_construction(budget, text_bytes, text_bytes * 4 + metadata, || {
                let owner = Url::from_file_path(request.including_path)
                    .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
                let resolved = resolver
                    .resolve_include(
                        &owner,
                        &format!("I {}", request.requested_name),
                        &AtomicBool::new(false),
                    )
                    .map_err(|reason| SourceStoreError::Io {
                        path: request.including_path.into(),
                        message: reason,
                    })?;
                let path = resolved
                    .uri
                    .to_file_path()
                    .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
                let hash = pascal_project::content_hash_bytes(resolved.text.as_bytes());
                let revision = if let Some(path_entry) = resolved.path_entry {
                    SourceRevision::Disk {
                        stamp: pascal_project::ProjectReadStamp {
                            bytes: text_bytes as u64,
                            modified: None,
                            is_dir: false,
                            is_symlink: false,
                            change: None,
                            settled: true,
                        },
                        content_hash: hash,
                        read_policy: Default::default(),
                        path_entry,
                    }
                } else {
                    SourceRevision::Overlay {
                        version: 0,
                        content_hash: hash,
                    }
                };
                Ok(LoadedSource {
                    id: SourceId::new(format!("source:{}", path.display())),
                    path,
                    bytes: std::sync::Arc::from(resolved.text.as_bytes()),
                    decoded_text: Some(std::sync::Arc::from(resolved.text)),
                    revision,
                    compile_time_backing: None,
                })
            })
            .map_err(|e| pascal_core::resolver::ResolverError::InvalidRequest(format!("{e:?}")))?;
        Ok(AdmittedCapabilityOutcome::new(
            Resolution::Found(source),
            AdmittedCapabilityReports::copy(&[], &[], budget)?,
        ))
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
            fn compile_time_include(
                &mut self,
                request: pascal_core::resolver::IncludeResolveRequest<'_>,
                budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
                cancel: &dyn pascal_core::resolver::CancellationToken,
            ) -> Result<
                pascal_core::resolver::AdmittedCapabilityOutcome<
                    pascal_core::resolver::LoadedSource,
                >,
                pascal_core::resolver::ResolverError,
            > {
                fixture_capability(
                    self,
                    request,
                    "{$DEFINE SAFE}\n".len(),
                    "/mapped/Shared.inc".len()
                        + "/mapped".len()
                        + std::mem::size_of::<pascal_project::ProjectPathEntry>()
                        + std::mem::size_of::<pascal_project::ReadPolicy>(),
                    budget,
                    cancel,
                )
            }
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
