//! Strict, immutable prepared-source inputs for project-aware CFG builds.
//!
//! Preparation is a pure, bounded operation over caller-owned source snapshots
//! and occurrence-specific include selections.  The lower-level constructors
//! also let a caller supply already projected bytes and a validated
//! [`SourceMap`].  This layer records the caller's configuration/provenance
//! assertion, parses with this crate's Pascal language, and refuses unresolved
//! or lossy input before it can be treated as a precise project unit.

use std::{borrow::Borrow, collections::HashMap, fmt, ops::Range, sync::Arc};

use tree_sitter::{Parser, Tree};

use crate::{
    source_map::{
        ExpansionId, MappedSourceSpan, SourceMap, SourceMapError, SourceMapSegment, SourceSnapshot,
    },
    ProjectSourceId,
};

/// Caller-supplied fidelity assertion for a prepared projection.
///
/// `Complete` means that the caller explicitly asserts that all executable
/// content relevant to the projection is represented.  It is not inferred
/// from the absence of preprocessor nodes and is not a proof that conditional
/// expressions were semantically evaluated correctly.  `Unresolved` and
/// `Lossy` are retained as explicit states so incomplete work cannot be
/// accidentally promoted to a precise [`PreparedSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreparationFidelity {
    /// The caller explicitly asserts a complete executable projection.
    Complete,
    /// Some active content or configuration is unresolved.
    Unresolved,
    /// Content was dropped or changed without a complete source contract.
    Lossy,
    /// The projection is known to be incomplete but is not classified more
    /// specifically.  This is also rejected by strict preparation.
    Incomplete,
}

impl PreparationFidelity {
    /// Alias emphasizing that `Complete` is an explicit caller assertion.
    pub const EXPLICIT_COMPLETE: Self = Self::Complete;

    /// Whether this fidelity is accepted by [`PreparedSource::new`].
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Broad origin of a prepared source.
///
/// This is metadata only.  `Configured` does not claim compiler-complete
/// semantics; the caller's [`PreparationFidelity`] is the separate explicit
/// completeness assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreparationProvenance {
    /// The prepared bytes are an exact raw identity projection.
    Raw,
    /// The prepared bytes came from an explicitly selected configuration or
    /// include projection.
    Configured,
}

impl PreparationProvenance {
    /// Alias for the raw identity provenance spelling.
    pub const RAW_IDENTITY: Self = Self::Raw;
}

/// Errors raised while constructing a strict [`PreparedSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedSourceError {
    /// The prepared source identity was empty.
    EmptySourceId(ProjectSourceId),
    /// The configuration identity was empty.
    EmptyConfigurationId,
    /// The caller did not assert complete prepared content.
    RejectedFidelity(PreparationFidelity),
    /// The map was validated against bytes with a different length.
    PreparedLengthMismatch { map_len: usize, prepared_len: usize },
    /// The map was validated against different bytes of the same length.
    PreparedBytesMismatch,
    /// The source map was invalid.
    SourceMap(SourceMapError),
    /// The parser returned no tree.
    ParserReturnedNoTree,
    /// The prepared bytes produced a tree containing parser errors.
    ParserErrors { range: Range<usize> },
    /// A strict prepared-source constructor found an unresolved preprocessor
    /// node.  Callers that have resolved the directive must provide the
    /// resulting prepared bytes and source map instead, or retain the source
    /// as a raw [`ProjectUnitInput`](crate::ProjectUnitInput).
    UnresolvedPreprocessor {
        /// Byte range of the first preprocessor node.
        range: Range<usize>,
        /// Tree-sitter kind of the preprocessor node.
        node_kind: String,
    },
}

impl fmt::Display for PreparedSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySourceId(id) => {
                write!(formatter, "prepared source ID {:?} must not be empty", id)
            }
            Self::EmptyConfigurationId => {
                formatter.write_str("prepared source configuration ID must not be empty")
            }
            Self::RejectedFidelity(fidelity) => write!(
                formatter,
                "prepared source requires an explicit complete projection, got {fidelity:?}"
            ),
            Self::PreparedLengthMismatch {
                map_len,
                prepared_len,
            } => write!(
                formatter,
                "source map prepared length {} does not match prepared length {}",
                map_len, prepared_len
            ),
            Self::PreparedBytesMismatch => formatter.write_str(
                "source map was validated against different prepared bytes of the same length",
            ),
            Self::SourceMap(error) => write!(formatter, "invalid prepared source map: {error}"),
            Self::ParserReturnedNoTree => formatter.write_str("Pascal parser returned no tree"),
            Self::ParserErrors { range } => {
                write!(
                    formatter,
                    "prepared Pascal source contains parser errors in {range:?}"
                )
            }
            Self::UnresolvedPreprocessor { range, node_kind } => write!(
                formatter,
                "strict prepared source cannot claim completeness for {node_kind:?} in {range:?}"
            ),
        }
    }
}

impl std::error::Error for PreparedSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SourceMap(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SourceMapError> for PreparedSourceError {
    fn from(error: SourceMapError) -> Self {
        Self::SourceMap(error)
    }
}

/// An immutable, parse-clean source projection with its original-source map.
///
/// The `source_id` identifies the exact prepared buffer consumed by the CFG
/// builder.  Original files and include occurrences are available through
/// [`Self::source_map`] and [`Self::original_sources`].  The parsed tree and
/// bytes are created and owned together, so a project snapshot cannot observe
/// a later caller mutation.
#[derive(Debug, Clone)]
pub struct PreparedSource {
    source_id: ProjectSourceId,
    bytes: Arc<[u8]>,
    tree: Tree,
    source_map: SourceMap,
    configuration_id: String,
    fidelity: PreparationFidelity,
    provenance: PreparationProvenance,
}

impl PreparedSource {
    /// Parse and validate a prepared source projection.
    ///
    /// The parser always uses [`crate::LANGUAGE`].  A source map must already
    /// be validated against the exact same prepared bytes; both its length
    /// and bytes are checked again here.  Only [`PreparationFidelity::Complete`]
    /// is accepted, and the bytes must contain no preprocessor nodes, so
    /// missing or lossy executable content cannot become an apparently empty
    /// include or a precise CFG by accident.  `Complete` is an explicit
    /// caller assertion, not permission to leave an unresolved include or
    /// other preprocessor directive in the prepared bytes.
    ///
    /// ```rust
    /// use cfg_pascal::{
    ///     PreparationFidelity, PreparationProvenance, PreparedSource,
    ///     ProjectSnapshot, ProjectSourceId, ProjectUnitId, ProjectUnitInput,
    ///     SourceMap, SourceSnapshot,
    /// };
    ///
    /// let bytes = b"unit Demo; interface implementation end.";
    /// let map = SourceMap::identity(SourceSnapshot::new(
    ///     ProjectSourceId::from("demo.pas"),
    ///     bytes,
    /// ))?;
    /// let prepared = PreparedSource::new(
    ///     ProjectSourceId::from("demo.prepared"),
    ///     bytes,
    ///     map,
    ///     "debug",
    ///     PreparationFidelity::Complete,
    ///     PreparationProvenance::Configured,
    /// )?;
    /// let unit = ProjectUnitInput::from_prepared(ProjectUnitId::from("demo"), prepared);
    /// let snapshot = ProjectSnapshot::new(vec![unit], Vec::new())?;
    /// assert_eq!(snapshot.configuration_id(), Some("debug"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new(
        source_id: ProjectSourceId,
        prepared_bytes: impl AsRef<[u8]>,
        source_map: SourceMap,
        configuration_id: impl Into<String>,
        fidelity: PreparationFidelity,
        provenance: PreparationProvenance,
    ) -> Result<Self, PreparedSourceError> {
        let prepared_bytes = prepared_bytes.as_ref();
        if source_id.as_str().is_empty() {
            return Err(PreparedSourceError::EmptySourceId(source_id));
        }

        let configuration_id = configuration_id.into();
        if configuration_id.is_empty() {
            return Err(PreparedSourceError::EmptyConfigurationId);
        }
        if !fidelity.is_complete() {
            return Err(PreparedSourceError::RejectedFidelity(fidelity));
        }
        if source_map.prepared_len() != prepared_bytes.len() {
            return Err(PreparedSourceError::PreparedLengthMismatch {
                map_len: source_map.prepared_len(),
                prepared_len: prepared_bytes.len(),
            });
        }
        if source_map.prepared_bytes() != prepared_bytes {
            return Err(PreparedSourceError::PreparedBytesMismatch);
        }

        let tree = parse_strict_clean(prepared_bytes)?;

        Ok(Self {
            source_id,
            bytes: source_map.prepared_bytes_arc(),
            tree,
            source_map,
            configuration_id,
            fidelity,
            provenance,
        })
    }

    /// Build a prepared source directly from original snapshots and ordered
    /// segments.  This is the intended seam for a future pure configurator.
    /// It has the same strict parser and preprocessor-node checks as
    /// [`Self::new`].
    pub fn from_segments(
        source_id: ProjectSourceId,
        prepared_bytes: impl AsRef<[u8]>,
        original_sources: Vec<SourceSnapshot>,
        segments: Vec<crate::SourceMapSegment>,
        configuration_id: impl Into<String>,
        fidelity: PreparationFidelity,
        provenance: PreparationProvenance,
    ) -> Result<Self, PreparedSourceError> {
        let source_map = SourceMap::new(&prepared_bytes, original_sources, segments)?;
        Self::new(
            source_id,
            prepared_bytes,
            source_map,
            configuration_id,
            fidelity,
            provenance,
        )
    }

    /// Construct a complete raw identity projection.
    ///
    /// This convenience path is valid only for a parse-clean source without
    /// any preprocessor nodes.  An unresolved include or conditional directive
    /// is rejected rather than being silently treated as complete.  Callers
    /// with a resolved configuration should use [`Self::new`] and provide the
    /// prepared bytes and source map explicitly; the same rule is enforced by
    /// every strict prepared-source constructor.
    pub fn identity(
        source_id: ProjectSourceId,
        bytes: impl AsRef<[u8]>,
        configuration_id: impl Into<String>,
    ) -> Result<Self, PreparedSourceError> {
        let snapshot = SourceSnapshot::new(source_id.clone(), bytes.as_ref());
        let source_map = SourceMap::identity(snapshot)?;
        Self::new(
            source_id,
            bytes,
            source_map,
            configuration_id,
            PreparationFidelity::Complete,
            PreparationProvenance::Raw,
        )
    }

    /// Stable identity of the prepared byte snapshot.
    pub fn source_id(&self) -> &ProjectSourceId {
        &self.source_id
    }

    /// Borrow the exact parsed prepared bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrow the parse-clean tree built from [`Self::bytes`].
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Borrow the validated prepared-to-original source map.
    pub fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    /// Borrow the original source snapshots retained by the map.
    pub fn original_sources(&self) -> &[SourceSnapshot] {
        self.source_map.original_sources()
    }

    /// Configuration identity supplied by the project model.
    pub fn configuration_id(&self) -> &str {
        &self.configuration_id
    }

    /// Explicit completeness assertion retained with this source.
    pub fn fidelity(&self) -> PreparationFidelity {
        self.fidelity
    }

    /// Preparation provenance retained with this source.
    pub fn provenance(&self) -> PreparationProvenance {
        self.provenance
    }

    /// Map a prepared range through the retained source map.
    pub fn map_range(
        &self,
        prepared_range: Range<usize>,
    ) -> Result<Vec<MappedSourceSpan>, SourceMapError> {
        self.source_map.map_range(prepared_range)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ProjectSourceId,
        Tree,
        Arc<[u8]>,
        SourceMap,
        String,
        PreparationFidelity,
        PreparationProvenance,
    ) {
        (
            self.source_id,
            self.tree,
            self.bytes,
            self.source_map,
            self.configuration_id,
            self.fidelity,
            self.provenance,
        )
    }
}

fn parse_strict_clean(bytes: &[u8]) -> Result<Tree, PreparedSourceError> {
    let mut parser = Parser::new();
    parser
        .set_language(&crate::LANGUAGE.into())
        .map_err(|_| PreparedSourceError::ParserReturnedNoTree)?;
    let tree = parser
        .parse(bytes, None)
        .ok_or(PreparedSourceError::ParserReturnedNoTree)?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(PreparedSourceError::ParserErrors {
            range: root.start_byte()..root.end_byte(),
        });
    }
    if let Some((range, node_kind)) = first_preprocessor_node(root, bytes) {
        return Err(PreparedSourceError::UnresolvedPreprocessor { range, node_kind });
    }
    Ok(tree)
}

fn first_preprocessor_node(
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(Range<usize>, String)> {
    if is_preprocessor_node(root, source) {
        return Some((root.start_byte()..root.end_byte(), root.kind().to_string()));
    }

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if let Some(found) = first_preprocessor_node(child, source) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn is_preprocessor_node(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    is_preprocessor_kind(node.kind())
        || (node.kind() == "comment"
            && source
                .get(node.byte_range())
                .is_some_and(|bytes| bytes.starts_with(b"(*$")))
}

pub(crate) fn is_preprocessor_kind(kind: &str) -> bool {
    kind.starts_with("pp")
}

/// Whether symbols not explicitly supplied to a preparation call are known
/// to be absent or remain unknown.
///
/// This is deliberately an environment property rather than an inference
/// from the loaded sources.  In a [`PreparationEnvironment::Partial`] project
/// an absent symbol is unknown, not false; callers that know a symbol is
/// absent should put it in [`PrepareSourceOptions::initial_undefined_symbols`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreparationEnvironment {
    /// Symbols omitted from the explicit initial sets are known to be absent.
    Complete,
    /// Symbols omitted from the explicit initial sets may be provided by an
    /// environment that was not loaded into this preparation call.
    Partial,
}

/// Alias for callers that prefer to name the complete/partial distinction
/// explicitly.
pub type EnvironmentCompleteness = PreparationEnvironment;

/// Resource limits for pure configured source preparation.
///
/// Limits are checked before growth or recursion.  They are intentionally
/// byte/token counters rather than timeouts so a preparation has deterministic
/// failure behavior independent of the host running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparationLimits {
    /// Total bytes in the caller-supplied source snapshot list.
    pub max_source_bytes: usize,
    /// Bytes in the expanded prepared buffer.
    pub max_output_bytes: usize,
    /// Number of active include occurrences expanded, including empty files.
    pub max_expanded_occurrences: usize,
    /// Maximum number of nested include frames below the root source.
    pub max_include_depth: usize,
    /// Maximum nesting depth of conditional groups in one source frame.
    pub max_conditional_depth: usize,
    /// Maximum number of directives processed across expanded occurrences.
    pub max_directives: usize,
    /// Maximum condition-expression bytes processed across expanded
    /// occurrences.
    pub max_expression_bytes: usize,
    /// Maximum condition-expression tokens processed across expanded
    /// occurrences.
    pub max_expression_tokens: usize,
    /// Maximum parser/expansion work units.  Work is charged for lexing and
    /// for bytes visited while producing each occurrence.
    pub max_work: usize,
    /// Maximum recursive-descent expression depth.  This separate bound keeps
    /// malformed or adversarial parentheses from overflowing the Rust stack.
    pub max_expression_depth: usize,
}

impl Default for PreparationLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 16 * 1024 * 1024,
            max_output_bytes: 64 * 1024 * 1024,
            max_expanded_occurrences: 10_000,
            max_include_depth: 64,
            max_conditional_depth: 256,
            max_directives: 100_000,
            max_expression_bytes: 1024 * 1024,
            max_expression_tokens: 1_000_000,
            max_work: 100_000_000,
            max_expression_depth: 128,
        }
    }
}

/// Options controlling one pure configured-source preparation call.
///
/// The source IDs, initial symbol sets, and configuration identity are all
/// caller-owned inputs.  The preparation call clones only the small metadata
/// needed for its result; the source snapshots themselves remain immutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareSourceOptions {
    /// Identity assigned to the newly prepared byte buffer.
    pub prepared_source_id: ProjectSourceId,
    /// Configuration identity retained by [`PreparedSource`].
    pub configuration_id: String,
    /// Whether an omitted symbol is false or unknown.
    pub environment: PreparationEnvironment,
    /// Symbols known to be defined before the root source is processed.
    pub initial_defined_symbols: Vec<String>,
    /// Symbols known to be undefined before the root source is processed.
    pub initial_undefined_symbols: Vec<String>,
    /// Bounds for this call.  The value is copied, so callers can reuse and
    /// mutate their own options after a call without changing a result.
    pub limits: PreparationLimits,
}

impl PrepareSourceOptions {
    /// Create options with empty symbol sets and default resource limits.
    pub fn new(
        prepared_source_id: ProjectSourceId,
        configuration_id: impl Into<String>,
        environment: PreparationEnvironment,
    ) -> Self {
        Self {
            prepared_source_id,
            configuration_id: configuration_id.into(),
            environment,
            initial_defined_symbols: Vec::new(),
            initial_undefined_symbols: Vec::new(),
            limits: PreparationLimits::default(),
        }
    }

    /// Add the symbols that are initially known to be defined.
    pub fn with_initial_defined_symbols<I, S>(mut self, symbols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.initial_defined_symbols = symbols.into_iter().map(Into::into).collect();
        self
    }

    /// Add the symbols that are initially known to be undefined.
    pub fn with_initial_undefined_symbols<I, S>(mut self, symbols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.initial_undefined_symbols = symbols.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the resource limits for this call.
    pub fn with_limits(mut self, limits: PreparationLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Shorter spelling for [`PrepareSourceOptions`].
pub type PreparationOptions = PrepareSourceOptions;

/// An occurrence-specific include selection made by the caller.
///
/// The key is the including source identity plus the exact byte range of the
/// original directive.  The preparer never interprets a filename as a path or
/// searches for a basename.  Repeated occurrences therefore get independent
/// expansion identities even when they select the same target snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IncludeBinding {
    /// Source containing the include directive.
    pub including_source_id: ProjectSourceId,
    /// Exact half-open byte range of the original `{$I ...}` or
    /// `(*$INCLUDE ...*)` directive.
    pub directive_range: Range<usize>,
    /// Source snapshot selected for this occurrence.
    pub target_source_id: ProjectSourceId,
}

impl IncludeBinding {
    /// Construct an occurrence-specific include selection.
    pub fn new(
        including_source_id: ProjectSourceId,
        directive_range: Range<usize>,
        target_source_id: ProjectSourceId,
    ) -> Self {
        Self {
            including_source_id,
            directive_range,
            target_source_id,
        }
    }

    /// Source containing this binding's directive.
    pub fn including_source_id(&self) -> &ProjectSourceId {
        &self.including_source_id
    }

    /// Exact original directive range used as the binding key.
    pub fn directive_range(&self) -> Range<usize> {
        self.directive_range.clone()
    }

    /// Selected target source identity.
    pub fn target_source_id(&self) -> &ProjectSourceId {
        &self.target_source_id
    }
}

/// Alias emphasizing that an [`IncludeBinding`] is already resolved by the
/// caller rather than discovered by this crate.
pub type ResolvedIncludeBinding = IncludeBinding;

/// Categories of deterministic preparation budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreparationBudget {
    /// Total supplied source snapshot bytes.
    SourceBytes,
    /// Expanded output bytes.
    OutputBytes,
    /// Active include occurrences.
    ExpandedOccurrences,
    /// Include nesting depth.
    IncludeDepth,
    /// Conditional nesting depth.
    ConditionalDepth,
    /// Directives processed.
    Directives,
    /// Condition-expression bytes.
    ExpressionBytes,
    /// Condition-expression tokens.
    ExpressionTokens,
    /// Aggregate work units.
    Work,
    /// Recursive expression depth.
    ExpressionDepth,
}

/// Rich failures returned by [`prepare_source`].
///
/// Directive-related variants retain the immutable source identity and exact
/// original byte range that caused the failure.  The error is intentionally
/// strict: callers must select include targets and provide enough environment
/// information instead of receiving a silently incomplete `PreparedSource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareSourceError {
    /// The requested root source identity was empty.
    EmptyRootSourceId(ProjectSourceId),
    /// The root source was not present in the supplied snapshot list.
    RootSourceNotLoaded(ProjectSourceId),
    /// A source identity appeared more than once in the snapshot list.
    DuplicateSourceId(ProjectSourceId),
    /// An initial symbol name is not a Pascal preprocessor identifier.
    InvalidSymbolName { symbol: String },
    /// A symbol was supplied in both initial sets.
    ConflictingInitialSymbol { symbol: String },
    /// A binding's including source was not supplied.
    IncludeBindingSourceNotLoaded { source_id: ProjectSourceId },
    /// A binding range is outside its including source.
    IncludeBindingRangeOutOfBounds {
        source_id: ProjectSourceId,
        range: Range<usize>,
        source_len: usize,
    },
    /// A binding range is not exactly one supported include directive.
    IncludeBindingNotIncludeDirective {
        source_id: ProjectSourceId,
        range: Range<usize>,
    },
    /// The same including-source/range key was supplied twice.
    DuplicateIncludeBinding {
        source_id: ProjectSourceId,
        range: Range<usize>,
    },
    /// A binding selected a source absent from the snapshot list.
    IncludeTargetNotLoaded {
        source_id: ProjectSourceId,
        range: Range<usize>,
        target_source_id: ProjectSourceId,
    },
    /// An active include has no caller-selected target.
    UnresolvedInclude {
        source_id: ProjectSourceId,
        range: Range<usize>,
        requested: String,
    },
    /// An active include would recurse through an already active source.
    IncludeCycle {
        source_id: ProjectSourceId,
        range: Range<usize>,
        cycle: Vec<ProjectSourceId>,
    },
    /// A limit was reached before doing the operation that would exceed it.
    BudgetExceeded {
        budget: PreparationBudget,
        source_id: ProjectSourceId,
        range: Range<usize>,
        limit: usize,
        observed: usize,
    },
    /// An active condition depends on an omitted symbol in a partial
    /// environment.
    UnknownActiveCondition {
        source_id: ProjectSourceId,
        range: Range<usize>,
        expression: String,
    },
    /// A condition or directive argument is malformed.
    InvalidDirective {
        source_id: ProjectSourceId,
        range: Range<usize>,
        directive: String,
        message: String,
    },
    /// Conditional directives were not properly nested.
    MalformedNesting {
        source_id: ProjectSourceId,
        range: Range<usize>,
        directive: String,
    },
    /// An active directive has side effects outside the supported subset.
    UnsupportedDirective {
        source_id: ProjectSourceId,
        range: Range<usize>,
        directive: String,
    },
    /// The reachable projection contains no active source content.
    NoCompleteContent { root_source_id: ProjectSourceId },
    /// The output map could not be validated.
    SourceMap(SourceMapError),
    /// The expanded buffer could not become a strict prepared source.
    PreparedSource(PreparedSourceError),
}

impl PrepareSourceError {
    /// Return the source identity associated with a source-local failure.
    pub fn source_id(&self) -> Option<&ProjectSourceId> {
        match self {
            Self::IncludeBindingSourceNotLoaded { source_id }
            | Self::IncludeBindingRangeOutOfBounds { source_id, .. }
            | Self::IncludeBindingNotIncludeDirective { source_id, .. }
            | Self::DuplicateIncludeBinding { source_id, .. }
            | Self::IncludeTargetNotLoaded { source_id, .. }
            | Self::UnresolvedInclude { source_id, .. }
            | Self::IncludeCycle { source_id, .. }
            | Self::BudgetExceeded { source_id, .. }
            | Self::UnknownActiveCondition { source_id, .. }
            | Self::InvalidDirective { source_id, .. }
            | Self::MalformedNesting { source_id, .. }
            | Self::UnsupportedDirective { source_id, .. } => Some(source_id),
            Self::EmptyRootSourceId(_)
            | Self::RootSourceNotLoaded(_)
            | Self::DuplicateSourceId(_)
            | Self::NoCompleteContent { .. }
            | Self::InvalidSymbolName { .. }
            | Self::ConflictingInitialSymbol { .. }
            | Self::SourceMap(_)
            | Self::PreparedSource(_) => None,
        }
    }

    /// Return the original source range associated with a source-local
    /// failure, if one exists.
    pub fn range(&self) -> Option<Range<usize>> {
        match self {
            Self::IncludeBindingRangeOutOfBounds { range, .. }
            | Self::IncludeBindingNotIncludeDirective { range, .. }
            | Self::DuplicateIncludeBinding { range, .. }
            | Self::IncludeTargetNotLoaded { range, .. }
            | Self::UnresolvedInclude { range, .. }
            | Self::IncludeCycle { range, .. }
            | Self::BudgetExceeded { range, .. }
            | Self::UnknownActiveCondition { range, .. }
            | Self::InvalidDirective { range, .. }
            | Self::MalformedNesting { range, .. }
            | Self::UnsupportedDirective { range, .. } => Some(range.clone()),
            _ => None,
        }
    }
}

impl fmt::Display for PrepareSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRootSourceId(id) => write!(formatter, "root source ID {:?} is empty", id),
            Self::RootSourceNotLoaded(id) => {
                write!(formatter, "root source {:?} was not supplied", id)
            }
            Self::DuplicateSourceId(id) => {
                write!(formatter, "source snapshot ID {:?} was supplied twice", id)
            }
            Self::InvalidSymbolName { symbol } => {
                write!(formatter, "invalid preprocessor symbol {:?}", symbol)
            }
            Self::ConflictingInitialSymbol { symbol } => write!(
                formatter,
                "preprocessor symbol {:?} is both initially defined and undefined",
                symbol
            ),
            Self::IncludeBindingSourceNotLoaded { source_id } => write!(
                formatter,
                "include binding source {:?} was not supplied",
                source_id
            ),
            Self::IncludeBindingRangeOutOfBounds {
                source_id,
                range,
                source_len,
            } => write!(
                formatter,
                "include binding range {:?} in source {:?} exceeds source length {}",
                range, source_id, source_len
            ),
            Self::IncludeBindingNotIncludeDirective { source_id, range } => write!(
                formatter,
                "include binding range {:?} in source {:?} is not an include directive",
                range, source_id
            ),
            Self::DuplicateIncludeBinding { source_id, range } => write!(
                formatter,
                "include binding for source {:?} range {:?} was supplied twice",
                source_id, range
            ),
            Self::IncludeTargetNotLoaded {
                source_id,
                range,
                target_source_id,
            } => write!(
                formatter,
                "include at {:?} in source {:?} selects missing source {:?}",
                range, source_id, target_source_id
            ),
            Self::UnresolvedInclude {
                source_id,
                range,
                requested,
            } => write!(
                formatter,
                "include {:?} at {:?} in source {:?} has no selected target",
                requested, range, source_id
            ),
            Self::IncludeCycle {
                source_id,
                range,
                cycle,
            } => write!(
                formatter,
                "include at {:?} in source {:?} creates cycle {:?}",
                range, source_id, cycle
            ),
            Self::BudgetExceeded {
                budget,
                source_id,
                range,
                limit,
                observed,
            } => write!(
                formatter,
                "preparation budget {budget:?} exceeded at {:?} in source {:?}: {} > {}",
                range, source_id, observed, limit
            ),
            Self::UnknownActiveCondition {
                source_id,
                range,
                expression,
            } => write!(
                formatter,
                "active condition {:?} at {:?} in source {:?} is unknown",
                expression, range, source_id
            ),
            Self::InvalidDirective {
                source_id,
                range,
                directive,
                message,
            } => write!(
                formatter,
                "invalid directive {:?} at {:?} in source {:?}: {}",
                directive, range, source_id, message
            ),
            Self::MalformedNesting {
                source_id,
                range,
                directive,
            } => write!(
                formatter,
                "malformed conditional nesting at {:?} in source {:?}: {}",
                range, source_id, directive
            ),
            Self::UnsupportedDirective {
                source_id,
                range,
                directive,
            } => write!(
                formatter,
                "unsupported active directive {:?} at {:?} in source {:?}",
                directive, range, source_id
            ),
            Self::NoCompleteContent { root_source_id } => write!(
                formatter,
                "configured preparation rooted at {:?} produced no active source content",
                root_source_id
            ),
            Self::SourceMap(error) => write!(formatter, "invalid prepared source map: {error}"),
            Self::PreparedSource(error) => write!(formatter, "invalid prepared source: {error}"),
        }
    }
}

impl std::error::Error for PrepareSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SourceMap(error) => Some(error),
            Self::PreparedSource(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SourceMapError> for PrepareSourceError {
    fn from(error: SourceMapError) -> Self {
        Self::SourceMap(error)
    }
}

impl From<PreparedSourceError> for PrepareSourceError {
    fn from(error: PreparedSourceError) -> Self {
        Self::PreparedSource(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IncludeKey {
    source_id: ProjectSourceId,
    range: Range<usize>,
}

#[derive(Debug, Clone)]
struct RawDirective {
    range: Range<usize>,
    argument_range: Range<usize>,
    keyword_range: Range<usize>,
}

#[derive(Debug, Clone)]
enum LexItem {
    Text(Range<usize>),
    Directive(RawDirective),
}

#[derive(Debug, Clone)]
struct LexedSource {
    items: Vec<LexItem>,
    directives: HashMap<(usize, usize), RawDirective>,
}

struct SourceCatalog<'a> {
    sources: &'a [SourceSnapshot],
    indices: HashMap<ProjectSourceId, usize>,
    lexed: HashMap<ProjectSourceId, LexedSource>,
}

impl<'a> SourceCatalog<'a> {
    fn new(
        sources: &'a [SourceSnapshot],
        limits: PreparationLimits,
    ) -> Result<Self, PrepareSourceError> {
        let mut indices = HashMap::with_capacity(sources.len());
        let mut total = 0usize;
        for source in sources {
            total = total.checked_add(source.len()).ok_or_else(|| {
                PrepareSourceError::BudgetExceeded {
                    budget: PreparationBudget::SourceBytes,
                    source_id: source.source_id().clone(),
                    range: 0..source.len(),
                    limit: limits.max_source_bytes,
                    observed: usize::MAX,
                }
            })?;
            if total > limits.max_source_bytes {
                return Err(PrepareSourceError::BudgetExceeded {
                    budget: PreparationBudget::SourceBytes,
                    source_id: source.source_id().clone(),
                    range: 0..source.len(),
                    limit: limits.max_source_bytes,
                    observed: total,
                });
            }
            if indices
                .insert(source.source_id().clone(), indices.len())
                .is_some()
            {
                return Err(PrepareSourceError::DuplicateSourceId(
                    source.source_id().clone(),
                ));
            }
        }

        Ok(Self {
            sources,
            indices,
            lexed: HashMap::new(),
        })
    }

    fn source(&self, source_id: &ProjectSourceId) -> Option<&SourceSnapshot> {
        self.indices
            .get(source_id)
            .and_then(|index| self.sources.get(*index))
    }

    fn ensure_lexed(
        &mut self,
        source_id: &ProjectSourceId,
        limits: PreparationLimits,
        work: &mut usize,
    ) -> Result<(), PrepareSourceError> {
        if self.lexed.contains_key(source_id) {
            return Ok(());
        }
        let Some(source) = self.source(source_id) else {
            return Err(PrepareSourceError::RootSourceNotLoaded(source_id.clone()));
        };
        charge(
            work,
            PreparationBudget::Work,
            source.len(),
            limits.max_work,
            source_id,
            0..source.len(),
        )?;
        let lexed = lex_source(source_id, source.bytes(), limits)?;
        self.lexed.insert(source_id.clone(), lexed);
        Ok(())
    }

    fn lexed(&self, source_id: &ProjectSourceId) -> &LexedSource {
        self.lexed
            .get(source_id)
            .expect("source must be lexed before it is processed")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriState {
    True,
    False,
    Unknown,
}

struct SymbolEnvironment {
    values: HashMap<String, bool>,
    completeness: PreparationEnvironment,
}

impl SymbolEnvironment {
    fn new(options: &PrepareSourceOptions) -> Result<Self, PrepareSourceError> {
        let mut values = HashMap::new();
        for symbol in &options.initial_defined_symbols {
            let symbol =
                normalize_symbol(symbol).ok_or_else(|| PrepareSourceError::InvalidSymbolName {
                    symbol: symbol.clone(),
                })?;
            if values.insert(symbol.clone(), true).is_some_and(|old| !old) {
                return Err(PrepareSourceError::ConflictingInitialSymbol { symbol });
            }
        }
        for symbol in &options.initial_undefined_symbols {
            let symbol =
                normalize_symbol(symbol).ok_or_else(|| PrepareSourceError::InvalidSymbolName {
                    symbol: symbol.clone(),
                })?;
            if values.insert(symbol.clone(), false).is_some_and(|old| old) {
                return Err(PrepareSourceError::ConflictingInitialSymbol { symbol });
            }
        }
        Ok(Self {
            values,
            completeness: options.environment,
        })
    }

    fn state(&self, symbol: &str) -> TriState {
        match self.values.get(symbol).copied() {
            Some(true) => TriState::True,
            Some(false) => TriState::False,
            None => match self.completeness {
                PreparationEnvironment::Complete => TriState::False,
                PreparationEnvironment::Partial => TriState::Unknown,
            },
        }
    }

    fn set(&mut self, symbol: String, defined: bool) {
        self.values.insert(symbol, defined);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConditionExpr {
    operations: Vec<ConditionOp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConditionOp {
    Literal(bool),
    Defined(String),
    Not,
    And,
    Or,
}

impl ConditionExpr {
    fn literal(value: bool) -> Self {
        Self {
            operations: vec![ConditionOp::Literal(value)],
        }
    }

    fn defined(symbol: String) -> Self {
        Self {
            operations: vec![ConditionOp::Defined(symbol)],
        }
    }

    fn not(mut expression: Self) -> Self {
        expression.operations.push(ConditionOp::Not);
        expression
    }

    fn combine(mut left: Self, right: Self, operator: ConditionOp) -> Self {
        left.operations.extend(right.operations);
        left.operations.push(operator);
        left
    }

    fn evaluate(&self, symbols: &SymbolEnvironment) -> TriState {
        let mut values = Vec::with_capacity(self.operations.len());
        for operation in &self.operations {
            match operation {
                ConditionOp::Literal(value) => values.push(if *value {
                    TriState::True
                } else {
                    TriState::False
                }),
                ConditionOp::Defined(symbol) => values.push(symbols.state(symbol)),
                ConditionOp::Not => {
                    let value = values.pop().unwrap_or(TriState::Unknown);
                    values.push(match value {
                        TriState::True => TriState::False,
                        TriState::False => TriState::True,
                        TriState::Unknown => TriState::Unknown,
                    });
                }
                ConditionOp::And => {
                    let right = values.pop().unwrap_or(TriState::Unknown);
                    let left = values.pop().unwrap_or(TriState::Unknown);
                    values.push(match (left, right) {
                        (TriState::False, _) => TriState::False,
                        (TriState::True, right) => right,
                        (TriState::Unknown, TriState::False) => TriState::False,
                        (TriState::Unknown, TriState::True | TriState::Unknown) => {
                            TriState::Unknown
                        }
                    });
                }
                ConditionOp::Or => {
                    let right = values.pop().unwrap_or(TriState::Unknown);
                    let left = values.pop().unwrap_or(TriState::Unknown);
                    values.push(match (left, right) {
                        (TriState::True, _) => TriState::True,
                        (TriState::False, right) => right,
                        (TriState::Unknown, TriState::True) => TriState::True,
                        (TriState::Unknown, TriState::False | TriState::Unknown) => {
                            TriState::Unknown
                        }
                    });
                }
            }
        }
        values.pop().unwrap_or(TriState::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectiveCommand {
    If(ConditionExpr),
    ElseIf(ConditionExpr),
    Else,
    EndIf,
    Define(String),
    Undef(String),
    Include(String),
    Unsupported(String),
}

#[derive(Debug, Clone)]
struct ConditionalFrame {
    parent_active: bool,
    branch_taken: bool,
    current_active: bool,
    saw_else: bool,
    source_id: ProjectSourceId,
    range: Range<usize>,
}

#[derive(Debug)]
struct SourceFrame {
    source_id: ProjectSourceId,
    expansion_id: ExpansionId,
    item_index: usize,
    include_depth: usize,
    output_start: usize,
}

/// Prepare one root source using only immutable caller-supplied snapshots and
/// occurrence-specific include selections.
///
/// No filesystem, environment-variable, MSBuild, search-path, or project
/// manager access is performed.  A caller selects every include target by its
/// exact source/range key.  The supported strict subset is:
///
/// * `IFDEF` and `IFNDEF` with one symbol;
/// * `IF`/`ELSEIF`/`ELIF` expressions using `Defined(name)`, `NOT`, `AND`,
///   `OR`, parentheses, `TRUE`, and `FALSE`;
/// * `ELSE`, `ENDIF`/`IFEND`, source-order `DEFINE`/`UNDEF`, and `I`/`INCLUDE`;
/// * directive spellings in both `{$...}` and `(*$...*)` outside Pascal
///   strings, ordinary comments, and line comments.
///
/// Active directives are whitespace-masked while preserving `CR`/`LF` bytes.
/// Inactive source is masked the same way, and selected includes are expanded
/// in place with deterministic distinct [`ExpansionId`] values.  `DEFINE` and
/// `UNDEF` mutations are shared through nested include frames and remain local
/// to this call.  This subset is a bounded configuration projection, not a
/// claim of compiler-complete Pascal preprocessor semantics.
///
/// The caller owns include resolution.  For example, a project service can
/// select an include target once, then pass that decision for the exact
/// directive span without making this helper inspect the filesystem:
///
/// ```rust
/// use cfg_pascal::{
///     prepare_source, IncludeBinding, PreparationEnvironment, PrepareSourceOptions,
///     ProjectSourceId, SourceSnapshot,
/// };
///
/// let root_id = ProjectSourceId::from("demo.pas");
/// let included_id = ProjectSourceId::from("body.inc");
/// let root = b"program Demo;\nbegin\n{$IFDEF FEATURE}\n{$I body.inc}\n{$ENDIF}\nend.\n";
/// let included = b"  Writeln('configured');\n";
/// let directive = b"{$I body.inc}";
/// let start = root
///     .windows(directive.len())
///     .position(|window| window == directive)
///     .expect("include directive");
/// let options = PrepareSourceOptions::new(
///     ProjectSourceId::from("demo.prepared"),
///     "debug",
///     PreparationEnvironment::Complete,
/// )
/// .with_initial_defined_symbols(["FEATURE"]);
/// let prepared = prepare_source(
///     &root_id,
///     &[
///         SourceSnapshot::new(root_id.clone(), root),
///         SourceSnapshot::new(included_id.clone(), included),
///     ],
///     &[IncludeBinding::new(
///         root_id.clone(),
///         start..start + directive.len(),
///         included_id,
///     )],
///     options,
/// )?;
/// assert!(prepared.bytes().windows(b"configured".len()).any(|window| {
///     window == b"configured"
/// }));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn prepare_source<R>(
    root_source_id: R,
    loaded_sources: &[SourceSnapshot],
    include_bindings: &[IncludeBinding],
    options: PrepareSourceOptions,
) -> Result<PreparedSource, PrepareSourceError>
where
    R: Borrow<ProjectSourceId>,
{
    let root_source_id = root_source_id.borrow().clone();
    if root_source_id.as_str().is_empty() {
        return Err(PrepareSourceError::EmptyRootSourceId(root_source_id));
    }

    let limits = options.limits;
    let mut catalog = SourceCatalog::new(loaded_sources, limits)?;
    if catalog.source(&root_source_id).is_none() {
        return Err(PrepareSourceError::RootSourceNotLoaded(root_source_id));
    }

    let mut symbols = SymbolEnvironment::new(&options)?;
    let mut work = 0usize;
    catalog.ensure_lexed(&root_source_id, limits, &mut work)?;

    let bindings = validate_include_bindings(&mut catalog, include_bindings, limits, &mut work)?;
    let mut output = Vec::new();
    let mut segments = Vec::new();
    let mut usage = PreparationUsage {
        work,
        ..PreparationUsage::default()
    };
    let mut expanded_occurrences = 0usize;
    let mut active_content = false;
    let mut conditionals: Vec<ConditionalFrame> = Vec::new();
    let mut stack = vec![SourceFrame {
        source_id: root_source_id.clone(),
        expansion_id: ExpansionId::root(),
        item_index: 0,
        include_depth: 0,
        output_start: 0,
    }];
    let mut include_chain = vec![root_source_id.clone()];

    while !stack.is_empty() {
        let frame_index = stack.len() - 1;
        let source_id = stack[frame_index].source_id.clone();
        let item_count = catalog.lexed(&source_id).items.len();
        if stack[frame_index].item_index == item_count {
            let frame = stack.pop().expect("source frame exists");
            include_chain.pop();
            if frame.include_depth > 0
                && output.len() > frame.output_start
                && !matches!(output.last(), Some(b'\n'))
            {
                append_synthetic(
                    &mut output,
                    &mut segments,
                    &frame.source_id,
                    frame.expansion_id,
                    catalog
                        .source(&source_id)
                        .expect("completed frame source remains loaded")
                        .len(),
                    &mut usage,
                    limits,
                )?;
            }
            continue;
        }

        let item = catalog.lexed(&source_id).items[stack[frame_index].item_index].clone();
        stack[frame_index].item_index += 1;
        let expansion_id = stack[frame_index].expansion_id.clone();
        let source = catalog
            .source(&source_id)
            .expect("frame source must be loaded")
            .bytes();

        match item {
            LexItem::Text(range) => {
                let active = conditionals
                    .last()
                    .map(|frame| frame.current_active)
                    .unwrap_or(true);
                charge_work(&mut usage, range.len(), limits, &source_id, range.clone())?;
                if active {
                    active_content |= source[range.clone()]
                        .iter()
                        .any(|byte| !byte.is_ascii_whitespace());
                    append_copied(
                        &mut output,
                        &mut segments,
                        source_id.clone(),
                        range,
                        expansion_id,
                        source,
                        limits,
                    )?;
                } else {
                    append_masked(
                        &mut output,
                        &mut segments,
                        source_id.clone(),
                        range,
                        expansion_id,
                        source,
                        limits,
                    )?;
                }
            }
            LexItem::Directive(raw) => {
                charge_directive(&mut usage, limits, &source_id, raw.range.clone())?;
                let command = parse_directive(&raw, source, &source_id, &mut usage, limits)?;
                let frame_active = conditionals
                    .last()
                    .map(|frame| frame.current_active)
                    .unwrap_or(true);
                match command {
                    DirectiveCommand::If(condition) => {
                        let parent_active = frame_active;
                        let current_active = if parent_active {
                            require_condition(
                                condition.evaluate(&symbols),
                                &source_id,
                                raw.range.clone(),
                                source,
                            )?
                        } else {
                            false
                        };
                        let depth = conditionals.len().saturating_add(1);
                        check_depth(
                            PreparationBudget::ConditionalDepth,
                            depth,
                            limits.max_conditional_depth,
                            &source_id,
                            raw.range.clone(),
                        )?;
                        conditionals.push(ConditionalFrame {
                            parent_active,
                            branch_taken: current_active,
                            current_active,
                            saw_else: false,
                            source_id: source_id.clone(),
                            range: raw.range.clone(),
                        });
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                    DirectiveCommand::ElseIf(condition) => {
                        let Some(frame) = conditionals.last_mut() else {
                            return Err(malformed_directive(
                                &source_id,
                                raw.range.clone(),
                                &raw,
                                source,
                            ));
                        };
                        if frame.saw_else {
                            return Err(PrepareSourceError::MalformedNesting {
                                source_id,
                                range: raw.range,
                                directive: "ELSEIF after ELSE".to_string(),
                            });
                        }
                        let current_active = if frame.parent_active && !frame.branch_taken {
                            require_condition(
                                condition.evaluate(&symbols),
                                &source_id,
                                raw.range.clone(),
                                source,
                            )?
                        } else {
                            false
                        };
                        frame.current_active = current_active;
                        frame.branch_taken |= current_active;
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                    DirectiveCommand::Else => {
                        let Some(frame) = conditionals.last_mut() else {
                            return Err(malformed_directive(
                                &source_id,
                                raw.range.clone(),
                                &raw,
                                source,
                            ));
                        };
                        if frame.saw_else {
                            return Err(PrepareSourceError::MalformedNesting {
                                source_id,
                                range: raw.range,
                                directive: "duplicate ELSE".to_string(),
                            });
                        }
                        frame.saw_else = true;
                        frame.current_active = frame.parent_active && !frame.branch_taken;
                        frame.branch_taken = true;
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                    DirectiveCommand::EndIf => {
                        if conditionals.pop().is_none() {
                            return Err(malformed_directive(
                                &source_id,
                                raw.range.clone(),
                                &raw,
                                source,
                            ));
                        }
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                    DirectiveCommand::Define(symbol) => {
                        if frame_active {
                            symbols.set(symbol, true);
                        }
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                    DirectiveCommand::Undef(symbol) => {
                        if frame_active {
                            symbols.set(symbol, false);
                        }
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                    DirectiveCommand::Include(requested) => {
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id.clone(),
                            raw.range.clone(),
                            expansion_id.clone(),
                            source,
                            limits,
                        )?;
                        if !frame_active {
                            continue;
                        }
                        let key = IncludeKey {
                            source_id: source_id.clone(),
                            range: raw.range.clone(),
                        };
                        let Some(target_source_id) = bindings.get(&key).cloned() else {
                            return Err(PrepareSourceError::UnresolvedInclude {
                                source_id,
                                range: raw.range,
                                requested,
                            });
                        };
                        let occurrence = expanded_occurrences.checked_add(1).ok_or_else(|| {
                            PrepareSourceError::BudgetExceeded {
                                budget: PreparationBudget::ExpandedOccurrences,
                                source_id: source_id.clone(),
                                range: raw.range.clone(),
                                limit: limits.max_expanded_occurrences,
                                observed: usize::MAX,
                            }
                        })?;
                        if occurrence > limits.max_expanded_occurrences {
                            return Err(PrepareSourceError::BudgetExceeded {
                                budget: PreparationBudget::ExpandedOccurrences,
                                source_id,
                                range: raw.range,
                                limit: limits.max_expanded_occurrences,
                                observed: occurrence,
                            });
                        }
                        expanded_occurrences = occurrence;
                        let depth = stack[frame_index].include_depth.saturating_add(1);
                        check_depth(
                            PreparationBudget::IncludeDepth,
                            depth,
                            limits.max_include_depth,
                            &stack[frame_index].source_id,
                            raw.range.clone(),
                        )?;
                        if include_chain.contains(&target_source_id) {
                            let mut cycle = include_chain.clone();
                            cycle.push(target_source_id);
                            return Err(PrepareSourceError::IncludeCycle {
                                source_id,
                                range: raw.range,
                                cycle,
                            });
                        }
                        catalog.ensure_lexed(&target_source_id, limits, &mut usage.work)?;
                        let child_expansion = child_expansion_id(&expansion_id, occurrence);
                        stack.push(SourceFrame {
                            source_id: target_source_id.clone(),
                            expansion_id: child_expansion,
                            item_index: 0,
                            include_depth: depth,
                            output_start: output.len(),
                        });
                        include_chain.push(target_source_id);
                    }
                    DirectiveCommand::Unsupported(directive) => {
                        if frame_active {
                            return Err(PrepareSourceError::UnsupportedDirective {
                                source_id,
                                range: raw.range,
                                directive,
                            });
                        }
                        append_masked(
                            &mut output,
                            &mut segments,
                            source_id,
                            raw.range,
                            expansion_id,
                            source,
                            limits,
                        )?;
                    }
                }
            }
        }
    }

    if let Some(frame) = conditionals.last() {
        return Err(PrepareSourceError::MalformedNesting {
            source_id: frame.source_id.clone(),
            range: frame.range.clone(),
            directive: "end of source reached before ENDIF".to_string(),
        });
    }

    if !active_content {
        return Err(PrepareSourceError::NoCompleteContent { root_source_id });
    }

    let source_map = SourceMap::new(output.as_slice(), loaded_sources.to_vec(), segments)?;
    PreparedSource::new(
        options.prepared_source_id,
        output,
        source_map,
        options.configuration_id,
        PreparationFidelity::Complete,
        PreparationProvenance::Configured,
    )
    .map_err(Into::into)
}

/// Variant with the options before bindings for callers that naturally build
/// configuration first.  It delegates to [`prepare_source`] and has identical
/// semantics.
pub fn prepare_source_with_options<R>(
    root_source_id: R,
    loaded_sources: &[SourceSnapshot],
    options: PrepareSourceOptions,
    include_bindings: &[IncludeBinding],
) -> Result<PreparedSource, PrepareSourceError>
where
    R: Borrow<ProjectSourceId>,
{
    prepare_source(root_source_id, loaded_sources, include_bindings, options)
}

#[derive(Debug, Default)]
struct PreparationUsage {
    work: usize,
    directives: usize,
    expression_bytes: usize,
    expression_tokens: usize,
}

fn charge(
    counter: &mut usize,
    budget: PreparationBudget,
    amount: usize,
    limit: usize,
    source_id: &ProjectSourceId,
    range: Range<usize>,
) -> Result<(), PrepareSourceError> {
    let observed = counter.checked_add(amount).unwrap_or(usize::MAX);
    if observed > limit {
        return Err(PrepareSourceError::BudgetExceeded {
            budget,
            source_id: source_id.clone(),
            range,
            limit,
            observed,
        });
    }
    *counter = observed;
    Ok(())
}

fn charge_work(
    usage: &mut PreparationUsage,
    amount: usize,
    limits: PreparationLimits,
    source_id: &ProjectSourceId,
    range: Range<usize>,
) -> Result<(), PrepareSourceError> {
    charge(
        &mut usage.work,
        PreparationBudget::Work,
        amount,
        limits.max_work,
        source_id,
        range,
    )
}

fn charge_directive(
    usage: &mut PreparationUsage,
    limits: PreparationLimits,
    source_id: &ProjectSourceId,
    range: Range<usize>,
) -> Result<(), PrepareSourceError> {
    charge(
        &mut usage.directives,
        PreparationBudget::Directives,
        1,
        limits.max_directives,
        source_id,
        range.clone(),
    )?;
    charge_work(usage, range.len(), limits, source_id, range)
}

fn charge_expression(
    usage: &mut PreparationUsage,
    bytes: usize,
    tokens: usize,
    limits: PreparationLimits,
    source_id: &ProjectSourceId,
    range: Range<usize>,
) -> Result<(), PrepareSourceError> {
    charge(
        &mut usage.expression_bytes,
        PreparationBudget::ExpressionBytes,
        bytes,
        limits.max_expression_bytes,
        source_id,
        range.clone(),
    )?;
    charge_work(usage, bytes, limits, source_id, range.clone())?;
    charge(
        &mut usage.expression_tokens,
        PreparationBudget::ExpressionTokens,
        tokens,
        limits.max_expression_tokens,
        source_id,
        range,
    )
}

fn check_depth(
    budget: PreparationBudget,
    depth: usize,
    limit: usize,
    source_id: &ProjectSourceId,
    range: Range<usize>,
) -> Result<(), PrepareSourceError> {
    if depth > limit {
        return Err(PrepareSourceError::BudgetExceeded {
            budget,
            source_id: source_id.clone(),
            range,
            limit,
            observed: depth,
        });
    }
    Ok(())
}

fn validate_include_bindings(
    catalog: &mut SourceCatalog<'_>,
    include_bindings: &[IncludeBinding],
    limits: PreparationLimits,
    work: &mut usize,
) -> Result<HashMap<IncludeKey, ProjectSourceId>, PrepareSourceError> {
    let mut bindings = HashMap::with_capacity(include_bindings.len());
    for binding in include_bindings {
        let source_id = &binding.including_source_id;
        let Some(source_len) = catalog.source(source_id).map(SourceSnapshot::len) else {
            return Err(PrepareSourceError::IncludeBindingSourceNotLoaded {
                source_id: source_id.clone(),
            });
        };
        if binding.directive_range.start > binding.directive_range.end
            || binding.directive_range.end > source_len
        {
            return Err(PrepareSourceError::IncludeBindingRangeOutOfBounds {
                source_id: source_id.clone(),
                range: binding.directive_range.clone(),
                source_len,
            });
        }
        catalog.ensure_lexed(source_id, limits, work)?;
        let Some(directive) = catalog
            .lexed(source_id)
            .directives
            .get(&(binding.directive_range.start, binding.directive_range.end))
        else {
            return Err(PrepareSourceError::IncludeBindingNotIncludeDirective {
                source_id: source_id.clone(),
                range: binding.directive_range.clone(),
            });
        };
        let source = catalog
            .source(source_id)
            .expect("binding source remains loaded after lexing");
        let argument = trim_range(source.bytes(), directive.argument_range.clone());
        let is_include = (keyword_is(source.bytes(), directive.keyword_range.clone(), b"I")
            || keyword_is(source.bytes(), directive.keyword_range.clone(), b"INCLUDE"))
            && !argument.is_empty()
            && !matches!(source.bytes().get(argument.start), Some(b'+' | b'-'));
        if !is_include {
            return Err(PrepareSourceError::IncludeBindingNotIncludeDirective {
                source_id: source_id.clone(),
                range: binding.directive_range.clone(),
            });
        }
        if let Err(message) = validate_include_argument(source.bytes(), argument.clone()) {
            return Err(invalid_directive(
                source_id,
                binding.directive_range.clone(),
                source_text(source.bytes(), directive.range.clone()),
                message,
            ));
        }
        if catalog.source(&binding.target_source_id).is_none() {
            return Err(PrepareSourceError::IncludeTargetNotLoaded {
                source_id: source_id.clone(),
                range: binding.directive_range.clone(),
                target_source_id: binding.target_source_id.clone(),
            });
        }
        let key = IncludeKey {
            source_id: source_id.clone(),
            range: binding.directive_range.clone(),
        };
        if bindings
            .insert(key, binding.target_source_id.clone())
            .is_some()
        {
            return Err(PrepareSourceError::DuplicateIncludeBinding {
                source_id: source_id.clone(),
                range: binding.directive_range.clone(),
            });
        }
    }
    Ok(bindings)
}

fn lex_source(
    source_id: &ProjectSourceId,
    source: &[u8],
    limits: PreparationLimits,
) -> Result<LexedSource, PrepareSourceError> {
    let mut items = Vec::new();
    let mut directives = HashMap::new();
    let mut cursor = 0usize;
    let mut text_start = 0usize;
    while cursor < source.len() {
        let directive_start = match source[cursor] {
            b'\'' | b'"' => {
                cursor = skip_string(source, cursor);
                continue;
            }
            b'/' if source.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(source, cursor + 2);
                continue;
            }
            b'{' if source.get(cursor + 1) == Some(&b'$') => Some((b'}', 2)),
            b'(' if source.get(cursor + 1) == Some(&b'*')
                && source.get(cursor + 2) == Some(&b'$') =>
            {
                Some((b'*', 3))
            }
            b'{' => {
                cursor = skip_block_comment(source, cursor + 1, b'}');
                continue;
            }
            b'(' if source.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_paren_comment(source, cursor + 2);
                continue;
            }
            _ => {
                cursor += 1;
                continue;
            }
        };

        let (closing, body_offset) = directive_start.expect("directive start is present");
        if text_start < cursor {
            items.push(LexItem::Text(text_start..cursor));
        }
        let end = if closing == b'}' {
            find_byte(source, cursor + body_offset, b'}')
        } else {
            find_paren_close(source, cursor + body_offset)
        };
        let Some(end) = end else {
            return Err(PrepareSourceError::InvalidDirective {
                source_id: source_id.clone(),
                range: cursor..source.len(),
                directive: source_text(source, cursor..source.len()),
                message: "unterminated directive".to_string(),
            });
        };
        let end_exclusive = if closing == b'}' { end + 1 } else { end + 2 };
        let body_start = cursor + body_offset;
        let keyword_start = skip_ascii_whitespace(source, body_start);
        let keyword_end = source[keyword_start..end]
            .iter()
            .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            .map(|offset| keyword_start + offset)
            .unwrap_or(end);
        let raw = RawDirective {
            range: cursor..end_exclusive,
            argument_range: keyword_end..end,
            keyword_range: keyword_start..keyword_end,
        };
        if directives.len() >= limits.max_directives {
            return Err(PrepareSourceError::BudgetExceeded {
                budget: PreparationBudget::Directives,
                source_id: source_id.clone(),
                range: raw.range.clone(),
                limit: limits.max_directives,
                observed: directives.len() + 1,
            });
        }
        directives.insert((raw.range.start, raw.range.end), raw.clone());
        items.push(LexItem::Directive(raw));
        cursor = end_exclusive;
        text_start = cursor;
    }
    if text_start < source.len() {
        items.push(LexItem::Text(text_start..source.len()));
    }
    Ok(LexedSource { items, directives })
}

fn parse_directive(
    raw: &RawDirective,
    source: &[u8],
    source_id: &ProjectSourceId,
    usage: &mut PreparationUsage,
    limits: PreparationLimits,
) -> Result<DirectiveCommand, PrepareSourceError> {
    let directive = source_text(source, raw.range.clone());
    let argument_range = trim_range(source, raw.argument_range.clone());
    let argument = source_text(source, argument_range.clone());
    let keyword = keyword_text(source, raw.keyword_range.clone());
    match keyword.as_str() {
        "IFDEF" => Ok(DirectiveCommand::If(ConditionExpr::defined(parse_symbol(
            source,
            argument_range,
            source_id,
            raw.range.clone(),
            &directive,
        )?))),
        "IFNDEF" => Ok(DirectiveCommand::If(ConditionExpr::not(
            ConditionExpr::defined(parse_symbol(
                source,
                argument_range,
                source_id,
                raw.range.clone(),
                &directive,
            )?),
        ))),
        "IF" => Ok(DirectiveCommand::If(parse_expression(
            source,
            argument_range,
            source_id,
            raw.range.clone(),
            &directive,
            usage,
            limits,
        )?)),
        "ELSEIF" | "ELIF" => Ok(DirectiveCommand::ElseIf(parse_expression(
            source,
            argument_range,
            source_id,
            raw.range.clone(),
            &directive,
            usage,
            limits,
        )?)),
        "ELSE" => {
            if !argument_range.is_empty() {
                Err(invalid_directive(
                    source_id,
                    raw.range.clone(),
                    directive,
                    "ELSE does not accept an expression",
                ))
            } else {
                Ok(DirectiveCommand::Else)
            }
        }
        "ENDIF" | "IFEND" => {
            if !argument_range.is_empty() {
                Err(invalid_directive(
                    source_id,
                    raw.range.clone(),
                    directive,
                    "ENDIF does not accept an argument",
                ))
            } else {
                Ok(DirectiveCommand::EndIf)
            }
        }
        "DEFINE" => Ok(DirectiveCommand::Define(parse_symbol(
            source,
            argument_range,
            source_id,
            raw.range.clone(),
            &directive,
        )?)),
        "UNDEF" => Ok(DirectiveCommand::Undef(parse_symbol(
            source,
            argument_range,
            source_id,
            raw.range.clone(),
            &directive,
        )?)),
        "I" | "INCLUDE" => {
            if argument_range.is_empty()
                || matches!(source.get(argument_range.start), Some(b'+' | b'-'))
            {
                Ok(DirectiveCommand::Unsupported(directive))
            } else if let Err(message) = validate_include_argument(source, argument_range.clone()) {
                Err(invalid_directive(
                    source_id,
                    raw.range.clone(),
                    directive,
                    message,
                ))
            } else {
                Ok(DirectiveCommand::Include(argument))
            }
        }
        _ => Ok(DirectiveCommand::Unsupported(directive)),
    }
}

fn parse_symbol(
    source: &[u8],
    range: Range<usize>,
    source_id: &ProjectSourceId,
    directive_range: Range<usize>,
    directive: &str,
) -> Result<String, PrepareSourceError> {
    if range.is_empty() || !is_identifier(source, range.clone()) {
        return Err(invalid_directive(
            source_id,
            directive_range,
            directive.to_string(),
            "expected one preprocessor symbol",
        ));
    }
    Ok(source[range]
        .iter()
        .map(|byte| byte.to_ascii_uppercase() as char)
        .collect())
}

#[derive(Debug, Clone)]
enum ExprTokenKind {
    True,
    False,
    Defined,
    Not,
    And,
    Or,
    LeftParen,
    RightParen,
    Identifier(String),
}

#[derive(Debug, Clone)]
struct ExprToken {
    kind: ExprTokenKind,
}

fn parse_expression(
    source: &[u8],
    range: Range<usize>,
    source_id: &ProjectSourceId,
    directive_range: Range<usize>,
    directive: &str,
    usage: &mut PreparationUsage,
    limits: PreparationLimits,
) -> Result<ConditionExpr, PrepareSourceError> {
    let expression_bytes = source[range.clone()].len();
    charge_expression(
        usage,
        expression_bytes,
        0,
        limits,
        source_id,
        directive_range.clone(),
    )?;
    let tokens = tokenize_expression(
        source,
        range,
        source_id,
        directive_range.clone(),
        directive,
        usage,
        limits,
    )?;
    let mut parser = ExpressionParser {
        tokens,
        position: 0,
        source_id,
        directive_range,
        directive,
        max_depth: limits.max_expression_depth,
    };
    let expression = parser.parse_or(0)?;
    if parser.position != parser.tokens.len() {
        return Err(invalid_directive(
            source_id,
            parser.directive_range.clone(),
            directive.to_string(),
            "unexpected token after condition expression",
        ));
    }
    Ok(expression)
}

fn tokenize_expression(
    source: &[u8],
    range: Range<usize>,
    source_id: &ProjectSourceId,
    directive_range: Range<usize>,
    directive: &str,
    usage: &mut PreparationUsage,
    limits: PreparationLimits,
) -> Result<Vec<ExprToken>, PrepareSourceError> {
    let mut tokens = Vec::new();
    let mut cursor = range.start;
    while cursor < range.end {
        if source[cursor].is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        let kind = match source[cursor] {
            b'(' => {
                cursor += 1;
                ExprTokenKind::LeftParen
            }
            b')' => {
                cursor += 1;
                ExprTokenKind::RightParen
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = cursor;
                cursor += 1;
                while cursor < range.end
                    && (source[cursor].is_ascii_alphanumeric() || source[cursor] == b'_')
                {
                    cursor += 1;
                }
                let word = source[start..cursor]
                    .iter()
                    .map(|byte| byte.to_ascii_uppercase() as char)
                    .collect::<String>();
                match word.as_str() {
                    "TRUE" => ExprTokenKind::True,
                    "FALSE" => ExprTokenKind::False,
                    "DEFINED" => ExprTokenKind::Defined,
                    "NOT" => ExprTokenKind::Not,
                    "AND" => ExprTokenKind::And,
                    "OR" => ExprTokenKind::Or,
                    _ => ExprTokenKind::Identifier(word),
                }
            }
            _ => {
                return Err(invalid_directive(
                    source_id,
                    directive_range,
                    directive.to_string(),
                    "unsupported token in condition expression",
                ));
            }
        };
        charge(
            &mut usage.expression_tokens,
            PreparationBudget::ExpressionTokens,
            1,
            limits.max_expression_tokens,
            source_id,
            directive_range.clone(),
        )?;
        charge_work(usage, 1, limits, source_id, directive_range.clone())?;
        tokens.push(ExprToken { kind });
    }
    if tokens.is_empty() {
        return Err(invalid_directive(
            source_id,
            directive_range,
            directive.to_string(),
            "condition expression must not be empty",
        ));
    }
    Ok(tokens)
}

struct ExpressionParser<'a> {
    tokens: Vec<ExprToken>,
    position: usize,
    source_id: &'a ProjectSourceId,
    directive_range: Range<usize>,
    directive: &'a str,
    max_depth: usize,
}

impl<'a> ExpressionParser<'a> {
    fn parse_or(&mut self, depth: usize) -> Result<ConditionExpr, PrepareSourceError> {
        let mut expression = self.parse_and(depth)?;
        while self.take(|kind| matches!(kind, ExprTokenKind::Or)) {
            let right = self.parse_and(depth)?;
            expression = ConditionExpr::combine(expression, right, ConditionOp::Or);
        }
        Ok(expression)
    }

    fn parse_and(&mut self, depth: usize) -> Result<ConditionExpr, PrepareSourceError> {
        let mut expression = self.parse_not(depth)?;
        while self.take(|kind| matches!(kind, ExprTokenKind::And)) {
            let right = self.parse_not(depth)?;
            expression = ConditionExpr::combine(expression, right, ConditionOp::And);
        }
        Ok(expression)
    }

    fn parse_not(&mut self, depth: usize) -> Result<ConditionExpr, PrepareSourceError> {
        if self.take(|kind| matches!(kind, ExprTokenKind::Not)) {
            self.ensure_depth(depth + 1)?;
            return Ok(ConditionExpr::not(self.parse_not(depth + 1)?));
        }
        self.parse_primary(depth)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<ConditionExpr, PrepareSourceError> {
        let Some(token) = self.next() else {
            return Err(self.invalid("expected condition operand"));
        };
        match token.kind {
            ExprTokenKind::True => Ok(ConditionExpr::literal(true)),
            ExprTokenKind::False => Ok(ConditionExpr::literal(false)),
            ExprTokenKind::Defined => {
                if !self.take(|kind| matches!(kind, ExprTokenKind::LeftParen)) {
                    return Err(self.invalid("Defined must be followed by (name)"));
                }
                let Some(symbol) = self.next() else {
                    return Err(self.invalid("Defined requires a symbol"));
                };
                let ExprTokenKind::Identifier(symbol) = symbol.kind else {
                    return Err(self.invalid("Defined requires a symbol"));
                };
                if !self.take(|kind| matches!(kind, ExprTokenKind::RightParen)) {
                    return Err(self.invalid("Defined requires a closing parenthesis"));
                }
                Ok(ConditionExpr::defined(symbol))
            }
            ExprTokenKind::LeftParen => {
                self.ensure_depth(depth + 1)?;
                let expression = self.parse_or(depth + 1)?;
                if !self.take(|kind| matches!(kind, ExprTokenKind::RightParen)) {
                    return Err(self.invalid("missing closing parenthesis"));
                }
                Ok(expression)
            }
            ExprTokenKind::Identifier(_)
            | ExprTokenKind::Not
            | ExprTokenKind::And
            | ExprTokenKind::Or
            | ExprTokenKind::RightParen => {
                Err(self
                    .invalid("expected TRUE, FALSE, Defined(name), or parenthesized expression"))
            }
        }
    }

    fn take<F>(&mut self, predicate: F) -> bool
    where
        F: FnOnce(&ExprTokenKind) -> bool,
    {
        let Some(token) = self.tokens.get(self.position) else {
            return false;
        };
        if predicate(&token.kind) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<ExprToken> {
        let token = self.tokens.get(self.position).cloned()?;
        self.position += 1;
        Some(token)
    }

    fn ensure_depth(&self, depth: usize) -> Result<(), PrepareSourceError> {
        if depth > self.max_depth {
            return Err(PrepareSourceError::BudgetExceeded {
                budget: PreparationBudget::ExpressionDepth,
                source_id: self.source_id.clone(),
                range: self.directive_range.clone(),
                limit: self.max_depth,
                observed: depth,
            });
        }
        Ok(())
    }

    fn invalid(&self, message: &str) -> PrepareSourceError {
        invalid_directive(
            self.source_id,
            self.directive_range.clone(),
            self.directive.to_string(),
            message,
        )
    }
}

fn require_condition(
    state: TriState,
    source_id: &ProjectSourceId,
    range: Range<usize>,
    source: &[u8],
) -> Result<bool, PrepareSourceError> {
    match state {
        TriState::True => Ok(true),
        TriState::False => Ok(false),
        TriState::Unknown => Err(PrepareSourceError::UnknownActiveCondition {
            source_id: source_id.clone(),
            range: range.clone(),
            expression: source_text(source, range),
        }),
    }
}

fn invalid_directive(
    source_id: &ProjectSourceId,
    range: Range<usize>,
    directive: String,
    message: &str,
) -> PrepareSourceError {
    PrepareSourceError::InvalidDirective {
        source_id: source_id.clone(),
        range,
        directive,
        message: message.to_string(),
    }
}

fn malformed_directive(
    source_id: &ProjectSourceId,
    range: Range<usize>,
    raw: &RawDirective,
    source: &[u8],
) -> PrepareSourceError {
    PrepareSourceError::MalformedNesting {
        source_id: source_id.clone(),
        range,
        directive: keyword_text(source, raw.keyword_range.clone()),
    }
}

fn keyword_text(source: &[u8], range: Range<usize>) -> String {
    source[range]
        .iter()
        .map(|byte| byte.to_ascii_uppercase() as char)
        .collect()
}

fn keyword_is(source: &[u8], range: Range<usize>, expected: &[u8]) -> bool {
    range.len() == expected.len()
        && source[range]
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.to_ascii_uppercase() == *expected)
}

fn validate_include_argument(source: &[u8], range: Range<usize>) -> Result<(), &'static str> {
    let Some(&first) = source.get(range.start) else {
        return Err("include path must not be empty");
    };
    if matches!(first, b'\'' | b'"') {
        let quote = first;
        let mut cursor = range.start + 1;
        let content_start = cursor;
        while cursor < range.end {
            match source[cursor] {
                byte if byte == quote => {
                    if source.get(cursor + 1) == Some(&quote) {
                        cursor += 2;
                    } else {
                        if cursor == content_start {
                            return Err("quoted include path must not be empty");
                        }
                        cursor += 1;
                        return if cursor == range.end {
                            Ok(())
                        } else {
                            Err("quoted include path has trailing tokens")
                        };
                    }
                }
                b'\r' | b'\n' => return Err("quoted include path must not contain a line break"),
                _ => cursor += 1,
            }
        }
        Err("unterminated quoted include path")
    } else if source[range]
        .iter()
        .all(|byte| !byte.is_ascii_whitespace() && !matches!(*byte, b'\'' | b'"'))
    {
        Ok(())
    } else {
        Err("bare include path must be one token")
    }
}

fn normalize_symbol(symbol: &str) -> Option<String> {
    let bytes = symbol.as_bytes();
    if bytes.is_empty()
        || !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_'
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|byte| byte.to_ascii_uppercase() as char)
            .collect(),
    )
}

fn is_identifier(source: &[u8], range: Range<usize>) -> bool {
    if range.is_empty() {
        return false;
    }
    normalize_symbol(std::str::from_utf8(&source[range]).unwrap_or_default()).is_some()
}

fn trim_range(source: &[u8], mut range: Range<usize>) -> Range<usize> {
    while range.start < range.end && source[range.start].is_ascii_whitespace() {
        range.start += 1;
    }
    while range.start < range.end && source[range.end - 1].is_ascii_whitespace() {
        range.end -= 1;
    }
    range
}

fn source_text(source: &[u8], range: Range<usize>) -> String {
    String::from_utf8_lossy(&source[range]).into_owned()
}

fn append_copied(
    output: &mut Vec<u8>,
    segments: &mut Vec<SourceMapSegment>,
    source_id: ProjectSourceId,
    range: Range<usize>,
    expansion_id: ExpansionId,
    source: &[u8],
    limits: PreparationLimits,
) -> Result<(), PrepareSourceError> {
    append_bytes(
        output,
        source[range.clone()].iter().copied(),
        range.len(),
        limits,
        &source_id,
        range.clone(),
    )?;
    segments.push(SourceMapSegment::copied(
        output.len() - range.len()..output.len(),
        source_id,
        range,
        expansion_id,
    ));
    Ok(())
}

fn append_masked(
    output: &mut Vec<u8>,
    segments: &mut Vec<SourceMapSegment>,
    source_id: ProjectSourceId,
    range: Range<usize>,
    expansion_id: ExpansionId,
    source: &[u8],
    limits: PreparationLimits,
) -> Result<(), PrepareSourceError> {
    let bytes = source[range.clone()].iter().map(|byte| {
        if matches!(*byte, b'\r' | b'\n') {
            *byte
        } else {
            b' '
        }
    });
    append_bytes(
        output,
        bytes,
        range.len(),
        limits,
        &source_id,
        range.clone(),
    )?;
    segments.push(SourceMapSegment::masked(
        output.len() - range.len()..output.len(),
        source_id,
        range,
        expansion_id,
    ));
    Ok(())
}

fn append_synthetic(
    output: &mut Vec<u8>,
    segments: &mut Vec<SourceMapSegment>,
    source_id: &ProjectSourceId,
    expansion_id: ExpansionId,
    source_len: usize,
    usage: &mut PreparationUsage,
    limits: PreparationLimits,
) -> Result<(), PrepareSourceError> {
    charge_work(usage, 1, limits, source_id, source_len..source_len)?;
    append_bytes(
        output,
        std::iter::once(b'\n'),
        1,
        limits,
        source_id,
        source_len..source_len,
    )?;
    segments.push(SourceMapSegment::synthetic(
        output.len() - 1..output.len(),
        expansion_id,
    ));
    Ok(())
}

fn append_bytes<I>(
    output: &mut Vec<u8>,
    bytes: I,
    len: usize,
    limits: PreparationLimits,
    source_id: &ProjectSourceId,
    range: Range<usize>,
) -> Result<(), PrepareSourceError>
where
    I: IntoIterator<Item = u8>,
{
    let observed = output.len().saturating_add(len);
    if observed > limits.max_output_bytes {
        return Err(PrepareSourceError::BudgetExceeded {
            budget: PreparationBudget::OutputBytes,
            source_id: source_id.clone(),
            range,
            limit: limits.max_output_bytes,
            observed,
        });
    }
    output.extend(bytes);
    Ok(())
}

fn child_expansion_id(parent: &ExpansionId, occurrence: usize) -> ExpansionId {
    if parent == &ExpansionId::root() {
        ExpansionId::new(format!("include-{occurrence}"))
    } else {
        ExpansionId::new(format!("{}/include-{occurrence}", parent.as_str()))
    }
}

fn skip_string(source: &[u8], mut cursor: usize) -> usize {
    let quote = source[cursor];
    cursor += 1;
    while cursor < source.len() {
        if source[cursor] == quote {
            if source.get(cursor + 1) == Some(&quote) {
                cursor += 2;
            } else {
                return cursor + 1;
            }
        } else {
            cursor += 1;
        }
    }
    cursor
}

fn skip_line_comment(source: &[u8], mut cursor: usize) -> usize {
    while cursor < source.len() && !matches!(source[cursor], b'\r' | b'\n') {
        cursor += 1;
    }
    cursor
}

fn skip_block_comment(source: &[u8], mut cursor: usize, close: u8) -> usize {
    while cursor < source.len() {
        if source[cursor] == close {
            return cursor + 1;
        }
        cursor += 1;
    }
    cursor
}

fn skip_paren_comment(source: &[u8], mut cursor: usize) -> usize {
    while cursor + 1 < source.len() {
        if source[cursor] == b'*' && source[cursor + 1] == b')' {
            return cursor + 2;
        }
        cursor += 1;
    }
    source.len()
}

fn find_byte(source: &[u8], mut cursor: usize, needle: u8) -> Option<usize> {
    while cursor < source.len() {
        if source[cursor] == needle {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn find_paren_close(source: &[u8], mut cursor: usize) -> Option<usize> {
    while cursor + 1 < source.len() {
        if source[cursor] == b'*' && source[cursor + 1] == b')' {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn skip_ascii_whitespace(source: &[u8], mut cursor: usize) -> usize {
    while cursor < source.len() && source[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}
