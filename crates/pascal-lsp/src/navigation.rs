use crate::text;
use lsp_types::{Location, Position, Range, Url};
use pascal_core::conditional::{self, ConditionalAnalysis};
use pascal_core::directive_fragment_rewrite::DirectivePatch;
use pascal_core::{FileInfo, parser};
#[cfg(test)]
use pascal_project::CompilerVersion;
use pascal_project::ConditionalContext;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::{Node, Tree};

mod assistance;
mod call_hierarchy;
pub mod compiled_dcu;
mod documentation;
mod folding;
mod inlay;
mod overload;
mod rename;
pub(crate) use rename::BindingWorkBudget;
mod selection;
mod semantic_tokens;
mod symbols;
mod type_hierarchy;
#[cfg(test)]
pub(crate) use assistance::CompletionResolutionSeed;
pub(crate) use assistance::completion_prefix_at_position;
pub(crate) use assistance::{
    CompletionMetadata, CompletionOptions, CompletionResult, MissingUnitCandidate,
};

/// A source-backed class method declaration for which the complete indexed
/// unit proves that no implementation currently exists.
///
/// The candidate deliberately contains the rendered implementation header and
/// the safe section boundary computed from the same parsed source. Callers
/// still have to turn the boundary into a physical edit; keeping the proof and
/// header construction here prevents code-action code from reconstructing
/// routine identity from display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingMethodImplementationCandidate {
    pub(crate) anchor: Range,
    pub(crate) declaration: Range,
    pub(crate) owner: String,
    pub(crate) method: String,
    pub(crate) unit_name: String,
    pub(crate) header: String,
    pub(crate) insertion_offset: usize,
    pub(crate) identity: u64,
}

/// A proven interface obligation whose class member declaration and/or body
/// can be generated without changing an existing member.  The candidate keeps
/// the physical insertion facts beside the contract proof so the workspace
/// layer does not have to reconstruct an interface signature from display
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingInterfaceMethodImplementationCandidate {
    pub(crate) anchor: Range,
    pub(crate) class_declaration: Range,
    pub(crate) owner: String,
    pub(crate) method: String,
    pub(crate) interface_uri: Url,
    pub(crate) interface_owner: String,
    pub(crate) interface_method: String,
    pub(crate) unit_name: String,
    pub(crate) declaration_header: Option<String>,
    pub(crate) declaration_insert_start: Option<usize>,
    pub(crate) declaration_insert_end: Option<usize>,
    pub(crate) declaration_indent: Option<String>,
    pub(crate) declaration_owner_indent: Option<String>,
    pub(crate) declaration_add_public: bool,
    pub(crate) implementation_header: String,
    pub(crate) implementation_insertion_offset: usize,
    pub(crate) obligation_identity: u64,
    pub(crate) identity: u64,
}
pub(crate) use folding::{
    FOLDING_KIND_COMMENT, FOLDING_KIND_IMPORTS, FOLDING_KIND_REGION, FoldingRangeOptions,
};
pub(crate) use inlay::InlayHintOptions;
pub(crate) use rename::RenameBindingInfo;
#[cfg(test)]
pub(crate) use rename::test_cancel_after_checks;
#[cfg(test)]
pub(crate) use rename::{TestCancellationPhase, test_cancel_in_phase};
pub(crate) use selection::validate_selection_position_count;

#[cfg(test)]
thread_local! {
    static OWNER_TYPE_ROOT_LOOKUPS: Cell<usize> = const { Cell::new(0) };
    static TEST_UNIT_URL_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_NODE_SIBLING_FRONTIER_ENTRIES: Cell<usize> = const { Cell::new(0) };
    static TEST_LEGACY_EXPORTED_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_SCOPE_SCAN_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_SCOPE_CHAIN_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_OWNER_FALLBACKS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_OWNER_HEADER_NODE_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_GENERIC_CONTEXT_CHECKS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_NODE_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_INTERVAL_QUERY_COMPARISONS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_SHADOW_CHECKS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_GENERIC_SYMBOL_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_SEMANTIC_ANCESTRY_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_CANCEL_AFTER_SEMANTIC_ANCESTRY: Cell<bool> = const { Cell::new(false) };
    static TEST_CONTRACT_INTERFACE_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_CANCEL_AFTER_CONTRACT_INTERFACE: Cell<bool> = const { Cell::new(false) };
    static TEST_CANCEL_AFTER_SEMANTIC_SUBSCRIPT_CHILD: Cell<bool> = const { Cell::new(false) };
    static TEST_DOCUMENT_PARSE_CALLS: Cell<usize> = const { Cell::new(0) };
    static TEST_QUALIFIED_IDENTIFIER_NODE_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_CANCEL_AFTER_QUALIFIED_IDENTIFIER_NODE_VISITS: Cell<Option<usize>> =
        const { Cell::new(None) };
    static TEST_QUALIFIED_IDENTIFIER_CANCEL_REQUESTED: Cell<bool> = const { Cell::new(false) };
    static TEST_UNIT_PATH_NODE_VISITS: Cell<usize> = const { Cell::new(0) };
    static TEST_CANCEL_AFTER_UNIT_PATH_NODE_VISITS: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
fn test_record_materialization(counter: &'static std::thread::LocalKey<Cell<usize>>) {
    counter.with(|value| value.set(value.get().saturating_add(1)));
}

#[cfg(test)]
fn test_materialization_count(counter: &'static std::thread::LocalKey<Cell<usize>>) -> usize {
    counter.with(Cell::get)
}

#[cfg(test)]
fn test_reset_qualified_identifier_work() {
    TEST_QUALIFIED_IDENTIFIER_NODE_VISITS.with(|value| value.set(0));
    TEST_CANCEL_AFTER_QUALIFIED_IDENTIFIER_NODE_VISITS.with(|value| value.set(None));
    TEST_QUALIFIED_IDENTIFIER_CANCEL_REQUESTED.with(|value| value.set(false));
}

#[cfg(test)]
fn test_qualified_identifier_node_visits() -> usize {
    TEST_QUALIFIED_IDENTIFIER_NODE_VISITS.with(Cell::get)
}

#[cfg(test)]
fn test_cancel_after_qualified_identifier_node_visits(visits: usize) {
    TEST_CANCEL_AFTER_QUALIFIED_IDENTIFIER_NODE_VISITS.with(|value| value.set(Some(visits)));
}

#[cfg(test)]
fn test_record_qualified_identifier_node_visit() {
    let visits = TEST_QUALIFIED_IDENTIFIER_NODE_VISITS.with(|value| {
        let visits = value.get().saturating_add(1);
        value.set(visits);
        visits
    });
    TEST_CANCEL_AFTER_QUALIFIED_IDENTIFIER_NODE_VISITS.with(|limit| {
        if limit.get().is_some_and(|limit| visits >= limit) {
            TEST_QUALIFIED_IDENTIFIER_CANCEL_REQUESTED.with(|requested| requested.set(true));
        }
    });
}

#[cfg(test)]
fn test_reset_unit_path_work() {
    TEST_UNIT_PATH_NODE_VISITS.with(|value| value.set(0));
    TEST_CANCEL_AFTER_UNIT_PATH_NODE_VISITS.with(|value| value.set(None));
}

#[cfg(test)]
fn test_unit_path_node_visits() -> usize {
    TEST_UNIT_PATH_NODE_VISITS.with(Cell::get)
}

#[cfg(test)]
fn test_cancel_after_unit_path_node_visits(visits: usize) {
    TEST_CANCEL_AFTER_UNIT_PATH_NODE_VISITS.with(|value| value.set(Some(visits)));
}

#[cfg(test)]
fn test_record_unit_path_nodes(nodes: usize, cancel: &AtomicBool) {
    let visits = TEST_UNIT_PATH_NODE_VISITS.with(|value| {
        let visits = value.get().saturating_add(nodes);
        value.set(visits);
        visits
    });
    if TEST_CANCEL_AFTER_UNIT_PATH_NODE_VISITS
        .with(|limit| limit.get().is_some_and(|limit| visits >= limit))
    {
        cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
fn test_reset_materialization_counters() {
    TEST_UNIT_URL_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|value| value.set(0));
}

#[cfg(test)]
pub(super) fn test_reset_semantic_token_work_counters() {
    TEST_SEMANTIC_SCOPE_SCAN_VISITS.with(|value| value.set(0));
    TEST_SEMANTIC_SCOPE_CHAIN_VISITS.with(|value| value.set(0));
    TEST_SEMANTIC_OWNER_FALLBACKS.with(|value| value.set(0));
    TEST_SEMANTIC_OWNER_HEADER_NODE_VISITS.with(|value| value.set(0));
    TEST_SEMANTIC_GENERIC_CONTEXT_CHECKS.with(|value| value.set(0));
    TEST_SEMANTIC_NODE_VISITS.with(|value| value.set(0));
    TEST_SEMANTIC_INTERVAL_QUERY_COMPARISONS.with(|value| value.set(0));
    TEST_SEMANTIC_SHADOW_CHECKS.with(|value| value.set(0));
}

#[cfg(test)]
pub(super) fn test_semantic_token_work_counters()
-> (usize, usize, usize, usize, usize, usize, usize, usize) {
    (
        TEST_SEMANTIC_SCOPE_SCAN_VISITS.with(Cell::get),
        TEST_SEMANTIC_SCOPE_CHAIN_VISITS.with(Cell::get),
        TEST_SEMANTIC_OWNER_FALLBACKS.with(Cell::get),
        TEST_SEMANTIC_OWNER_HEADER_NODE_VISITS.with(Cell::get),
        TEST_SEMANTIC_GENERIC_CONTEXT_CHECKS.with(Cell::get),
        TEST_SEMANTIC_NODE_VISITS.with(Cell::get),
        TEST_SEMANTIC_INTERVAL_QUERY_COMPARISONS.with(Cell::get),
        TEST_SEMANTIC_SHADOW_CHECKS.with(Cell::get),
    )
}

#[cfg(test)]
pub(super) fn test_record_semantic_node_visit() {
    TEST_SEMANTIC_NODE_VISITS.with(|value| value.set(value.get().saturating_add(1)));
}

#[cfg(test)]
fn test_reset_semantic_generic_symbol_visits() {
    TEST_SEMANTIC_GENERIC_SYMBOL_VISITS.with(|value| value.set(0));
}

#[cfg(test)]
fn test_semantic_generic_symbol_visits() -> usize {
    TEST_SEMANTIC_GENERIC_SYMBOL_VISITS.with(Cell::get)
}

#[cfg(test)]
fn test_reset_semantic_ancestry_visits() {
    TEST_SEMANTIC_ANCESTRY_VISITS.with(|value| value.set(0));
}

#[cfg(test)]
fn test_semantic_ancestry_visits() -> usize {
    TEST_SEMANTIC_ANCESTRY_VISITS.with(Cell::get)
}

#[cfg(test)]
struct TestSemanticCancellationGuard(bool);

#[cfg(test)]
fn test_cancel_after_semantic_ancestry() -> TestSemanticCancellationGuard {
    let previous = TEST_CANCEL_AFTER_SEMANTIC_ANCESTRY.with(|enabled| {
        let previous = enabled.get();
        enabled.set(true);
        previous
    });
    TestSemanticCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for TestSemanticCancellationGuard {
    fn drop(&mut self) {
        TEST_CANCEL_AFTER_SEMANTIC_ANCESTRY.with(|enabled| enabled.set(self.0));
    }
}

#[cfg(test)]
fn test_record_semantic_ancestry_visit() {
    TEST_SEMANTIC_ANCESTRY_VISITS.with(|value| value.set(value.get().saturating_add(1)));
}

#[cfg(test)]
fn test_cancel_after_semantic_ancestry_if_requested(cancel: &AtomicBool) {
    if TEST_CANCEL_AFTER_SEMANTIC_ANCESTRY.with(Cell::get) && test_semantic_ancestry_visits() > 0 {
        TEST_CANCEL_AFTER_SEMANTIC_ANCESTRY.with(|enabled| enabled.set(false));
        cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
fn test_reset_contract_interface_visits() {
    TEST_CONTRACT_INTERFACE_VISITS.with(|value| value.set(0));
}

#[cfg(test)]
fn test_contract_interface_visits() -> usize {
    TEST_CONTRACT_INTERFACE_VISITS.with(Cell::get)
}

#[cfg(test)]
struct TestContractInterfaceCancellationGuard(bool);

#[cfg(test)]
fn test_cancel_after_contract_interface() -> TestContractInterfaceCancellationGuard {
    let previous = TEST_CANCEL_AFTER_CONTRACT_INTERFACE.with(|enabled| {
        let previous = enabled.get();
        enabled.set(true);
        previous
    });
    TestContractInterfaceCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for TestContractInterfaceCancellationGuard {
    fn drop(&mut self) {
        TEST_CANCEL_AFTER_CONTRACT_INTERFACE.with(|enabled| enabled.set(self.0));
    }
}

#[cfg(test)]
fn test_record_contract_interface_visit(cancel: &AtomicBool) {
    TEST_CONTRACT_INTERFACE_VISITS.with(|value| value.set(value.get().saturating_add(1)));
    if TEST_CANCEL_AFTER_CONTRACT_INTERFACE.with(Cell::get) {
        TEST_CANCEL_AFTER_CONTRACT_INTERFACE.with(|enabled| enabled.set(false));
        cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
struct TestSemanticSubscriptCancellationGuard(bool);

#[cfg(test)]
fn test_cancel_after_semantic_subscript_child() -> TestSemanticSubscriptCancellationGuard {
    let previous = TEST_CANCEL_AFTER_SEMANTIC_SUBSCRIPT_CHILD.with(|enabled| {
        let previous = enabled.get();
        enabled.set(true);
        previous
    });
    TestSemanticSubscriptCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for TestSemanticSubscriptCancellationGuard {
    fn drop(&mut self) {
        TEST_CANCEL_AFTER_SEMANTIC_SUBSCRIPT_CHILD.with(|enabled| enabled.set(self.0));
    }
}

#[cfg(test)]
fn test_cancel_after_semantic_subscript_child_if_requested(cancel: &AtomicBool) {
    if TEST_CANCEL_AFTER_SEMANTIC_SUBSCRIPT_CHILD.with(Cell::get) {
        TEST_CANCEL_AFTER_SEMANTIC_SUBSCRIPT_CHILD.with(|enabled| enabled.set(false));
        cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(super) fn test_reset_document_parse_count() {
    TEST_DOCUMENT_PARSE_CALLS.with(|value| value.set(0));
}

#[cfg(test)]
pub(super) fn test_document_parse_count() -> usize {
    TEST_DOCUMENT_PARSE_CALLS.with(Cell::get)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum IntegerKind {
    Literal,
    ShortInt,
    SmallInt,
    Integer,
    Byte,
    Word,
    Cardinal,
    LongWord,
    Int64,
    UInt64,
    NativeInt,
    NativeUInt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum BuiltinType {
    Integer(IntegerKind),
    Real,
    String,
    Character,
    Boolean,
}

/// The navigation operation requested by an LSP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavigationTarget {
    /// The declaration visible from the current lexical/import scope.
    Declaration,
    /// The implementation body when one is available.
    Definition,
    /// The implementation body, explicitly requested by the client.
    Implementation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticTokenResolutionMode {
    /// Resolve identifiers against the complete workspace snapshot.
    Full,
    /// Emit only syntax-derived tokens when semantic bindings are uncertain.
    LexicalOnly,
}

/// An in-memory, incrementally replaceable index of Pascal source documents.
///
/// The index deliberately has no filesystem policy: callers decide which
/// documents belong to a workspace and feed disk or unsaved-buffer contents to
/// [`NavigationIndex::update`].
#[derive(Default, Clone)]
pub struct NavigationIndex {
    documents: HashMap<Url, Document>,
    units: HashMap<String, Vec<Url>>,
    auto_import_discovery_complete: Option<bool>,
    auto_import_unit_providers: HashMap<String, Vec<Url>>,
    compiled_unit_uris: HashSet<Url>,
    compiled_unit_bytes: usize,
}

/// A byte span in a parsed Pascal source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

/// The proof boundary used by source-semantic diagnostics.
///
/// A failed lookup is not automatically an absence proof.  Callers may use
/// this status to distinguish a name that was not found in a complete model
/// from a lookup that was ambiguous or had an unsupported input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticProofStatus {
    Resolved,
    ProvenAbsent,
    Ambiguous,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticDiagnosticKind {
    UnresolvedIdentifier,
    MissingMember,
    TypeMismatch,
    IncompatibleArgument,
    InvalidOverride,
    MissingInterfaceImplementation,
}

/// Syntactic role required by an add-missing-unit use site.
///
/// This is intentionally narrower than the completion symbol catalogue.  A
/// completion candidate may be useful for exploration, while a code action
/// must establish that importing the candidate can make the particular Pascal
/// use valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissingUnitUseKind {
    Type,
    Callable,
    Value,
    WritableValue,
}

impl MissingUnitUseKind {
    fn accepts(self, symbol_kind: SymbolKind) -> bool {
        match self {
            Self::Type => symbol_kind == SymbolKind::Type,
            Self::Callable => symbol_kind == SymbolKind::Routine,
            Self::Value => matches!(
                symbol_kind,
                SymbolKind::Constant | SymbolKind::Variable | SymbolKind::EnumValue
            ),
            Self::WritableValue => symbol_kind == SymbolKind::Variable,
        }
    }
}

fn semantic_diagnostic_kind_rank(kind: SemanticDiagnosticKind) -> u8 {
    match kind {
        SemanticDiagnosticKind::UnresolvedIdentifier => 0,
        SemanticDiagnosticKind::MissingMember => 1,
        SemanticDiagnosticKind::TypeMismatch => 2,
        SemanticDiagnosticKind::IncompatibleArgument => 3,
        SemanticDiagnosticKind::InvalidOverride => 4,
        SemanticDiagnosticKind::MissingInterfaceImplementation => 5,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticDiagnostic {
    pub(crate) kind: SemanticDiagnosticKind,
    pub(crate) span: SourceSpan,
    pub(crate) message: String,
}

const MAX_SEMANTIC_DIAGNOSTIC_NODES: usize = 100_000;
const MAX_SEMANTIC_DIAGNOSTIC_WORK: usize = 100_000;
const MAX_SEMANTIC_DIAGNOSTIC_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_MISSING_UNIT_REQUEST_WORK: usize = 300_000;
pub(crate) const MAX_MISSING_UNIT_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_SEMANTIC_DIAGNOSTICS: usize = 256;
const MAX_CONTRACT_ANCESTRY_DEPTH: usize = 256;
const MAX_QUALIFIED_IMPORT_ALIAS_PATH_NODES: usize = 128;

/// Metadata for one unit imported by a document's `uses` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportMetadata {
    pub name: String,
    pub span: SourceSpan,
}

/// The source span of one parsed `uses` declaration.  The span includes the
/// `uses` keyword and its terminating semicolon; callers that need to preserve
/// trivia can use it as a boundary around the already-parsed import entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsesClauseMetadata {
    pub(crate) span: SourceSpan,
    pub(crate) interface: bool,
}

/// Facts needed before changing the relative order of imported units.
///
/// This is intentionally a source-backed proof, not a formatting preference.
/// The organizer currently accepts only dependency-free providers for relative
/// reordering.  That is stronger than checking only the selected unit: it
/// avoids treating a local fact as a proof about a transitive initialization or
/// finalization closure that has not been traversed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnitOrderSafety {
    pub(crate) complete: bool,
    pub(crate) has_initialization: bool,
    pub(crate) has_finalization: bool,
    pub(crate) has_helpers: bool,
    pub(crate) exported_names: Vec<String>,
    pub(crate) dependency_uris: Vec<Url>,
    pub(crate) conditional_fingerprint: u64,
}

impl NavigationIndex {
    /// Construct an empty navigation index.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_auto_import_discovery_complete(&mut self, complete: bool) {
        self.auto_import_discovery_complete = Some(complete);
    }

    pub(crate) fn set_auto_import_unit_providers(&mut self, providers: HashMap<String, Vec<Url>>) {
        self.auto_import_unit_providers = providers;
    }

    #[cfg(test)]
    pub(crate) fn seed_recovery_test_provider_maps(
        &mut self,
        units: HashMap<String, Vec<Url>>,
        auto_import_unit_providers: HashMap<String, Vec<Url>>,
    ) {
        self.units = units;
        self.auto_import_unit_providers = auto_import_unit_providers;
    }

    #[cfg(test)]
    pub(crate) fn recovery_test_provider_map_lengths(&self) -> (usize, usize) {
        (
            self.units.values().map(Vec::len).sum(),
            self.auto_import_unit_providers.values().map(Vec::len).sum(),
        )
    }

    /// Return the source-accurate outline for one indexed document.
    pub fn document_symbols(&self, uri: &Url) -> Result<Vec<lsp_types::DocumentSymbol>, String> {
        symbols::document_symbols(self, uri)
    }

    /// Return source-accurate syntax folding ranges for one indexed document.
    pub fn folding_ranges(&self, uri: &Url) -> Result<Vec<lsp_types::FoldingRange>, String> {
        folding::folding_ranges(self, uri)
    }

    /// Return syntax-structural selection ranges for each requested position.
    pub fn selection_ranges(
        &self,
        uri: &Url,
        positions: &[Position],
    ) -> Result<Vec<lsp_types::SelectionRange>, String> {
        selection::selection_ranges(self, uri, positions)
    }

    pub(crate) fn selection_ranges_with_cancel(
        &self,
        uri: &Url,
        positions: &[Position],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::SelectionRange>, String> {
        selection::selection_ranges_with_cancel(self, uri, positions, cancel)
    }

    pub(crate) fn folding_ranges_with_cancel(
        &self,
        uri: &Url,
        options: FoldingRangeOptions,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::FoldingRange>, String> {
        folding::folding_ranges_with_cancel(self, uri, options, cancel)
    }

    pub(crate) fn inlay_hints_with_cancel(
        &self,
        uri: &Url,
        range: Range,
        options: InlayHintOptions,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::InlayHint>, String> {
        inlay::inlay_hints_with_cancel(self, uri, range, options, cancel)
    }

    pub(crate) fn prepare_call_hierarchy_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<lsp_types::CallHierarchyItem>>, String> {
        call_hierarchy::prepare(self, uri, position, cancel)
    }

    pub(crate) fn prepare_type_hierarchy_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<lsp_types::TypeHierarchyItem>>, String> {
        type_hierarchy::prepare(self, uri, position, cancel)
    }

    pub(crate) fn type_hierarchy_supertypes_with_cancel(
        &self,
        item: &lsp_types::TypeHierarchyItem,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<lsp_types::TypeHierarchyItem>>, String> {
        type_hierarchy::supertypes(self, item, cancel)
    }

    pub(crate) fn type_hierarchy_subtypes_with_cancel(
        &self,
        item: &lsp_types::TypeHierarchyItem,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<lsp_types::TypeHierarchyItem>>, String> {
        type_hierarchy::subtypes(self, item, cancel)
    }

    pub(crate) fn incoming_calls_with_cancel(
        &self,
        item: &lsp_types::CallHierarchyItem,
        cancel: &AtomicBool,
    ) -> Result<Vec<lsp_types::CallHierarchyIncomingCall>, String> {
        call_hierarchy::incoming(self, item, cancel)
    }

    pub(crate) fn outgoing_calls_with_cancel(
        &self,
        item: &lsp_types::CallHierarchyItem,
        cancel: &AtomicBool,
    ) -> Result<Vec<lsp_types::CallHierarchyOutgoingCall>, String> {
        call_hierarchy::outgoing(self, item, cancel)
    }

    pub(crate) fn document_symbols_with_cancel(
        &self,
        uri: &Url,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::DocumentSymbol>, String> {
        symbols::document_symbols_with_cancel(self, uri, cancel)
    }

    pub(crate) fn semantic_tokens_with_resolution_mode(
        &self,
        uri: &Url,
        range: Option<&Range>,
        cancel: &std::sync::atomic::AtomicBool,
        mode: SemanticTokenResolutionMode,
    ) -> Result<lsp_types::SemanticTokens, String> {
        semantic_tokens::semantic_tokens_with_mode(self, uri, range, cancel, mode)
    }

    pub(crate) fn semantic_tokens_legend() -> lsp_types::SemanticTokensLegend {
        semantic_tokens::legend()
    }

    /// Return fully-resolved declarations from the indexed documents whose
    /// names contain `query`, case-insensitively.
    pub fn workspace_symbols(
        &self,
        query: &str,
    ) -> Result<Vec<lsp_types::SymbolInformation>, String> {
        symbols::workspace_symbols(self, query)
    }

    pub(crate) fn workspace_symbols_with_cancel(
        &self,
        query: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::SymbolInformation>, String> {
        symbols::workspace_symbols_with_cancel(self, query, cancel)
    }

    pub(crate) fn flatten_document_symbols(
        uri: &Url,
        symbols: Vec<lsp_types::DocumentSymbol>,
    ) -> Vec<lsp_types::SymbolInformation> {
        symbols::flatten_document_symbols(uri, symbols)
    }

    /// Parse and replace one document.
    ///
    /// Parsing happens before the old document is replaced, so a parser error
    /// leaves the last known-good overlay available to the caller.
    pub fn update(&mut self, uri: Url, source: String) -> Result<(), String> {
        self.update_with_defines(uri, source, &[])
    }

    pub(crate) fn update_compiled_unit_document(
        &mut self,
        document: &compiled_dcu::CompiledUnitDocument,
    ) -> Result<(), String> {
        let uri = document.uri().clone();
        if compiled_dcu::virtual_unit_identity(&uri).is_none() {
            return Err("compiled-unit URI is not canonical".to_string());
        }
        let size = document.text().len();
        if size > compiled_dcu::CompiledUnitDocument::MAX_TEXT_BYTES {
            return Err("compiled-unit virtual text exceeds its byte limit".to_string());
        }
        let existing_size = self
            .documents
            .get(&uri)
            .map_or(0, |existing| existing.source.len());
        let document_count =
            self.compiled_unit_uris.len() + usize::from(!self.compiled_unit_uris.contains(&uri));
        let retained_bytes = self
            .compiled_unit_bytes
            .saturating_sub(existing_size)
            .saturating_add(size);
        if document_count > 128 || retained_bytes > 4 * 1024 * 1024 {
            return Err("compiled-unit virtual index limit exceeded".to_string());
        }
        self.update(uri.clone(), document.text().to_owned())?;
        self.compiled_unit_uris.insert(uri);
        self.compiled_unit_bytes = retained_bytes;
        Ok(())
    }

    pub(crate) fn is_bound_compiled_unit_provider(
        &self,
        uri: &Url,
        importers: &HashSet<Url>,
    ) -> bool {
        self.compiled_unit_uris.contains(uri)
            && importers.iter().any(|importer| {
                self.documents
                    .get(importer)
                    .and_then(|document| document.import_bindings.as_ref())
                    .is_some_and(|bindings| bindings.values().any(|provider| provider == uri))
            })
    }

    fn compiled_shell_members_are_opaque(&self, uri: &Url) -> bool {
        self.compiled_unit_uris.contains(uri)
    }

    /// Snapshot the bounded authorization facts that let a selected-context
    /// navigation result authorize a later virtual-document read. The source
    /// hash is checked again by the owning workspace before serving content.
    pub(crate) fn compiled_unit_provider_bindings(&self) -> Vec<(Url, Url, u64)> {
        let mut bindings = Vec::new();
        for (importer, document) in &self.documents {
            let Some(imports) = &document.import_bindings else {
                continue;
            };
            let source_hash = pascal_project::content_hash_bytes(document.source.as_bytes());
            for provider in imports.values() {
                if self.compiled_unit_uris.contains(provider) {
                    bindings.push((importer.clone(), provider.clone(), source_hash));
                    if bindings.len() >= 4096 {
                        return bindings;
                    }
                }
            }
        }
        bindings
    }

    /// Parse and replace one document using the selected project's positive
    /// conditional-compilation facts.
    pub(crate) fn update_with_defines(
        &mut self,
        uri: Url,
        source: String,
        defines: &[String],
    ) -> Result<(), String> {
        let cancel = AtomicBool::new(false);
        self.update_with_context_with_cancel(
            uri,
            source,
            &ConditionalContext::from_defines(defines),
            &cancel,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn update_with_defines_with_cancel(
        &mut self,
        uri: Url,
        source: String,
        defines: &[String],
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        self.update_with_context_and_cached_with_cancel(
            uri,
            source,
            &ConditionalContext::from_defines(defines),
            None,
            cancel,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn update_with_defines_and_cached_with_cancel(
        &mut self,
        uri: Url,
        source: String,
        defines: &[String],
        cached: Option<Arc<ParsedDocument>>,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        self.update_with_context_and_cached_with_cancel(
            uri,
            source,
            &ConditionalContext::from_defines(defines),
            cached,
            cancel,
        )
    }

    pub(crate) fn update_with_context_with_cancel(
        &mut self,
        uri: Url,
        source: String,
        context: &ConditionalContext,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        self.update_with_context_and_cached_with_cancel(uri, source, context, None, cancel)
    }

    pub(crate) fn update_with_context_and_cached_with_cancel(
        &mut self,
        uri: Url,
        source: String,
        context: &ConditionalContext,
        cached: Option<Arc<ParsedDocument>>,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let previous = self
            .documents
            .get(&uri)
            .map(|document| document.parsed.clone());
        let document =
            Document::parse_with_cancel(uri.clone(), source, context, cancel, previous.or(cached))?;
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        let old_unit = self
            .documents
            .get(&uri)
            .map(|document| document.unit_name.clone());
        let new_unit = document.unit_name.clone();
        self.documents.insert(uri.clone(), document);
        if let Some(old_unit) = old_unit {
            self.remove_uri_from_unit(&old_unit, &uri);
        }
        let urls = self.units.entry(new_unit).or_default();
        urls.push(uri);
        urls.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        urls.dedup();
        Ok(())
    }

    /// Rebuild this retained semantic snapshot with one physical document
    /// replaced by a transformed source. The original import bindings and
    /// conditional contexts are copied exactly; only declaration/reference
    /// identities are rebound from the transformed tree.
    pub(crate) fn rebind_with_replaced_source_for_fix_all(
        &self,
        uri: &Url,
        replacement: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Self, String> {
        check_navigation_cancel(cancel)?;
        let target = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let document_count = self.documents.len();
        budget.require_owned_bytes(
            document_count.saturating_mul(std::mem::size_of::<(&Url, &Document)>()),
            cancel,
        )?;
        let mut documents = self.documents.iter().collect::<Vec<_>>();
        budget.require_work(
            documents
                .len()
                .saturating_mul(binary_search_work(documents.len())),
            cancel,
        )?;
        documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

        budget.require_owned_bytes(
            hash_map_clone_storage_bytes::<Url, Document>(document_count),
            cancel,
        )?;
        let mut result = Self {
            documents: HashMap::with_capacity(self.documents.len()),
            units: clone_units_for_fix_all(&self.units, cancel, budget)?,
            auto_import_discovery_complete: self.auto_import_discovery_complete,
            auto_import_unit_providers: clone_providers_for_fix_all(
                &self.auto_import_unit_providers,
                cancel,
                budget,
            )?,
            compiled_unit_uris: self.compiled_unit_uris.clone(),
            compiled_unit_bytes: self.compiled_unit_bytes,
        };
        for (document_uri, document) in documents {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            budget.require_owned_bytes(document_uri.as_str().len(), cancel)?;
            let import_bindings = clone_import_bindings_for_fix_all(
                document.import_bindings.as_ref(),
                cancel,
                budget,
            )?;
            result.documents.insert(
                document_uri.clone(),
                Document {
                    parsed: document.parsed.clone(),
                    import_bindings,
                    import_binding_fingerprint: document.import_binding_fingerprint,
                },
            );
        }

        // Unchanged documents keep their parsed semantic payload through Arc;
        // only the requested physical document is incrementally rebound.
        budget.require_work(1, cancel)?;
        budget.require_bytes(replacement.len(), cancel)?;
        budget.require_work(replacement.len(), cancel)?;
        budget.require_owned_bytes(replacement.len(), cancel)?;
        budget.require_work(rebind_work_reservation(target), cancel)?;
        budget.require_owned_bytes(rebind_owned_reservation(target), cancel)?;
        let target_import_state = result
            .documents
            .get_mut(uri)
            .map(|document| {
                (
                    document.import_bindings.take(),
                    document.import_binding_fingerprint,
                )
            })
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let replacement = replacement.to_owned();
        result.update_with_context_and_cached_with_cancel(
            uri.clone(),
            replacement,
            &target.conditional_context,
            None,
            cancel,
        )?;
        check_navigation_cancel(cancel)?;
        let rebound_target = result
            .documents
            .get_mut(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        rebound_target.import_bindings = target_import_state.0;
        rebound_target.import_binding_fingerprint = target_import_state.1;
        Ok(result)
    }

    pub(crate) fn reusable_documents(&self) -> Vec<(Url, Arc<ParsedDocument>)> {
        self.documents
            .iter()
            .map(|(uri, document)| (uri.clone(), document.parsed.clone()))
            .collect()
    }

    /// Remove a document and all symbols contributed by it.
    pub fn remove(&mut self, uri: &Url) {
        if self.compiled_unit_uris.remove(uri) {
            if let Some(document) = self.documents.get(uri) {
                self.compiled_unit_bytes = self
                    .compiled_unit_bytes
                    .saturating_sub(document.source.len());
            }
        }
        if let Some(document) = self.documents.remove(uri) {
            self.remove_uri_from_unit(&document.unit_name, uri);
        }
    }

    /// Return the parsed `uses` entries for a document.
    pub fn imports(&self, uri: &Url) -> Vec<ImportMetadata> {
        self.documents
            .get(uri)
            .map(|document| document.imports.clone())
            .unwrap_or_default()
    }

    /// Return the parsed `uses` declaration boundaries for one document.
    pub(crate) fn uses_clauses(&self, uri: &Url) -> Vec<UsesClauseMetadata> {
        self.documents
            .get(uri)
            .map(|document| document.uses_clauses.clone())
            .unwrap_or_default()
    }

    /// Return the project/provider binding selected for an imported unit.
    ///
    /// An explicit workspace binding is required for source actions.  The
    /// fallback catalogue used by a standalone `NavigationIndex` is useful
    /// for navigation, but it is not a sufficient identity proof for edits.
    pub(crate) fn import_provider_uri<'a>(&'a self, uri: &Url, name: &str) -> Option<&'a Url> {
        self.documents
            .get(uri)
            .and_then(|document| document.import_bindings.as_ref())
            .and_then(|bindings| bindings.get(&canonical_name(name)))
    }

    /// Return the identity of the complete import-binding context used for a
    /// document.  The value includes the effective conditional context and
    /// every resolved import spelling, so a project alias rebinding cannot
    /// look fresh merely because the same provider URI set remains present.
    pub(crate) fn import_binding_context_fingerprint(&self, uri: &Url) -> Option<u64> {
        self.documents
            .get(uri)
            .and_then(|document| document.import_binding_fingerprint)
    }

    /// Check qualified references to one import spelling using the parsed
    /// syntax tree rather than a raw-text search.  Comments, whitespace,
    /// escaped identifiers, and dotted namespace spellings are represented by
    /// the same semantic path here.  Failure to understand a qualified node
    /// fails closed for the optional organizer assistance.
    pub(crate) fn import_spelling_has_qualified_use_with_budget(
        &self,
        uri: &Url,
        clause: SourceSpan,
        spelling: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(true);
        };
        budget.require_bytes(spelling.len(), cancel)?;
        for part in spelling.split('.') {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            check_navigation_cancel(cancel)?;
            if part.is_empty() {
                return Ok(true);
            }
        }
        budget.require_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
        let mut pending = vec![document.tree.root_node()];
        while let Some(node) = pending.pop() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            check_navigation_cancel(cancel)?;
            if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot") {
                let span = Span::from_node(node);
                if span.start < clause.end && span.end > clause.start {
                    // The import clause itself is not a use of its spelling.
                } else if !is_nested_qualified_identifier_node(node) {
                    match qualified_identifier_matches_spelling_with_budget(
                        node,
                        &document.source,
                        spelling,
                        cancel,
                        budget,
                    )? {
                        Some(true) => return Ok(true),
                        Some(false) => {}
                        None => return Ok(true),
                    }
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                budget.require_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
                pending.push(child);
            }
        }
        Ok(false)
    }

    /// Return source-backed ordering facts while charging the caller's
    /// request-wide assistance budget.
    pub(crate) fn unit_order_safety_with_budget(
        &self,
        uri: &Url,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<UnitOrderSafety>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        budget.require_bytes(document.source.len(), cancel)?;
        budget.require_work(document.helpers.len(), cancel)?;
        let mut exported_names = Vec::new();
        for index in &document.exported_symbol_indices {
            budget.require_work(1, cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                return Ok(Some(UnitOrderSafety {
                    complete: false,
                    has_initialization: document.has_initialization,
                    has_finalization: document.has_finalization,
                    has_helpers: !document.helpers.is_empty(),
                    exported_names,
                    dependency_uris: Vec::new(),
                    conditional_fingerprint: document.conditional_context.fingerprint(),
                }));
            };
            if document
                .conditional_unknown_symbols
                .get(*index)
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            budget.require_bytes(symbol.key.len(), cancel)?;
            exported_names.push(canonical_name(&symbol.key));
        }
        budget.require_work(
            exported_names.len().saturating_mul(usize::BITS as usize),
            cancel,
        )?;
        exported_names.sort();
        exported_names.dedup();

        let mut dependency_uris = Vec::new();
        let mut seen_dependencies = HashSet::new();
        let mut complete = document.conditionals.complete
            && document.conditionals.unknown_spans.is_empty()
            && document.parser_recovery_spans.is_empty()
            && document.unknown_imports.is_empty();
        for import in &document.imports {
            budget.require_work(1, cancel)?;
            budget.require_bytes(import.name.len(), cancel)?;
            let Some(provider) = self.import_provider_uri(uri, &import.name) else {
                complete = false;
                continue;
            };
            budget.require_bytes(provider.as_str().len(), cancel)?;
            if seen_dependencies.insert(provider) {
                dependency_uris.push(provider.clone());
            }
        }
        Ok(Some(UnitOrderSafety {
            complete,
            has_initialization: document.has_initialization,
            has_finalization: document.has_finalization,
            has_helpers: !document.helpers.is_empty(),
            exported_names,
            dependency_uris,
            conditional_fingerprint: document.conditional_context.fingerprint(),
        }))
    }

    pub(crate) fn source_text(&self, uri: &Url) -> Option<&str> {
        self.documents
            .get(uri)
            .map(|document| document.source.as_ref())
    }

    /// Find a declaration-owned class method that can safely receive a
    /// generated implementation.
    ///
    /// This is intentionally stricter than ordinary navigation. A method
    /// action needs one exact owner, a supported signature, a complete
    /// declaration/definition proof, and a physical implementation boundary
    /// in the same unit. Any ambiguity is represented as no candidate rather
    /// than as a best-effort edit.
    pub(crate) fn missing_method_implementation_candidates_with_budget(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<MissingMethodImplementationCandidate>, String> {
        check_navigation_cancel(cancel)?;
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(Vec::new());
        };
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Ok(Vec::new());
        };
        if is_ignored_offset(document.tree.root_node(), offset)
            || !is_declaration_identifier(identifier)
        {
            return Ok(Vec::new());
        }
        let identifier_span = Span::from_node(identifier);
        let mut declaration_index = None;
        for (index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if !budget.take_work(1, cancel)? {
                return Ok(Vec::new());
            }
            if symbol.kind == SymbolKind::Routine
                && symbol.origin == Origin::Declaration
                && symbol.span == identifier_span
            {
                declaration_index = Some(index);
                break;
            }
        }
        let Some(declaration_index) = declaration_index else {
            return Ok(Vec::new());
        };
        let declaration = &document.symbols[declaration_index];
        if declaration.owner_type.is_none()
            || declaration.local_only
            || !matches!(
                declaration.region,
                Region::Interface | Region::Implementation
            )
            || declaration.routine_kind == RoutineKind::Operator
            || declaration.routine_directives.abstract_
            || declaration.routine_directives.forward
            || declaration.routine_directives.calling_convention_unknown
            || declaration.routine_key.is_none()
            || declaration.routine_signature.is_none()
            || declaration
                .routine_signature
                .as_deref()
                .is_some_and(|signature| {
                    signature
                        .split(',')
                        .any(|part| part.trim_end().ends_with('?'))
                })
        {
            return Ok(Vec::new());
        }
        if document
            .conditional_unknown_symbols
            .get(declaration_index)
            .copied()
            .unwrap_or(true)
            || document.has_parser_recovery_near(declaration.declaration_span)
        {
            return Ok(Vec::new());
        }

        let declaration_node = match ancestor_routine_declaration(
            identifier,
            declaration.declaration_span,
            cancel,
            budget,
        )? {
            Some(node) => node,
            None => return Ok(Vec::new()),
        };
        if declaration_node.child_by_field_name("assign").is_some()
            || declaration_node
                .child_by_field_name("rttiAttributes")
                .is_some()
            || routine_declaration_is_external(declaration_node)
        {
            return Ok(Vec::new());
        }

        let Some(owner_node) = ancestor_declared_type(declaration_node, cancel, budget)? else {
            return Ok(Vec::new());
        };
        let Some(true) =
            type_node_contains_kind_with_budget(owner_node, "declClass", cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        let Some(false) =
            type_node_contains_kind_with_budget(owner_node, "declHelper", cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        let Some(owner_name) =
            declared_type_qualification(owner_node, document.source.as_ref(), cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        let owner_key = declaration.owner_type.as_deref().unwrap_or_default();
        let mut owner_symbols = Vec::new();
        for (index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if !budget.take_work(1, cancel)? {
                return Ok(Vec::new());
            }
            if symbol.kind == SymbolKind::Type
                && symbol.origin == Origin::Declaration
                && symbol.generic_parameter.is_none()
                && symbol.key == owner_key
                && symbol.declaration_span == Span::from_node(owner_node)
                && symbol.type_kind == TypeKind::Class
            {
                owner_symbols.push((index, symbol));
            }
        }
        if owner_symbols.len() != 1 {
            return Ok(Vec::new());
        }
        let (owner_index, owner_symbol) = owner_symbols[0];
        if !budget.take_work(1, cancel)?
            || document
                .conditional_unknown_symbols
                .get(owner_index)
                .copied()
                .unwrap_or(true)
            || document.has_parser_recovery_near(Span::from_node(owner_node))
            || owner_symbol
                .generic_parameters
                .iter()
                .any(|parameter| parameter.constraint_unsupported)
        {
            return Ok(Vec::new());
        }

        if !method_signature_is_supported(declaration) {
            return Ok(Vec::new());
        }
        let Some(insertion_offset) =
            method_implementation_insertion_offset(document, cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        if document.conditionals.is_unknown_at(insertion_offset)
            || document.has_parser_recovery_near(Span {
                start: insertion_offset,
                end: insertion_offset.saturating_add(1),
            })
        {
            return Ok(Vec::new());
        }

        match method_definition_proof(document, declaration, declaration_index, cancel, budget)? {
            ContractMatch::Yes | ContractMatch::Unknown => return Ok(Vec::new()),
            ContractMatch::No => {}
        }

        let Some(method_name_node) = declaration_name_identifiers(declaration_node)
            .last()
            .copied()
        else {
            return Ok(Vec::new());
        };
        let method_name = document
            .source
            .get(method_name_node.start_byte()..method_name_node.end_byte())
            .unwrap_or_default()
            .to_owned();
        if method_name.is_empty() {
            return Ok(Vec::new());
        }
        let Some(header) = method_implementation_header(
            self,
            declaration,
            declaration_node,
            document.source.as_ref(),
            uri,
            uri,
            &owner_name,
            &GenericSubstitution::empty(),
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        if !budget.take_bytes(header.len(), cancel)? {
            return Ok(Vec::new());
        }
        let declaration_range = Range::new(
            text::offset_to_position(&document.source, declaration.declaration_span.start)
                .ok_or_else(|| "method declaration is not a UTF-16 boundary".to_string())?,
            text::offset_to_position(&document.source, declaration.declaration_span.end)
                .ok_or_else(|| "method declaration is not a UTF-16 boundary".to_string())?,
        );
        let anchor = Range::new(
            text::offset_to_position(&document.source, declaration.span.start)
                .ok_or_else(|| "method name is not a UTF-16 boundary".to_string())?,
            text::offset_to_position(&document.source, declaration.span.end)
                .ok_or_else(|| "method name is not a UTF-16 boundary".to_string())?,
        );
        let title = format!("{owner_name}.{method_name}");
        if owner_name.len() > 256
            || method_name.len() > 256
            || title.len() > 512
            || document.unit_name.len() > 256
        {
            return Ok(Vec::new());
        }
        let mut hasher = DefaultHasher::new();
        declaration.routine_key.hash(&mut hasher);
        declaration.routine_kind.hash(&mut hasher);
        declaration.is_static.hash(&mut hasher);
        declaration.routine_signature.hash(&mut hasher);
        declaration.result_type_ref.hash(&mut hasher);
        declaration.generic_parameters.hash(&mut hasher);
        owner_name.hash(&mut hasher);
        method_name.hash(&mut hasher);
        header.hash(&mut hasher);
        declaration.declaration_span.hash(&mut hasher);
        let identity = hasher.finish();

        Ok(vec![MissingMethodImplementationCandidate {
            anchor,
            declaration: declaration_range,
            owner: owner_name.to_owned(),
            method: method_name,
            unit_name: document.unit_name.clone(),
            header,
            insertion_offset,
            identity,
        }])
    }

    /// Find interface obligations for the concrete class selected at
    /// `position` and retain only obligations for which a safe declaration and
    /// implementation edit can be described. The contract traversal is the
    /// same bounded proof used by missing-interface diagnostics.
    pub(crate) fn missing_interface_method_implementation_candidates_with_budget(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<MissingInterfaceMethodImplementationCandidate>, String> {
        check_navigation_cancel(cancel)?;
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(Vec::new());
        };
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Ok(Vec::new());
        };
        if is_ignored_offset(document.tree.root_node(), offset)
            || !is_declaration_identifier(identifier)
        {
            return Ok(Vec::new());
        }
        let identifier_span = Span::from_node(identifier);
        let mut class_symbol_index = None;
        for (index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if !budget.take_work(1, cancel)? {
                return Ok(Vec::new());
            }
            if symbol.kind == SymbolKind::Type
                && symbol.origin == Origin::Declaration
                && symbol.type_kind == TypeKind::Class
                && symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
                && symbol.span == identifier_span
                && class_symbol_index.replace(index).is_some()
            {
                return Ok(Vec::new());
            }
        }
        let Some(class_symbol_index) = class_symbol_index else {
            return Ok(Vec::new());
        };
        if document
            .conditional_unknown_symbols
            .get(class_symbol_index)
            .copied()
            .unwrap_or(true)
        {
            return Ok(Vec::new());
        }
        let class_symbol = &document.symbols[class_symbol_index];
        let Some(class_instance) = self.contract_type_instance(uri, &class_symbol.key) else {
            return Ok(Vec::new());
        };
        if !contract_substitution_is_complete(&class_instance) {
            return Ok(Vec::new());
        }
        let Some(class_declaration_node) = declared_type_node_for_span_with_budget(
            document.tree.root_node(),
            identifier_span,
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        let Some(class_type_node) = class_declaration_node.child_by_field_name("type") else {
            return Ok(Vec::new());
        };
        if class_type_node.kind() != "declClass"
            || class_type_node
                .children(&mut class_type_node.walk())
                .any(|child| child.kind() == "declHelper")
        {
            return Ok(Vec::new());
        }
        let Some(owner_name) = declared_type_qualification(
            class_declaration_node,
            document.source.as_ref(),
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        let Some(class_declaration_range) =
            range_for_span(&document.source, class_symbol.declaration_span)
        else {
            return Ok(Vec::new());
        };
        let mut ancestry = ContractAncestryState::new();
        let Some((missing, class_surface)) = self
            .missing_interface_contracts_for_class_with_budget(
                &class_instance,
                &mut ancestry,
                cancel,
                budget,
            )?
        else {
            return Ok(Vec::new());
        };
        let mut candidates = Vec::new();
        for obligation in missing {
            check_navigation_cancel(cancel)?;
            let Some(interface_method_symbol) = self.symbol(&obligation.requirement.candidate)
            else {
                continue;
            };
            if !method_signature_is_supported(interface_method_symbol) {
                continue;
            }
            let Some(interface_document) =
                self.documents.get(&obligation.requirement.candidate.uri)
            else {
                continue;
            };
            let Some(interface_node) = routine_declaration_node_for_symbol(
                interface_document,
                interface_method_symbol,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            if interface_method_symbol.routine_directives.abstract_
                || interface_method_symbol.routine_directives.forward
                || interface_method_symbol.unresolved_abbreviated
                || routine_declaration_is_external(interface_node)
            {
                continue;
            }
            let Some(interface_header) = method_implementation_header(
                self,
                interface_method_symbol,
                interface_node,
                interface_document.source.as_ref(),
                &obligation.requirement.candidate.uri,
                uri,
                &owner_name,
                &obligation.requirement.substitution,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            let method_name = if obligation.method_name == interface_method_symbol.key {
                interface_method_symbol.name.clone()
            } else {
                obligation.method_name.clone()
            };
            let Some(implementation_header) =
                replace_method_header_name(&interface_header, &owner_name, &method_name)
            else {
                continue;
            };

            let direct_declaration = self.direct_interface_method_declaration(
                &obligation,
                &class_instance,
                cancel,
                budget,
            )?;
            if matches!(
                direct_declaration,
                DirectInterfaceMethodDeclaration::Missing
            ) && obligation.implementation != ContractMatch::No
            {
                continue;
            }
            let (declaration_header, declaration_insert, implementation_header) =
                match direct_declaration {
                    DirectInterfaceMethodDeclaration::Missing => {
                        if !self.interface_method_name_collision_is_safe(
                            &obligation,
                            &class_surface,
                            &method_name,
                            cancel,
                            budget,
                        )? {
                            continue;
                        }
                        let Some(declaration_header) = method_declaration_header(
                            self,
                            interface_method_symbol,
                            interface_node,
                            interface_document.source.as_ref(),
                            &obligation.requirement.candidate.uri,
                            uri,
                            &method_name,
                            &obligation.requirement.substitution,
                            cancel,
                            budget,
                        )?
                        else {
                            continue;
                        };
                        let Some(insert) = class_member_insertion(
                            class_type_node,
                            document.source.as_ref(),
                            cancel,
                            budget,
                        )?
                        else {
                            continue;
                        };
                        (
                            Some(declaration_header),
                            Some(insert),
                            implementation_header,
                        )
                    }
                    DirectInterfaceMethodDeclaration::Present { index } => {
                        let Some(symbol) = document.symbols.get(index) else {
                            continue;
                        };
                        // An interface body action must be authorized by the
                        // actual direct declaration selected here.  The
                        // aggregate class status may be Yes because an
                        // unrelated inherited routine matched the obligation;
                        // that result cannot make a private/protected direct
                        // shadow an eligible interface implementation.  Keep
                        // ordinary Task 30 declaration-selected actions
                        // independent: this access gate is only on the
                        // interface-action path.
                        if !matches!(
                            symbol.visibility,
                            Visibility::Public | Visibility::Published
                        ) {
                            continue;
                        }
                        if obligation.implementation == ContractMatch::Unknown
                            && !obligation.unknown_only_because_of_open_compiler_root
                        {
                            continue;
                        }
                        match method_definition_proof(document, symbol, index, cancel, budget)? {
                            ContractMatch::No => {}
                            ContractMatch::Yes | ContractMatch::Unknown => continue,
                        }
                        let Some(node) =
                            routine_declaration_node_for_symbol(document, symbol, cancel, budget)?
                        else {
                            continue;
                        };
                        if symbol.routine_directives.abstract_
                            || symbol.routine_directives.forward
                            || symbol.unresolved_abbreviated
                            || routine_declaration_is_external(node)
                        {
                            continue;
                        }
                        let Some(header) = method_implementation_header(
                            self,
                            symbol,
                            node,
                            document.source.as_ref(),
                            uri,
                            uri,
                            &owner_name,
                            &class_instance.substitution,
                            cancel,
                            budget,
                        )?
                        else {
                            continue;
                        };
                        (None, None, header)
                    }
                    DirectInterfaceMethodDeclaration::Ambiguous => continue,
                };
            let (
                declaration_insert_start,
                declaration_insert_end,
                declaration_indent,
                declaration_owner_indent,
                declaration_add_public,
            ) = declaration_insert
                .map(|insert| {
                    (
                        Some(insert.start),
                        Some(insert.end),
                        Some(insert.indent),
                        Some(insert.owner_indent),
                        insert.add_public,
                    )
                })
                .unwrap_or((None, None, None, None, false));
            let Some(method_name_node) =
                declaration_name_identifiers(interface_node).last().copied()
            else {
                continue;
            };
            let interface_method_name = interface_document
                .source
                .get(method_name_node.start_byte()..method_name_node.end_byte())
                .unwrap_or_default()
                .to_owned();
            if interface_method_name.is_empty() {
                continue;
            }
            let mut obligation_hasher = DefaultHasher::new();
            obligation.identity.hash(&mut obligation_hasher);
            let obligation_identity = obligation_hasher.finish();
            let mut hasher = DefaultHasher::new();
            obligation.identity.hash(&mut hasher);
            obligation.requirement.candidate.hash(&mut hasher);
            obligation.requirement.substitution.hash(&mut hasher);
            obligation.requirement.interface.hash(&mut hasher);
            class_symbol.key.hash(&mut hasher);
            class_symbol.declaration_span.hash(&mut hasher);
            owner_name.hash(&mut hasher);
            method_name.hash(&mut hasher);
            interface_method_name.hash(&mut hasher);
            declaration_header.hash(&mut hasher);
            declaration_insert_start.hash(&mut hasher);
            declaration_insert_end.hash(&mut hasher);
            implementation_header.hash(&mut hasher);
            let identity = hasher.finish();
            candidates.push(MissingInterfaceMethodImplementationCandidate {
                anchor: Range::new(
                    text::offset_to_position(&document.source, class_symbol.span.start)
                        .ok_or_else(|| "class name is not a UTF-16 boundary".to_string())?,
                    text::offset_to_position(&document.source, class_symbol.span.end)
                        .ok_or_else(|| "class name is not a UTF-16 boundary".to_string())?,
                ),
                class_declaration: class_declaration_range,
                owner: owner_name.clone(),
                method: method_name,
                interface_uri: obligation.requirement.candidate.uri.clone(),
                interface_owner: obligation.requirement.interface.key.clone(),
                interface_method: interface_method_name,
                unit_name: document.unit_name.clone(),
                declaration_header,
                declaration_insert_start,
                declaration_insert_end,
                declaration_indent,
                declaration_owner_indent,
                declaration_add_public,
                implementation_header,
                implementation_insertion_offset: method_implementation_insertion_offset(
                    document, cancel, budget,
                )?
                .ok_or_else(|| "implementation insertion point is unavailable".to_string())?,
                obligation_identity,
                identity,
            });
        }
        Ok(candidates)
    }

    /// Bind a document's imports to the workspace-selected unit documents.
    ///
    /// Supplying an empty iterator is intentional: once a workspace owns an
    /// index, an unresolved import must not fall back to a same-named unit
    /// contributed by another project. A plain `NavigationIndex` remains
    /// global and implicit when this method has not been called for a document.
    pub fn bind_imports<I>(&mut self, uri: &Url, bindings: I)
    where
        I: IntoIterator<Item = (String, Url)>,
    {
        let bindings = bindings
            .into_iter()
            .map(|(name, uri)| (canonical_name(&name), uri))
            .collect();
        if let Some(document) = self.documents.get_mut(uri) {
            document.import_binding_fingerprint = Some(import_binding_fingerprint(
                &document.conditional_context,
                &bindings,
            ));
            document.import_bindings = Some(bindings);
        }
    }

    /// Clear a document's resolved imports while retaining an explicit
    /// workspace-owned empty binding map.
    pub fn clear_import_bindings(&mut self, uri: &Url) {
        if let Some(document) = self.documents.get_mut(uri) {
            document.import_binding_fingerprint = Some(import_binding_fingerprint(
                &document.conditional_context,
                &HashMap::new(),
            ));
            document.import_bindings = Some(HashMap::new());
        }
    }

    pub(crate) fn rename_bound_unit_provider(
        &mut self,
        old_uri: &Url,
        new_uri: &Url,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), String> {
        let old_key = canonical_name(old_name);
        let new_key = canonical_name(new_name);
        for document in self.documents.values() {
            let Some(bindings) = document.import_bindings.as_ref() else {
                continue;
            };
            for (key, uri) in bindings {
                if uri == old_uri && key != &old_key {
                    return Err(
                        "unit rename cannot prove a project or namespace alias binding".to_string(),
                    );
                }
                if key == &new_key && uri != old_uri && uri != new_uri {
                    return Err(
                        "unit rename target collides with an existing project alias".to_string()
                    );
                }
            }
        }
        for document in self.documents.values_mut() {
            let conditional_context = document.conditional_context.clone();
            let Some(bindings) = document.import_bindings.as_mut() else {
                continue;
            };
            let Some(uri) = bindings.remove(&old_key) else {
                continue;
            };
            if &uri == old_uri {
                bindings.insert(new_key.clone(), new_uri.clone());
            } else {
                bindings.insert(old_key.clone(), uri);
            }
            document.import_binding_fingerprint =
                Some(import_binding_fingerprint(&conditional_context, bindings));
        }
        Ok(())
    }

    /// Return the unit name declared by a parsed document.
    pub fn unit_name(&self, uri: &Url) -> Option<String> {
        self.documents
            .get(uri)
            .map(|document| document.unit_name.clone())
    }

    pub(crate) fn unit_declaration_position(&self, uri: &Url) -> Option<lsp_types::Position> {
        let document = self.documents.get(uri)?;
        let declarations = identifier_nodes(document.tree.root_node())
            .into_iter()
            .filter(|node| is_unit_declaration_identifier(*node))
            .collect::<Vec<_>>();
        if declarations.len() != 1 {
            return None;
        }
        text::offset_to_position(&document.source, declarations[0].start_byte())
    }

    pub(crate) fn unit_display_name(&self, uri: &Url) -> Option<String> {
        self.documents
            .get(uri)
            .map(|document| document.unit_display_name.clone())
    }

    /// Return whether this URI has a parsed document in the index.
    pub fn contains(&self, uri: &Url) -> bool {
        self.documents.contains_key(uri)
    }

    pub(crate) fn conditional_analysis(&self, uri: &Url) -> Option<&ConditionalAnalysis> {
        self.documents
            .get(uri)
            .map(|document| &document.conditionals)
    }

    pub(crate) fn conditional_unknown_at(&self, uri: &Url, offset: usize) -> bool {
        self.conditional_analysis(uri)
            .is_some_and(|analysis| analysis.is_unknown_at(offset))
    }

    pub(crate) fn conditional_unknown_contains_identifier(&self, uri: &Url, name: &str) -> bool {
        let Some(document) = self.documents.get(uri) else {
            return false;
        };
        document
            .conditionals
            .unknown_contains_identifier(&document.source, name)
    }

    fn candidate_is_conditionally_unknown(&self, candidate: &Candidate) -> bool {
        let Some(document) = self.documents.get(&candidate.uri) else {
            return true;
        };
        document
            .conditional_unknown_symbols
            .get(candidate.index)
            .copied()
            .unwrap_or(true)
    }

    fn candidate_access_decision(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        candidate: &Candidate,
        state: &mut ResolutionState,
    ) -> AccessDecision {
        let Some(document) = self.documents.get(&candidate.uri) else {
            return AccessDecision::Inaccessible;
        };
        let Some(symbol) = document.symbols.get(candidate.index) else {
            return AccessDecision::Inaccessible;
        };
        if !symbol_is_available_at(document, symbol, &candidate.uri, current_uri, offset) {
            return AccessDecision::Inaccessible;
        }
        let Some(owner_type) = symbol.owner_type.as_deref() else {
            return AccessDecision::Visible;
        };
        match symbol.visibility {
            Visibility::Public | Visibility::Published => AccessDecision::Visible,
            Visibility::Private | Visibility::Protected if candidate.uri == *current_uri => {
                AccessDecision::Visible
            }
            Visibility::Private => AccessDecision::Inaccessible,
            Visibility::StrictPrivate => match current_document.owner_type_at(offset) {
                Some(current_owner)
                    if candidate.uri == *current_uri && current_owner == owner_type =>
                {
                    AccessDecision::Visible
                }
                Some(_) => AccessDecision::Inaccessible,
                None => AccessDecision::Inaccessible,
            },
            Visibility::StrictProtected | Visibility::Protected => self.descendant_access_decision(
                current_uri,
                current_document,
                offset,
                candidate,
                owner_type,
                state,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_access_decision_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        candidate: &Candidate,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<AccessDecision, String> {
        let Some(document) = self.documents.get(&candidate.uri) else {
            return Ok(AccessDecision::Inaccessible);
        };
        let Some(symbol) = document.symbols.get(candidate.index) else {
            return Ok(AccessDecision::Inaccessible);
        };
        if !symbol_is_available_at(document, symbol, &candidate.uri, current_uri, offset) {
            return Ok(AccessDecision::Inaccessible);
        }
        let Some(owner_type) = symbol.owner_type.as_deref() else {
            return Ok(AccessDecision::Visible);
        };
        match symbol.visibility {
            Visibility::Public | Visibility::Published => Ok(AccessDecision::Visible),
            Visibility::Private | Visibility::Protected if candidate.uri == *current_uri => {
                Ok(AccessDecision::Visible)
            }
            Visibility::Private => Ok(AccessDecision::Inaccessible),
            Visibility::StrictPrivate => match current_document.owner_type_at(offset) {
                Some(current_owner)
                    if candidate.uri == *current_uri && current_owner == owner_type =>
                {
                    Ok(AccessDecision::Visible)
                }
                Some(_) => Ok(AccessDecision::Inaccessible),
                None => Ok(AccessDecision::Inaccessible),
            },
            Visibility::StrictProtected | Visibility::Protected => self
                .descendant_access_decision_with_budget(
                    current_uri,
                    current_document,
                    offset,
                    candidate,
                    owner_type,
                    state,
                    cancel,
                    budget,
                ),
        }
    }

    fn descendant_access_decision(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        candidate: &Candidate,
        owner_type: &str,
        state: &mut ResolutionState,
    ) -> AccessDecision {
        let Some(current_owner) = current_document.owner_type_at(offset) else {
            return AccessDecision::Inaccessible;
        };
        let current_substitution = self
            .owner_type_substitution(current_uri, &current_owner)
            .unwrap_or_else(GenericSubstitution::empty);
        let uncertain_before = state.receiver_resolution_uncertain();
        if self
            .member_owner_substitution(
                current_uri,
                &current_owner,
                &current_substitution,
                &candidate.uri,
                owner_type,
                state,
            )
            .is_some()
        {
            return AccessDecision::Visible;
        }
        if !uncertain_before && state.receiver_resolution_uncertain() {
            return AccessDecision::Unknown;
        }
        let mut ancestry_state = AncestryResolutionState::new();
        let status = self
            .resolve_type_ancestry(current_uri, &current_owner, &mut ancestry_state)
            .status;
        if status == AncestryStatus::Unknown {
            AccessDecision::Unknown
        } else {
            AccessDecision::Inaccessible
        }
    }

    fn owner_type_substitution(
        &self,
        current_uri: &Url,
        owner_type: &str,
    ) -> Option<GenericSubstitution> {
        let document = self.documents.get(current_uri)?;
        let indices = document.type_symbol_indices.get(owner_type)?;
        if indices.len() != 1 {
            return None;
        }
        let symbol = document.symbols.get(*indices.first()?)?;
        Some(symbolic_generic_substitution(
            current_uri,
            &symbol.generic_parameters,
            symbol.scope,
        ))
    }

    fn owner_type_substitution_with_budget(
        &self,
        current_uri: &Url,
        owner_type: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        budget.require_work(1, cancel)?;
        budget.require_bytes(
            current_uri.as_str().len().saturating_add(owner_type.len()),
            cancel,
        )?;
        let substitution = self.owner_type_substitution(current_uri, owner_type);
        if let Some(substitution) = &substitution {
            budget.require_work(substitution.0.len(), cancel)?;
            budget.require_bytes(
                substitution.0.keys().map(String::len).sum::<usize>(),
                cancel,
            )?;
        }
        Ok(substitution)
    }

    #[allow(clippy::too_many_arguments)]
    fn descendant_access_decision_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        candidate: &Candidate,
        owner_type: &str,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<AccessDecision, String> {
        let Some(current_owner) = current_document.owner_type_at(offset) else {
            return Ok(AccessDecision::Inaccessible);
        };
        let current_substitution = self
            .owner_type_substitution_with_budget(current_uri, &current_owner, cancel, budget)?
            .unwrap_or_else(GenericSubstitution::empty);
        let uncertain_before = state.receiver_resolution_uncertain();
        if self
            .member_owner_substitution_with_budget(
                current_uri,
                &current_owner,
                &current_substitution,
                &candidate.uri,
                owner_type,
                state,
                cancel,
                budget,
            )?
            .is_some()
        {
            return Ok(AccessDecision::Visible);
        }
        if !uncertain_before && state.receiver_resolution_uncertain() {
            return Ok(AccessDecision::Unknown);
        }
        let mut ancestry_state = AncestryResolutionState::new();
        let status = self
            .resolve_type_ancestry_with_budget(
                current_uri,
                &current_owner,
                &mut ancestry_state,
                cancel,
                budget,
            )?
            .status;
        Ok(if status == AncestryStatus::Unknown {
            AccessDecision::Unknown
        } else {
            AccessDecision::Inaccessible
        })
    }

    fn filter_accessible_candidates_with_state(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        candidates: Vec<Candidate>,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let mut result = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            match self.candidate_access_decision(
                current_uri,
                current_document,
                offset,
                &candidate,
                state,
            ) {
                AccessDecision::Visible => result.push(candidate),
                AccessDecision::Unknown => state.mark_receiver_uncertain(),
                AccessDecision::Inaccessible => state.mark_inaccessible_candidate(),
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn filter_accessible_candidates_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        candidates: Vec<Candidate>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_work(candidates.len(), cancel)?;
        let mut result = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            check_navigation_cancel(cancel)?;
            match self.candidate_access_decision_with_budget(
                current_uri,
                current_document,
                offset,
                &candidate,
                state,
                cancel,
                budget,
            )? {
                AccessDecision::Visible => result.push(candidate),
                AccessDecision::Unknown => state.mark_receiver_uncertain(),
                AccessDecision::Inaccessible => state.mark_inaccessible_candidate(),
            }
        }
        Ok(result)
    }

    pub(crate) fn position_is_ignored_or_empty(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<bool, String> {
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(true);
        };
        Ok(is_ignored_offset(document.tree.root_node(), offset)
            || identifier_at(document.tree.root_node(), offset).is_none())
    }

    /// Return only source-semantic diagnostics whose absence was proven by the
    /// bounded navigation model.
    ///
    /// This intentionally reports no result when the syntax tree, imports,
    /// receiver, ancestry, helper selection, accessibility, or traversal
    /// budget is uncertain.  It is therefore safe to use as an additive
    /// diagnostic pass without turning compiler-dependent Pascal semantics
    /// into false positives.
    pub(crate) fn semantic_diagnostics_with_cancel(
        &self,
        uri: &Url,
        cancel: &AtomicBool,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Err(format!("document is not indexed: {uri}"));
        };
        if !document.parser_recovery_spans.is_empty() {
            return Ok(Vec::new());
        }

        let mut budget = AssistanceBudget::new(
            MAX_SEMANTIC_DIAGNOSTIC_WORK,
            MAX_SEMANTIC_DIAGNOSTIC_BYTES,
            "semantic diagnostics",
        );
        let mut pending = vec![document.tree.root_node()];
        let mut identifiers = Vec::new();
        let mut assignments = Vec::new();
        let mut calls = Vec::new();
        let mut visited = 0usize;
        while let Some(node) = pending.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err("request cancelled".to_string());
            }
            visited = visited.saturating_add(1);
            if visited > MAX_SEMANTIC_DIAGNOSTIC_NODES {
                return Ok(Vec::new());
            }
            match budget.require_work(1, cancel) {
                Ok(()) => {}
                Err(error) if error == "request cancelled" => return Err(error),
                Err(_) => return Ok(Vec::new()),
            }
            if node.kind() == "identifier" {
                identifiers.push(node);
                continue;
            }
            if node.kind() == "assignment" {
                assignments.push(node);
            } else if node.kind() == "exprCall" {
                calls.push(node);
            }
            let mut cursor = node.walk();
            pending.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }

        let mut diagnostics = Vec::new();
        for identifier in identifiers {
            if cancel.load(Ordering::Relaxed) {
                return Err("request cancelled".to_string());
            }
            let status = match self.semantic_proof_at_with_budget(
                uri,
                document,
                identifier.start_byte(),
                identifier,
                cancel,
                &mut budget,
            ) {
                Ok(status) => status,
                Err(error) if error == "request cancelled" => return Err(error),
                Err(_) => return Ok(Vec::new()),
            };
            let kind = match status {
                SemanticProofStatus::ProvenAbsent => {
                    if member_expression_at(identifier)
                        .is_some_and(|dot| is_right_hand_member(dot, identifier))
                    {
                        SemanticDiagnosticKind::MissingMember
                    } else {
                        SemanticDiagnosticKind::UnresolvedIdentifier
                    }
                }
                SemanticProofStatus::Resolved
                | SemanticProofStatus::Ambiguous
                | SemanticProofStatus::Incomplete => continue,
            };
            if diagnostics.len() >= MAX_SEMANTIC_DIAGNOSTICS {
                return Ok(Vec::new());
            }
            let name = node_text(identifier, &document.source);
            let message = match kind {
                SemanticDiagnosticKind::UnresolvedIdentifier => {
                    format!("unresolved identifier '{name}'")
                }
                SemanticDiagnosticKind::MissingMember => {
                    format!("missing member '{name}'")
                }
                SemanticDiagnosticKind::TypeMismatch
                | SemanticDiagnosticKind::IncompatibleArgument
                | SemanticDiagnosticKind::InvalidOverride
                | SemanticDiagnosticKind::MissingInterfaceImplementation => continue,
            };
            diagnostics.push(SemanticDiagnostic {
                kind,
                span: SourceSpan {
                    start: identifier.start_byte(),
                    end: identifier.end_byte(),
                },
                message,
            });
        }
        for assignment in assignments {
            if document.conditionals.is_unknown_at(assignment.start_byte())
                || document
                    .opaque_ranges
                    .iter()
                    .any(|range| range.contains(Span::from_node(assignment)))
            {
                continue;
            }
            let Some(operator) = assignment.child_by_field_name("operator") else {
                continue;
            };
            if node_text_with_budget(operator, &document.source, cancel, &mut budget)? != ":=" {
                continue;
            }
            let (Some(lhs), Some(rhs)) = (
                assignment.child_by_field_name("lhs"),
                assignment.child_by_field_name("rhs"),
            ) else {
                continue;
            };
            let mut state = ResolutionState::new();
            let analysis = match overload::analyze_assignment(
                self,
                uri,
                document,
                lhs,
                rhs,
                &mut state,
                0,
                cancel,
                &mut budget,
            ) {
                Ok(analysis) => analysis,
                Err(error) if error == "request cancelled" => return Err(error),
                Err(_) => return Ok(Vec::new()),
            };
            let overload::AssignmentAnalysis::Incompatible { expected, actual } = analysis else {
                continue;
            };
            if diagnostics.len() >= MAX_SEMANTIC_DIAGNOSTICS {
                return Ok(Vec::new());
            }
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::TypeMismatch,
                span: SourceSpan {
                    start: rhs.start_byte(),
                    end: rhs.end_byte(),
                },
                message: format!("type mismatch: cannot assign '{actual}' to '{expected}'"),
            });
        }
        for call in calls {
            if document.conditionals.is_unknown_at(call.start_byte())
                || document
                    .opaque_ranges
                    .iter()
                    .any(|range| range.contains(Span::from_node(call)))
            {
                continue;
            }
            let analysis = match self.semantic_call_analysis_with_budget(
                uri,
                document,
                call,
                cancel,
                &mut budget,
            ) {
                Ok(analysis) => analysis,
                Err(error) if error == "request cancelled" => return Err(error),
                Err(_) => return Ok(Vec::new()),
            };
            let overload::CallAnalysis::Incompatible(mismatches) = analysis else {
                continue;
            };
            for mismatch in mismatches {
                if diagnostics.len() >= MAX_SEMANTIC_DIAGNOSTICS {
                    return Ok(Vec::new());
                }
                diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::IncompatibleArgument,
                    span: SourceSpan {
                        start: mismatch.span.start,
                        end: mismatch.span.end,
                    },
                    message: format!(
                        "incompatible argument: expected '{}', found '{}'",
                        mismatch.expected, mismatch.actual
                    ),
                });
            }
        }
        let contract_diagnostics =
            match self.contract_diagnostics_with_budget(uri, document, cancel, &mut budget) {
                Ok(diagnostics) => diagnostics,
                Err(error) if error == "request cancelled" => return Err(error),
                Err(_) => return Ok(Vec::new()),
            };
        diagnostics.extend(contract_diagnostics);
        diagnostics.sort_by_key(|diagnostic| {
            (
                diagnostic.span.start,
                diagnostic.span.end,
                semantic_diagnostic_kind_rank(diagnostic.kind),
            )
        });
        Ok(diagnostics)
    }

    /// Return stable identities for interface obligations affected by a
    /// source-backed rename.  The identity records the requirement source
    /// declaration and the source implementation(s) that satisfy it, rather
    /// than spelling: an explicit resolution clause may legitimately change
    /// its mapped name while retaining the same implementation identity.
    pub(crate) fn rename_interface_contract_fingerprints_with_budget(
        &self,
        edited_uris: &HashSet<Url>,
        binding_names: &HashSet<String>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<String>, String> {
        let normalized_name_bytes = binding_names.iter().map(|name| name.len()).sum::<usize>();
        budget.require_work(binding_names.len(), cancel)?;
        budget.require_owned_bytes(
            normalized_name_bytes
                .saturating_add(
                    binding_names
                        .len()
                        .saturating_mul(std::mem::size_of::<String>()),
                )
                .saturating_add(
                    binding_names
                        .len()
                        .saturating_mul(2 * std::mem::size_of::<usize>()),
                ),
            cancel,
        )?;
        let mut normalized_names = HashSet::with_capacity(binding_names.len());
        for name in binding_names {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let normalized = canonical_name(name);
            budget.require_owned_bytes(normalized.len(), cancel)?;
            normalized_names.insert(normalized);
        }
        let binding_names = normalized_names;
        let mut class_capacity = 0usize;
        for document in self.documents.values() {
            for symbol in &document.symbols {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if symbol.kind == SymbolKind::Type
                    && symbol.type_kind == TypeKind::Class
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                {
                    class_capacity = class_capacity.saturating_add(1);
                }
            }
        }
        budget.require_owned_bytes(
            class_capacity.saturating_mul(std::mem::size_of::<(Url, String, usize, bool)>()),
            cancel,
        )?;
        let mut class_owners = Vec::<(Url, String, usize, bool)>::with_capacity(class_capacity);
        for (uri, document) in &self.documents {
            for (symbol_index, symbol) in document.symbols.iter().enumerate() {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if symbol.kind != SymbolKind::Type
                    || symbol.type_kind != TypeKind::Class
                    || symbol.owner_type.is_some()
                    || symbol.generic_parameter.is_some()
                {
                    continue;
                }
                let mut has_binding_member = false;
                if let Some(member_indices) =
                    document.member_symbol_indices_by_owner.get(&symbol.key)
                {
                    for member_index in member_indices {
                        check_navigation_cancel(cancel)?;
                        budget.require_work(1, cancel)?;
                        if document.symbols.get(*member_index).is_some_and(|member| {
                            member.kind == SymbolKind::Routine
                                && binding_names.contains(&member.key)
                        }) {
                            has_binding_member = true;
                        }
                    }
                }
                budget.require_owned_bytes(
                    std::mem::size_of::<(Url, String, usize, bool)>()
                        .saturating_add(uri.as_str().len())
                        .saturating_add(symbol.key.len()),
                    cancel,
                )?;
                class_owners.push((
                    uri.clone(),
                    symbol.key.clone(),
                    symbol_index,
                    has_binding_member,
                ));
            }
        }
        let mut affected_owners = Vec::<(Url, String)>::new();
        for (uri, key, _, has_binding_member) in &class_owners {
            if edited_uris.contains(uri) && *has_binding_member {
                budget.require_owned_bytes(
                    std::mem::size_of::<(Url, String)>()
                        .saturating_add(uri.as_str().len())
                        .saturating_add(key.len()),
                    cancel,
                )?;
                affected_owners.push((uri.clone(), key.clone()));
            }
        }
        if affected_owners.is_empty() {
            return Ok(Vec::new());
        }

        budget.require_owned_bytes(
            class_capacity.saturating_mul(std::mem::size_of::<String>()),
            cancel,
        )?;
        let mut fingerprints = Vec::with_capacity(class_capacity);
        let mut ancestry_resolution = AncestryResolutionState::new();
        for (owner_uri, owner_key, symbol_index, direct_member) in &class_owners {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(document) = self.documents.get(owner_uri) else {
                return Err("rename interface contract owner disappeared".to_string());
            };
            let Some(_symbol) = document.symbols.get(*symbol_index) else {
                return Err("rename interface contract owner disappeared".to_string());
            };
            let mut related = *direct_member;
            for target in &affected_owners {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if related {
                    break;
                }
                if owner_uri == &target.0 && owner_key == &target.1 {
                    related = true;
                    break;
                }
                let resolution = self.resolve_type_ancestry_with_budget(
                    owner_uri,
                    owner_key,
                    &mut ancestry_resolution,
                    cancel,
                    budget,
                )?;
                if resolution.status == AncestryStatus::Complete
                    && type_ancestry_contains(
                        self,
                        &resolution.parents,
                        target,
                        cancel,
                        budget,
                        &mut ancestry_resolution,
                    )?
                {
                    related = true;
                }
            }
            if !related {
                continue;
            }
            let Some(class_instance) = self.contract_type_instance(owner_uri, owner_key) else {
                return Err("rename interface contract owner disappeared".to_string());
            };
            if !contract_substitution_is_complete(&class_instance) {
                return Err("rename interface contract substitution is incomplete".to_string());
            }
            let mut ancestry = ContractAncestryState::new();
            let Some((obligations, surface)) = self
                .missing_interface_contracts_for_class_with_budget(
                    &class_instance,
                    &mut ancestry,
                    cancel,
                    budget,
                )?
            else {
                return Err("rename interface contract ancestry is unknown".to_string());
            };
            for obligation in obligations {
                check_navigation_cancel(cancel)?;
                let Some(requirement_symbol) = self.symbol(&obligation.requirement.candidate)
                else {
                    return Err("rename interface requirement disappeared".to_string());
                };
                let requirement_id = self.source_symbol_fingerprint_with_budget(
                    &obligation.requirement.candidate.uri,
                    requirement_symbol,
                    &binding_names,
                    cancel,
                    budget,
                )?;
                budget.require_owned_bytes(
                    surface
                        .routines
                        .len()
                        .saturating_mul(std::mem::size_of::<String>()),
                    cancel,
                )?;
                let mut matches = Vec::with_capacity(surface.routines.len());
                let mut unknown = false;
                for candidate in surface
                    .routines
                    .iter()
                    .filter(|candidate| candidate.symbol_key == obligation.method_name)
                {
                    check_navigation_cancel(cancel)?;
                    budget.require_work(1, cancel)?;
                    if candidate.conditional_unknown(self) {
                        unknown = true;
                        continue;
                    }
                    let Some(candidate_symbol) = self.symbol(&candidate.candidate) else {
                        unknown = true;
                        continue;
                    };
                    if !matches!(
                        candidate_symbol.visibility,
                        Visibility::Public | Visibility::Published
                    ) {
                        unknown = true;
                        continue;
                    }
                    match self.routines_contract_match(
                        requirement_symbol,
                        &obligation.requirement.candidate.uri,
                        &obligation.requirement.substitution,
                        candidate_symbol,
                        &candidate.candidate.uri,
                        &candidate.substitution,
                        cancel,
                        budget,
                    )? {
                        ContractMatch::Yes if candidate_symbol.routine_directives.abstract_ => {
                            unknown = true;
                        }
                        ContractMatch::Yes => {
                            matches.push(self.source_symbol_fingerprint_with_budget(
                                &candidate.candidate.uri,
                                candidate_symbol,
                                &binding_names,
                                cancel,
                                budget,
                            )?)
                        }
                        ContractMatch::No => {}
                        ContractMatch::Unknown => unknown = true,
                    }
                }
                matches.sort();
                let status = if unknown {
                    "unknown"
                } else if matches.is_empty() {
                    "missing"
                } else {
                    "implemented"
                };
                budget.require_owned_bytes(
                    std::mem::size_of::<String>()
                        .saturating_add(owner_uri.as_str().len())
                        .saturating_add(owner_key.len())
                        .saturating_add(requirement_id.len())
                        .saturating_add(status.len())
                        .saturating_add(
                            matches
                                .iter()
                                .map(|matched| matched.len().saturating_add(4))
                                .sum::<usize>(),
                        )
                        .saturating_add(32),
                    cancel,
                )?;
                fingerprints.push(format!(
                    "{}|{}|{}|{}",
                    owner_uri,
                    owner_key,
                    requirement_id,
                    format_args!("{status}:{matches:?}")
                ));
            }
        }
        fingerprints.sort();
        Ok(fingerprints)
    }

    /// Return the contract-proven interface obligations for one concrete
    /// class. `None` means that the class contract is incomplete or otherwise
    /// uncertain; an empty list is a complete class with no missing
    /// obligations. The returned class surface is the same ancestry-aware
    /// member surface used for collision analysis. Both diagnostics and code
    /// actions use this proof so they cannot drift into separate, weaker
    /// interface checkers.
    #[allow(clippy::too_many_arguments)]
    fn missing_interface_contracts_for_class_with_budget(
        &self,
        class_instance: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<(Vec<ContractMissingInterfaceMethod>, ContractClassSurface)>, String> {
        let class_resolution =
            self.resolve_contract_type_with_budget(class_instance, ancestry, cancel, budget)?;
        if class_resolution.status == AncestryStatus::Unknown {
            return Ok(None);
        }

        let inherited_from_abstract_parent = if class_resolution.interfaces.is_empty() {
            class_resolution
                .superclass
                .as_ref()
                .map(|parent| self.contract_type_is_abstract_with_budget(parent, cancel, budget))
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
        if class_resolution.interfaces.is_empty() && !inherited_from_abstract_parent {
            return Ok(Some((Vec::new(), ContractClassSurface::default())));
        }
        if self.contract_type_is_abstract_with_budget(class_instance, cancel, budget)? {
            return Ok(Some((Vec::new(), ContractClassSurface::default())));
        }

        let mut requirements = Vec::new();
        let mut collected_requirements = HashSet::new();
        let mut interfaces = class_resolution.interfaces.clone();
        if let Some(superclass) = class_resolution.superclass.clone() {
            if self
                .resolve_contract_type_with_budget(&superclass, ancestry, cancel, budget)?
                .status
                == AncestryStatus::Unknown
            {
                return Ok(None);
            }
            interfaces.extend(self.contract_interfaces_from_class_with_budget(
                &superclass,
                ancestry,
                cancel,
                budget,
            )?);
        }

        for interface in interfaces {
            let interface_requirements = self
                .collect_interface_requirements_with_budget(&interface, ancestry, cancel, budget)?;
            if interface_requirements.status == AncestryStatus::Unknown {
                return Ok(None);
            }
            budget.require_work(interface_requirements.requirements.len(), cancel)?;
            for requirement in interface_requirements.requirements {
                let identity = (
                    requirement.candidate.clone(),
                    requirement.substitution.clone(),
                );
                if collected_requirements.insert(identity) {
                    requirements.push(requirement);
                }
            }
        }

        let mut surface = ContractClassSurface::default();
        if self.collect_class_surface_with_budget(
            class_instance,
            ancestry,
            &mut surface,
            cancel,
            budget,
        )? == AncestryStatus::Unknown
        {
            return Ok(None);
        }

        let mut missing = Vec::new();
        let mut emitted_requirements = HashSet::new();
        for requirement in requirements {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(identity) = requirement.identity(self, cancel, budget)? else {
                // Deferred identity must be complete before it can authorize
                // either generation or later resolve-time reuse.  Treat the
                // whole class contract as uncertain rather than silently
                // dropping one obligation from a rename proof.
                return Ok(None);
            };
            if self.contract_delegation_status(
                &requirement,
                class_instance,
                &surface.delegations,
                ancestry,
                cancel,
                budget,
            )? != ContractMatch::No
            {
                continue;
            }
            let Some(interface_method) = self.symbol(&requirement.candidate) else {
                continue;
            };
            let method_name = match self
                .contract_method_resolution_name(&requirement, &surface.method_resolutions)
            {
                Ok(Some(name)) => name,
                Ok(None) => interface_method.key.clone(),
                Err(_) => continue,
            };
            // A diamond may replay the same source declaration through more
            // than one interface path.  Those are one obligation when they
            // resolve to the same implementation name, but a method-
            // resolution clause can make the paths materially different.
            // Deduplicate only after the mapped implementation identity is
            // known so two distinct obligations are never merged by spelling
            // alone.
            if !emitted_requirements.insert((identity.clone(), method_name.clone())) {
                continue;
            }
            let mut implementation = self.contract_method_implementation_status(
                &requirement,
                &method_name,
                &surface.routines,
                cancel,
                budget,
            )?;
            let mut unknown_only_because_of_open_compiler_root = false;
            // The compiler-provided TObject is an open member namespace.  An
            // empty source-backed routine list cannot prove that *any*
            // arbitrary requirement is absent, so keep the shared contract
            // result uncertain.  Both semantic diagnostics and declaration
            // generation must suppress the unproven absence claim; an exact
            // direct class declaration may still be used for body-only work.
            if surface.implicit_compiler_root_is_open && implementation == ContractMatch::No {
                implementation = ContractMatch::Unknown;
                unknown_only_because_of_open_compiler_root = true;
            }
            if implementation == ContractMatch::Unknown {
                missing.push(ContractMissingInterfaceMethod {
                    requirement,
                    identity,
                    method_name,
                    implementation,
                    unknown_only_because_of_open_compiler_root,
                });
                continue;
            }
            missing.push(ContractMissingInterfaceMethod {
                requirement,
                identity,
                method_name,
                implementation,
                unknown_only_because_of_open_compiler_root,
            });
        }
        Ok(Some((missing, surface)))
    }

    /// Check source-backed override and interface contracts.  The pass is
    /// deliberately separate from ordinary member lookup because implemented
    /// interfaces are not ordinary class members.  Unknown ancestry,
    /// substitutions, visibility, conditional state, and unsupported
    /// signatures suppress only the affected claim.
    #[allow(clippy::too_many_lines)]
    fn contract_diagnostics_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        let mut diagnostics = Vec::new();
        let mut ancestry = ContractAncestryState::new();
        if !self.contract_document_complete_with_budget(uri, &mut ancestry, cancel, budget)? {
            return Ok(diagnostics);
        }

        let override_symbols = document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.kind == SymbolKind::Routine
                    && symbol.origin == Origin::Declaration
                    && symbol.owner_type.is_some()
                    && symbol.routine_directives.override_
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        budget.require_work(override_symbols.len(), cancel)?;

        for symbol_index in override_symbols {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(symbol_index) else {
                continue;
            };
            if document
                .conditional_unknown_symbols
                .get(symbol_index)
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            let Some(owner_key) = symbol.owner_type.as_deref() else {
                continue;
            };
            let Some(owner_instance) = self.contract_type_instance(uri, owner_key) else {
                continue;
            };
            if owner_instance.kind != TypeKind::Class {
                continue;
            }
            if !contract_substitution_is_complete(&owner_instance) {
                continue;
            }
            if self.contract_routine_signature_status(
                &Candidate {
                    uri: uri.clone(),
                    index: symbol_index,
                },
                &owner_instance.substitution,
                cancel,
                budget,
            )? == ContractMatch::Unknown
            {
                continue;
            }

            let mut inherited = Vec::new();
            if self.collect_superclass_routines_with_budget(
                &owner_instance,
                &mut ancestry,
                &mut inherited,
                cancel,
                budget,
            )? == AncestryStatus::Unknown
            {
                continue;
            }

            let mut exact_virtual = false;
            let mut exact_nonvirtual = false;
            let mut same_name = false;
            let mut overloaded = false;
            let mut uncertain = false;
            for candidate in inherited
                .iter()
                .filter(|candidate| candidate.symbol_key == symbol.key)
            {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                same_name = true;
                if candidate.conditional_unknown(self) {
                    uncertain = true;
                    continue;
                }
                let Some(ancestor_symbol) = self.symbol(&candidate.candidate) else {
                    uncertain = true;
                    continue;
                };
                if ancestor_symbol.routine_directives.overload {
                    overloaded = true;
                }
                if matches!(
                    ancestor_symbol.visibility,
                    Visibility::Private | Visibility::StrictPrivate
                ) {
                    uncertain = true;
                    continue;
                }
                let contract_match = self.routines_contract_match(
                    symbol,
                    uri,
                    &owner_instance.substitution,
                    ancestor_symbol,
                    &candidate.candidate.uri,
                    &candidate.substitution,
                    cancel,
                    budget,
                )?;
                match contract_match {
                    ContractMatch::Yes => {
                        if ancestor_symbol.routine_directives.virtual_
                            || ancestor_symbol.routine_directives.dynamic
                        {
                            exact_virtual = true;
                        } else {
                            exact_nonvirtual = true;
                        }
                    }
                    ContractMatch::No => {}
                    ContractMatch::Unknown => uncertain = true,
                }
            }

            // A proven virtual/dynamic match wins over other overloads.  A
            // private/unsupported/overloaded candidate is not proof of an
            // invalid declaration, even when another candidate is a mismatch.
            if exact_virtual || uncertain || overloaded {
                continue;
            }
            let _ = (same_name, exact_nonvirtual);
            if diagnostics.len() >= MAX_SEMANTIC_DIAGNOSTICS {
                return Err("semantic diagnostics limit".to_owned());
            }
            let name = symbol.name.clone();
            budget.require_bytes(name.len().saturating_mul(2), cancel)?;
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::InvalidOverride,
                span: SourceSpan {
                    start: symbol.span.start,
                    end: symbol.span.end,
                },
                message: format!(
                    "invalid override '{}': no inherited virtual or dynamic method matches",
                    name
                ),
            });
        }

        let class_symbols = document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.kind == SymbolKind::Type
                    && symbol.type_kind == TypeKind::Class
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        budget.require_work(class_symbols.len(), cancel)?;

        for class_symbol_index in class_symbols {
            check_navigation_cancel(cancel)?;
            let Some(class_symbol) = document.symbols.get(class_symbol_index) else {
                continue;
            };
            if document
                .conditional_unknown_symbols
                .get(class_symbol_index)
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            let Some(class_instance) = self.contract_type_instance(uri, &class_symbol.key) else {
                continue;
            };
            if !contract_substitution_is_complete(&class_instance) {
                continue;
            }
            let Some((missing, _surface)) = self
                .missing_interface_contracts_for_class_with_budget(
                    &class_instance,
                    &mut ancestry,
                    cancel,
                    budget,
                )?
            else {
                continue;
            };
            for obligation in missing {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if obligation.implementation != ContractMatch::No {
                    continue;
                }
                let Some(interface_method) = self.symbol(&obligation.requirement.candidate) else {
                    continue;
                };
                if diagnostics.len() >= MAX_SEMANTIC_DIAGNOSTICS {
                    return Err("semantic diagnostics limit".to_owned());
                }
                let class_name = class_symbol.name.clone();
                let method_name = interface_method.name.clone();
                budget.require_bytes(
                    class_name
                        .len()
                        .saturating_add(method_name.len())
                        .saturating_mul(2),
                    cancel,
                )?;
                diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::MissingInterfaceImplementation,
                    span: SourceSpan {
                        start: class_symbol.span.start,
                        end: class_symbol.span.end,
                    },
                    message: format!(
                        "class '{}' does not implement interface method '{}'",
                        class_name, method_name
                    ),
                });
            }
        }

        Ok(diagnostics)
    }

    fn contract_type_instance(&self, uri: &Url, key: &str) -> Option<TypeInstance> {
        let document = self.documents.get(uri)?;
        let indices = document.type_symbol_indices.get(key)?;
        if indices.len() != 1 {
            return None;
        }
        let index = *indices.first()?;
        let symbol = document.symbols.get(index)?;
        if symbol.kind != SymbolKind::Type
            || !matches!(symbol.type_kind, TypeKind::Class | TypeKind::Interface)
        {
            return None;
        }
        Some(type_instance_from_symbol(
            &Candidate {
                uri: uri.clone(),
                index,
            },
            symbol,
        ))
    }

    fn contract_document_complete_with_budget(
        &self,
        uri: &Url,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        if let Some(result) = ancestry.contract_documents.get(uri) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            return Ok(*result);
        }
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !ancestry.active_contract_documents.insert(uri.clone()) {
            // A resolved, source-backed import cycle does not by itself make
            // either unit incomplete.  The active-path guard only prevents
            // recursive re-entry while the surrounding units are checked.
            return Ok(true);
        }
        let result = (|| {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(document) = self.documents.get(uri) else {
                return Ok(false);
            };
            if !document.parser_recovery_spans.is_empty()
                || !document.conditionals.unknown_spans.is_empty()
                || !document.opaque_ranges.is_empty()
            {
                return Ok(false);
            }

            let imports = document
                .interface_uses
                .iter()
                .chain(document.implementation_uses.iter())
                .cloned()
                .collect::<HashSet<_>>();
            budget.require_work(imports.len(), cancel)?;
            for unit in imports {
                check_navigation_cancel(cancel)?;
                let key = canonical_name(&unit);
                if document.unknown_imports.contains(&key) {
                    return Ok(false);
                }
                let urls = self.unit_urls_for_import_with_budget(document, &key, cancel, budget)?;
                if urls.len() != 1 {
                    return Ok(false);
                }
                let Some(provider_uri) = urls.first() else {
                    return Ok(false);
                };
                if !self.contract_document_complete_with_budget(
                    provider_uri,
                    ancestry,
                    cancel,
                    budget,
                )? {
                    return Ok(false);
                }
            }
            Ok(true)
        })();
        ancestry.active_contract_documents.remove(uri);
        if let Ok(result) = &result {
            ancestry.contract_documents.insert(uri.clone(), *result);
        }
        result
    }

    fn authoritative_tobject_instance_with_budget(
        &self,
        owner_uri: &Url,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<TypeInstance>, String> {
        if let Some(document) = self.documents.get(owner_uri) {
            if document
                .type_symbol_indices
                .get("tobject")
                .is_some_and(|indices| !indices.is_empty())
            {
                if !self
                    .contract_document_complete_with_budget(owner_uri, ancestry, cancel, budget)?
                {
                    return Ok(None);
                }
                return self.validated_tobject_instance_in_document(owner_uri, cancel, budget);
            }
        }
        let Some(urls) = self.units.get("system") else {
            // A standalone source file may use Delphi's compiler-provided
            // TObject without a source-backed System unit.  The root itself
            // contributes no interface obligations; keeping a synthetic,
            // source-local root lets us prove the explicit interfaces on the
            // class without inventing any implicit System members.
            return Ok(Some(TypeInstance {
                uri: owner_uri.clone(),
                key: "tobject".to_owned(),
                kind: TypeKind::Class,
                scope: 0,
                parameter_names: Vec::new(),
                substitution: GenericSubstitution::empty(),
                helper_owner: None,
            }));
        };
        if urls.len() != 1 {
            return Ok(None);
        }
        let system_uri = urls[0].clone();
        if !self.contract_document_complete_with_budget(&system_uri, ancestry, cancel, budget)? {
            return Ok(None);
        }
        self.validated_tobject_instance_in_document(&system_uri, cancel, budget)
    }

    fn validated_tobject_instance_in_document(
        &self,
        uri: &Url,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<TypeInstance>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let Some(indices) = document.type_symbol_indices.get("tobject") else {
            return Ok(None);
        };
        if indices.len() != 1 {
            return Ok(None);
        }
        budget.require_work(1, cancel)?;
        let index = indices[0];
        let Some(symbol) = document.symbols.get(index) else {
            return Ok(None);
        };
        let Some(entries) = document.type_ancestry.get("tobject") else {
            return Ok(None);
        };
        if symbol.kind != SymbolKind::Type
            || symbol.type_kind != TypeKind::Class
            || document
                .conditional_unknown_symbols
                .get(index)
                .copied()
                .unwrap_or(true)
            || entries.len() != 1
            || entries[0].kind != TypeKind::Class
            || entries[0].parent_declared
        {
            return Ok(None);
        }
        Ok(Some(type_instance_from_symbol(
            &Candidate {
                uri: uri.clone(),
                index,
            },
            symbol,
        )))
    }

    fn contract_instance_is_authoritative_tobject(
        &self,
        instance: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        Ok(self
            .authoritative_tobject_instance_with_budget(&instance.uri, ancestry, cancel, budget)?
            .is_some_and(|root| root == *instance))
    }

    fn contract_instance_is_synthetic_tobject_with_budget(
        &self,
        instance: &TypeInstance,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if instance.key != "tobject" || instance.kind != TypeKind::Class {
            return Ok(false);
        }
        let Some(document) = self.documents.get(&instance.uri) else {
            return Ok(true);
        };
        Ok(document
            .type_symbol_indices
            .get("tobject")
            .is_none_or(|indices| indices.is_empty()))
    }

    fn resolve_contract_parent_with_budget(
        &self,
        owner: &TypeInstance,
        parent: &ParentType,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<TypeInstance>, String> {
        let Some(type_ref) = parent.type_ref.as_ref() else {
            return Ok(None);
        };
        let Some(document) = self.documents.get(&owner.uri) else {
            return Ok(None);
        };
        let Some(lookup_identifier) = self.contract_lookup_identifier_with_budget(
            document,
            type_ref.span,
            cancel,
            budget,
            "contract ancestry",
        )?
        else {
            return Ok(None);
        };
        let mut resolution_state = ResolutionState::new();
        let receivers = self.type_receivers_for_type_ref_with_budget(
            &owner.uri,
            document,
            type_ref.span.start,
            type_ref,
            lookup_identifier,
            None,
            &owner.substitution,
            &mut resolution_state,
            cancel,
            budget,
        )?;
        if resolution_state.receiver_resolution_uncertain() {
            return Ok(None);
        }
        let no_source_backed_parent = receivers.is_empty();
        let Some(parent_instance) = unique_type_instance(receivers) else {
            if no_source_backed_parent
                && canonical_path(&type_ref.path) == "tobject"
                && type_ref.args.is_empty()
            {
                return self.authoritative_tobject_instance_with_budget(
                    &owner.uri, ancestry, cancel, budget,
                );
            }
            return Ok(None);
        };
        if matches!(parent_instance.kind, TypeKind::Class | TypeKind::Interface) {
            Ok(Some(parent_instance))
        } else {
            Ok(None)
        }
    }

    fn resolve_superclass_with_budget(
        &self,
        instance: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractParentResolution, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if let Some(result) = ancestry.superclasses.get(instance) {
            return Ok(result.clone());
        }
        if !ancestry.active_superclasses.insert(instance.clone()) {
            return Ok(ContractParentResolution::Unknown);
        }
        if ancestry.active_superclasses.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_superclasses.remove(instance);
            return Ok(ContractParentResolution::Unknown);
        }
        let result = (|| {
            if self
                .contract_instance_is_authoritative_tobject(instance, ancestry, cancel, budget)?
            {
                return Ok(ContractParentResolution::Resolved(None));
            }
            if !self.contract_document_complete_with_budget(
                &instance.uri,
                ancestry,
                cancel,
                budget,
            )? {
                return Ok(ContractParentResolution::Unknown);
            }
            let Some(document) = self.documents.get(&instance.uri) else {
                return Ok(ContractParentResolution::Unknown);
            };
            let Some(entries) = document.type_ancestry.get(&instance.key) else {
                return Ok(ContractParentResolution::Unknown);
            };
            if entries.len() != 1 || entries[0].kind != TypeKind::Class {
                return Ok(ContractParentResolution::Unknown);
            }
            let mut superclass = None;
            for parent in entries[0]
                .parents
                .iter()
                .filter(|parent| parent.relation == ParentRelation::Superclass)
            {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                let Some(parent_instance) = self.resolve_contract_parent_with_budget(
                    instance, parent, ancestry, cancel, budget,
                )?
                else {
                    return Ok(ContractParentResolution::Unknown);
                };
                match parent_instance.kind {
                    TypeKind::Class => {
                        if superclass.is_some() {
                            return Ok(ContractParentResolution::Unknown);
                        }
                        superclass = Some(parent_instance);
                    }
                    // Delphi permits an interface in the first parent slot;
                    // it is not a superclass and does not affect override
                    // proof.
                    TypeKind::Interface => {}
                    _ => return Ok(ContractParentResolution::Unknown),
                }
            }
            if superclass.is_none() {
                let Some(root) = self.authoritative_tobject_instance_with_budget(
                    &instance.uri,
                    ancestry,
                    cancel,
                    budget,
                )?
                else {
                    return Ok(ContractParentResolution::Unknown);
                };
                if root == *instance {
                    return Ok(ContractParentResolution::Resolved(None));
                }
                superclass = Some(root);
            }
            Ok(ContractParentResolution::Resolved(superclass))
        })();
        ancestry.active_superclasses.remove(instance);
        if let Ok(result) = &result {
            ancestry
                .superclasses
                .insert(instance.clone(), result.clone());
        }
        result
    }

    fn collect_superclass_routines_with_budget(
        &self,
        instance: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        routines: &mut Vec<ContractRoutineCandidate>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<AncestryStatus, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !ancestry.active_superclass_routines.insert(instance.clone()) {
            return Ok(AncestryStatus::Unknown);
        }
        if ancestry.active_superclass_routines.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_superclass_routines.remove(instance);
            return Ok(AncestryStatus::Unknown);
        }
        let result = (|| {
            // A compiler-provided TObject has unknown virtual members.  It is
            // safe as an empty root for interface obligations, but it cannot
            // prove that an `override` is invalid.
            if self.contract_instance_is_synthetic_tobject_with_budget(instance, cancel, budget)? {
                return Ok(AncestryStatus::Unknown);
            }
            let parent =
                match self.resolve_superclass_with_budget(instance, ancestry, cancel, budget)? {
                    ContractParentResolution::Resolved(parent) => parent,
                    ContractParentResolution::Unknown => return Ok(AncestryStatus::Unknown),
                };
            let Some(parent) = parent else {
                return Ok(AncestryStatus::Complete);
            };
            self.direct_contract_routines(&parent, routines, cancel, budget)?;
            self.collect_superclass_routines_with_budget(
                &parent, ancestry, routines, cancel, budget,
            )
        })();
        ancestry.active_superclass_routines.remove(instance);
        result
    }

    fn resolve_contract_type_with_budget(
        &self,
        instance: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractTypeResolution, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if let Some(result) = ancestry.types.get(instance) {
            return Ok(result.clone());
        }
        if !ancestry.active_types.insert(instance.clone()) {
            return Ok(ContractTypeResolution::unknown());
        }
        if ancestry.active_types.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_types.remove(instance);
            return Ok(ContractTypeResolution::unknown());
        }
        let result = (|| {
            if self
                .contract_instance_is_authoritative_tobject(instance, ancestry, cancel, budget)?
            {
                return Ok(ContractTypeResolution::complete());
            }
            if !self.contract_document_complete_with_budget(
                &instance.uri,
                ancestry,
                cancel,
                budget,
            )? {
                return Ok(ContractTypeResolution::unknown());
            }
            let Some(document) = self.documents.get(&instance.uri) else {
                return Ok(ContractTypeResolution::unknown());
            };
            let Some(indices) = document.type_symbol_indices.get(&instance.key) else {
                return Ok(ContractTypeResolution::unknown());
            };
            if indices.len() != 1 {
                return Ok(ContractTypeResolution::unknown());
            }
            let index = indices[0];
            let Some(type_symbol) = document.symbols.get(index) else {
                return Ok(ContractTypeResolution::unknown());
            };
            if type_symbol.kind != SymbolKind::Type
                || type_symbol.type_kind != instance.kind
                || document
                    .conditional_unknown_symbols
                    .get(index)
                    .copied()
                    .unwrap_or(true)
            {
                return Ok(ContractTypeResolution::unknown());
            }
            let Some(entries) = document.type_ancestry.get(&instance.key) else {
                return Ok(ContractTypeResolution::unknown());
            };
            if entries.len() != 1 {
                return Ok(ContractTypeResolution::unknown());
            }
            let entry = &entries[0];
            if entry.parent_declared && entry.parents.is_empty() {
                return Ok(ContractTypeResolution::unknown());
            }
            let mut result = ContractTypeResolution::complete();
            for parent in &entry.parents {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                let Some(parent_instance) = self.resolve_contract_parent_with_budget(
                    instance, parent, ancestry, cancel, budget,
                )?
                else {
                    return Ok(ContractTypeResolution::unknown());
                };
                match entry.kind {
                    TypeKind::Class if parent_instance.kind == TypeKind::Class => {
                        if result.superclass.is_some() {
                            return Ok(ContractTypeResolution::unknown());
                        }
                        result.superclass = Some(parent_instance);
                    }
                    TypeKind::Class if parent_instance.kind == TypeKind::Interface => {
                        result.interfaces.push(parent_instance);
                    }
                    TypeKind::Interface if parent_instance.kind == TypeKind::Interface => {
                        result.interfaces.push(parent_instance);
                    }
                    _ => return Ok(ContractTypeResolution::unknown()),
                }
            }
            if entry.kind == TypeKind::Class && result.superclass.is_none() {
                let Some(root) = self.authoritative_tobject_instance_with_budget(
                    &instance.uri,
                    ancestry,
                    cancel,
                    budget,
                )?
                else {
                    return Ok(ContractTypeResolution::unknown());
                };
                if root != *instance {
                    result.superclass = Some(root);
                }
            }
            for parent in result.superclass.iter().chain(result.interfaces.iter()) {
                if self
                    .resolve_contract_type_with_budget(parent, ancestry, cancel, budget)?
                    .status
                    == AncestryStatus::Unknown
                {
                    return Ok(ContractTypeResolution::unknown());
                }
            }
            Ok(result)
        })();
        ancestry.active_types.remove(instance);
        if let Ok(result) = &result {
            ancestry.types.insert(instance.clone(), result.clone());
        }
        result
    }

    fn contract_interfaces_from_class_with_budget(
        &self,
        class: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<TypeInstance>, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !ancestry.active_class_interfaces.insert(class.clone()) {
            return Ok(Vec::new());
        }
        if ancestry.active_class_interfaces.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_class_interfaces.remove(class);
            return Ok(Vec::new());
        }
        let result = (|| {
            let resolution =
                self.resolve_contract_type_with_budget(class, ancestry, cancel, budget)?;
            if resolution.status == AncestryStatus::Unknown {
                return Ok(Vec::new());
            }
            let mut interfaces = resolution.interfaces;
            if let Some(superclass) = resolution.superclass {
                interfaces.extend(self.contract_interfaces_from_class_with_budget(
                    &superclass,
                    ancestry,
                    cancel,
                    budget,
                )?);
            }
            Ok(interfaces)
        })();
        ancestry.active_class_interfaces.remove(class);
        result
    }

    fn direct_contract_routines(
        &self,
        instance: &TypeInstance,
        routines: &mut Vec<ContractRoutineCandidate>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<(), String> {
        let Some(document) = self.documents.get(&instance.uri) else {
            return Err("contract document is unavailable".to_owned());
        };
        let indices = document
            .member_symbol_indices_by_owner
            .get(&instance.key)
            .cloned()
            .unwrap_or_default();
        budget.require_work(indices.len(), cancel)?;
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(index) else {
                continue;
            };
            if symbol.kind != SymbolKind::Routine
                || symbol.owner_type.as_deref() != Some(instance.key.as_str())
            {
                continue;
            }
            routines.push(ContractRoutineCandidate {
                candidate: Candidate {
                    uri: instance.uri.clone(),
                    index,
                },
                substitution: instance.substitution.clone(),
                symbol_key: symbol.key.clone(),
            });
        }
        Ok(())
    }

    fn collect_class_surface_with_budget(
        &self,
        instance: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        surface: &mut ContractClassSurface,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<AncestryStatus, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !ancestry.active_class_surfaces.insert(instance.clone()) {
            return Ok(AncestryStatus::Unknown);
        }
        if ancestry.active_class_surfaces.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_class_surfaces.remove(instance);
            return Ok(AncestryStatus::Unknown);
        }
        let result = (|| {
            let resolution =
                self.resolve_contract_type_with_budget(instance, ancestry, cancel, budget)?;
            if resolution.status == AncestryStatus::Unknown {
                return Ok(AncestryStatus::Unknown);
            }
            if self.contract_instance_is_synthetic_tobject_with_budget(instance, cancel, budget)? {
                // A compiler-provided root has no source-backed member list.
                // Its namespace is therefore open: an empty local surface is
                // not evidence that a member name is absent.  Keep that fact
                // on the shared surface so both diagnostics and code-action
                // collision analysis fail closed consistently.
                surface.implicit_compiler_root_is_open = true;
                return Ok(AncestryStatus::Complete);
            }
            self.direct_contract_routines(instance, &mut surface.routines, cancel, budget)?;
            self.direct_contract_nonroutine_names(instance, surface, cancel, budget)?;
            let Some(document) = self.documents.get(&instance.uri) else {
                return Ok(AncestryStatus::Unknown);
            };
            for method_resolution in document
                .method_resolutions
                .iter()
                .filter(|resolution| resolution.class_owner == instance.key)
            {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                surface.method_resolutions.push(method_resolution.clone());
            }
            for delegation in document
                .interface_delegations
                .iter()
                .filter(|delegation| delegation.class_owner == instance.key)
            {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                let mut delegation = delegation.clone();
                delegation.declaring_uri = Some(instance.uri.clone());
                delegation.declaring_substitution = Some(instance.substitution.clone());
                surface.delegations.push(delegation);
            }
            if let Some(superclass) = resolution.superclass {
                self.collect_class_surface_with_budget(
                    &superclass,
                    ancestry,
                    surface,
                    cancel,
                    budget,
                )
            } else {
                Ok(AncestryStatus::Complete)
            }
        })();
        ancestry.active_class_surfaces.remove(instance);
        result
    }

    fn direct_contract_nonroutine_names(
        &self,
        instance: &TypeInstance,
        surface: &mut ContractClassSurface,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<(), String> {
        let Some(document) = self.documents.get(&instance.uri) else {
            return Err("contract document is unavailable".to_owned());
        };
        let indices = document
            .member_symbol_indices_by_owner
            .get(&instance.key)
            .cloned()
            .unwrap_or_default();
        budget.require_work(indices.len(), cancel)?;
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(index) else {
                return Err("contract member disappeared".to_owned());
            };
            if symbol.owner_type.as_deref() == Some(instance.key.as_str())
                && symbol.kind != SymbolKind::Routine
            {
                surface.nonroutine_names.insert(symbol.key.clone());
            }
        }
        Ok(())
    }

    fn collect_interface_requirements_with_budget(
        &self,
        interface: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractInterfaceRequirements, String> {
        #[cfg(test)]
        test_record_contract_interface_visit(cancel);
        let identity = (
            interface.uri.clone(),
            interface.key.clone(),
            interface.substitution.clone(),
        );
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if let Some(status) = ancestry.interface_requirements.get(&identity) {
            return Ok(status.clone());
        }
        if !ancestry.active_interfaces.insert(identity.clone()) {
            return Ok(ContractInterfaceRequirements::unknown());
        }
        if ancestry.active_interfaces.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_interfaces.remove(&identity);
            return Ok(ContractInterfaceRequirements::unknown());
        }
        let result = (|| {
            let resolution =
                self.resolve_contract_type_with_budget(interface, ancestry, cancel, budget)?;
            if resolution.status == AncestryStatus::Unknown || interface.kind != TypeKind::Interface
            {
                return Ok(ContractInterfaceRequirements::unknown());
            }
            let mut requirements = Vec::new();
            let mut seen = HashSet::new();
            for parent in resolution.interfaces {
                let parent_requirements = self.collect_interface_requirements_with_budget(
                    &parent, ancestry, cancel, budget,
                )?;
                if parent_requirements.status == AncestryStatus::Unknown {
                    return Ok(ContractInterfaceRequirements::unknown());
                }
                budget.require_work(parent_requirements.requirements.len(), cancel)?;
                for requirement in parent_requirements.requirements {
                    let requirement_identity = (
                        requirement.candidate.clone(),
                        requirement.substitution.clone(),
                    );
                    if seen.insert(requirement_identity) {
                        requirements.push(requirement);
                    }
                }
            }
            let Some(document) = self.documents.get(&interface.uri) else {
                return Ok(ContractInterfaceRequirements::unknown());
            };
            let indices = document
                .member_symbol_indices_by_owner
                .get(&interface.key)
                .cloned()
                .unwrap_or_default();
            budget.require_work(indices.len().max(1), cancel)?;
            for index in indices {
                check_navigation_cancel(cancel)?;
                let Some(symbol) = document.symbols.get(index) else {
                    continue;
                };
                if symbol.kind != SymbolKind::Routine
                    || symbol.origin != Origin::Declaration
                    || symbol.owner_type.as_deref() != Some(interface.key.as_str())
                {
                    continue;
                }
                if document
                    .conditional_unknown_symbols
                    .get(index)
                    .copied()
                    .unwrap_or(true)
                    || symbol
                        .generic_parameters
                        .iter()
                        .any(|parameter| parameter.constraint_unsupported)
                {
                    return Ok(ContractInterfaceRequirements::unknown());
                }
                if self.contract_routine_signature_status(
                    &Candidate {
                        uri: interface.uri.clone(),
                        index,
                    },
                    &interface.substitution,
                    cancel,
                    budget,
                )? == ContractMatch::Unknown
                {
                    return Ok(ContractInterfaceRequirements::unknown());
                }
                let candidate = Candidate {
                    uri: interface.uri.clone(),
                    index,
                };
                if seen.insert((candidate.clone(), interface.substitution.clone())) {
                    requirements.push(ContractRequirement {
                        candidate,
                        substitution: interface.substitution.clone(),
                        interface: interface.clone(),
                    });
                }
            }
            Ok(ContractInterfaceRequirements::complete(requirements))
        })();
        ancestry.active_interfaces.remove(&identity);
        if let Ok(status) = &result {
            ancestry
                .interface_requirements
                .insert(identity, status.clone());
        }
        result
    }

    fn contract_type_is_abstract_with_budget(
        &self,
        instance: &TypeInstance,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(document) = self.documents.get(&instance.uri) else {
            return Ok(true);
        };
        let Some(entries) = document.type_ancestry.get(&instance.key) else {
            return Ok(true);
        };
        if entries.len() != 1 || entries[0].abstract_ {
            return Ok(true);
        }
        let indices = document
            .member_symbol_indices_by_owner
            .get(&instance.key)
            .cloned()
            .unwrap_or_default();
        budget.require_work(indices.len(), cancel)?;
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(index) else {
                continue;
            };
            if symbol.kind != SymbolKind::Routine {
                continue;
            }
            if document
                .conditional_unknown_symbols
                .get(index)
                .copied()
                .unwrap_or(true)
            {
                return Ok(true);
            }
            if symbol.routine_directives.abstract_ {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn contract_lookup_identifier_with_budget<'a>(
        &self,
        document: &'a Document,
        span: Span,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        operation: &'static str,
    ) -> Result<Option<Node<'a>>, String> {
        let end = span.end.min(span.start.saturating_add(256));
        for offset in span.start..end.max(span.start.saturating_add(1)) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if let Some(identifier) = assistance::context_node_at_with_budget(
                document.tree.root_node(),
                offset,
                cancel,
                budget,
                operation,
            )? {
                return Ok(Some(identifier));
            }
        }
        Ok(None)
    }

    fn contract_type_reference_status(
        &self,
        uri: &Url,
        type_ref: &TypeRef,
        substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(ContractMatch::Unknown);
        };
        let Some(lookup_identifier) = self.contract_lookup_identifier_with_budget(
            document,
            type_ref.span,
            cancel,
            budget,
            "contract signature",
        )?
        else {
            return Ok(ContractMatch::Unknown);
        };
        let mut state = ResolutionState::new();
        let receivers = self.type_receivers_for_type_ref_with_budget(
            uri,
            document,
            type_ref.span.start,
            type_ref,
            lookup_identifier,
            None,
            substitution,
            &mut state,
            cancel,
            budget,
        )?;
        if state.receiver_resolution_uncertain() {
            return Ok(ContractMatch::Unknown);
        }
        Ok(if resolved_type_from_receivers(receivers).is_some() {
            ContractMatch::Yes
        } else {
            ContractMatch::Unknown
        })
    }

    fn contract_routine_signature_status(
        &self,
        candidate: &Candidate,
        substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let Some(symbol) = self.symbol(candidate) else {
            return Ok(ContractMatch::Unknown);
        };
        if !symbol.generic_parameters.is_empty()
            || symbol
                .generic_parameters
                .iter()
                .any(|parameter| parameter.constraint_unsupported)
            || symbol.routine_directives.calling_convention_unknown
        {
            return Ok(ContractMatch::Unknown);
        }
        for parameter in &symbol.routine_parameters {
            if parameter
                .type_shape
                .as_ref()
                .is_some_and(|shape| matches!(shape, TypeShape::Callable | TypeShape::Unknown))
            {
                return Ok(ContractMatch::Unknown);
            }
            let Some(type_ref) = parameter.type_ref.as_ref() else {
                return Ok(ContractMatch::Unknown);
            };
            if self.contract_type_reference_status(
                &candidate.uri,
                type_ref,
                substitution,
                cancel,
                budget,
            )? != ContractMatch::Yes
            {
                return Ok(ContractMatch::Unknown);
            }
        }
        if matches!(
            symbol.routine_kind,
            RoutineKind::Function | RoutineKind::Operator
        ) {
            let Some(result_type) = symbol.result_type_ref.as_ref() else {
                return Ok(ContractMatch::Unknown);
            };
            if self.contract_type_reference_status(
                &candidate.uri,
                result_type,
                substitution,
                cancel,
                budget,
            )? != ContractMatch::Yes
            {
                return Ok(ContractMatch::Unknown);
            }
        }
        Ok(ContractMatch::Yes)
    }

    #[allow(clippy::too_many_arguments)]
    fn routines_contract_match(
        &self,
        left: &Symbol,
        left_uri: &Url,
        left_substitution: &GenericSubstitution,
        right: &Symbol,
        right_uri: &Url,
        right_substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        if left.routine_kind != right.routine_kind || left.is_static != right.is_static {
            return Ok(ContractMatch::No);
        }
        if left.routine_directives.calling_convention_unknown
            || right.routine_directives.calling_convention_unknown
        {
            return Ok(ContractMatch::Unknown);
        }
        match Self::calling_conventions_contract_match(
            left.routine_directives.calling_convention,
            right.routine_directives.calling_convention,
        ) {
            ContractMatch::Yes => {}
            ContractMatch::No => return Ok(ContractMatch::No),
            ContractMatch::Unknown => return Ok(ContractMatch::Unknown),
        }
        // Generic method variance and constraint matching is compiler-specific;
        // retaining it as unknown is safer than claiming either compatibility
        // or absence.
        if !left.generic_parameters.is_empty() || !right.generic_parameters.is_empty() {
            return Ok(ContractMatch::Unknown);
        }
        if left.routine_parameters.len() != right.routine_parameters.len() {
            return Ok(ContractMatch::No);
        }
        for (left_parameter, right_parameter) in left
            .routine_parameters
            .iter()
            .zip(&right.routine_parameters)
        {
            if left_parameter.mode != right_parameter.mode {
                return Ok(ContractMatch::No);
            }
            let parameter_match = self.contract_parameter_type_match(
                left_uri,
                left_parameter.type_ref.as_ref(),
                left_substitution,
                right_uri,
                right_parameter.type_ref.as_ref(),
                right_substitution,
                cancel,
                budget,
            )?;
            if parameter_match != ContractMatch::Yes {
                return Ok(parameter_match);
            }
        }

        match (
            left.routine_kind,
            left.result_type_ref.as_ref(),
            right.result_type_ref.as_ref(),
        ) {
            (RoutineKind::Function | RoutineKind::Operator, Some(left), Some(right)) => self
                .contract_parameter_type_match(
                    left_uri,
                    Some(left),
                    left_substitution,
                    right_uri,
                    Some(right),
                    right_substitution,
                    cancel,
                    budget,
                ),
            (RoutineKind::Function | RoutineKind::Operator, None, None) => {
                Ok(ContractMatch::Unknown)
            }
            (RoutineKind::Function | RoutineKind::Operator, _, _) => Ok(ContractMatch::No),
            (RoutineKind::Procedure | RoutineKind::Constructor | RoutineKind::Destructor, _, _) => {
                Ok(
                    if left.result_type_ref.is_none() == right.result_type_ref.is_none() {
                        ContractMatch::Yes
                    } else {
                        ContractMatch::No
                    },
                )
            }
        }
    }

    fn calling_conventions_contract_match(
        left: Option<CallingConvention>,
        right: Option<CallingConvention>,
    ) -> ContractMatch {
        match (left, right) {
            (None, None) => ContractMatch::Yes,
            (Some(left), Some(right)) if left == right => ContractMatch::Yes,
            (Some(_), Some(_)) => ContractMatch::No,
            // An omitted convention is not itself an ABI.  A project/target
            // context could establish that it means register, but NavigationIndex
            // deliberately does not guess a platform default.  Keep the mixed
            // pair uncertain so it cannot become a false absence proof.
            (None, Some(_)) | (Some(_), None) => ContractMatch::Unknown,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_parameter_type_match(
        &self,
        left_uri: &Url,
        left_type: Option<&TypeRef>,
        left_substitution: &GenericSubstitution,
        right_uri: &Url,
        right_type: Option<&TypeRef>,
        right_substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let (Some(left_type), Some(right_type)) = (left_type, right_type) else {
            return Ok(ContractMatch::Unknown);
        };
        let Some(left_document) = self.documents.get(left_uri) else {
            return Ok(ContractMatch::Unknown);
        };
        let Some(right_document) = self.documents.get(right_uri) else {
            return Ok(ContractMatch::Unknown);
        };
        let Some(left_identifier) = self.contract_lookup_identifier_with_budget(
            left_document,
            left_type.span,
            cancel,
            budget,
            "contract signature",
        )?
        else {
            return Ok(ContractMatch::Unknown);
        };
        let Some(right_identifier) = self.contract_lookup_identifier_with_budget(
            right_document,
            right_type.span,
            cancel,
            budget,
            "contract signature",
        )?
        else {
            return Ok(ContractMatch::Unknown);
        };
        let mut left_state = ResolutionState::new();
        let left_receivers = self.type_receivers_for_type_ref_with_budget(
            left_uri,
            left_document,
            left_type.span.start,
            left_type,
            left_identifier,
            None,
            left_substitution,
            &mut left_state,
            cancel,
            budget,
        )?;
        if left_state.receiver_resolution_uncertain() {
            return Ok(ContractMatch::Unknown);
        }
        let mut right_state = ResolutionState::new();
        let right_receivers = self.type_receivers_for_type_ref_with_budget(
            right_uri,
            right_document,
            right_type.span.start,
            right_type,
            right_identifier,
            None,
            right_substitution,
            &mut right_state,
            cancel,
            budget,
        )?;
        if right_state.receiver_resolution_uncertain() {
            return Ok(ContractMatch::Unknown);
        }
        let (Some(left), Some(right)) = (
            resolved_type_from_receivers(left_receivers),
            resolved_type_from_receivers(right_receivers),
        ) else {
            return Ok(ContractMatch::Unknown);
        };
        let left_identity =
            self.semantic_type_identity_with_budget(&left, cancel, budget, &mut HashSet::new(), 0)?;
        let right_identity = self.semantic_type_identity_with_budget(
            &right,
            cancel,
            budget,
            &mut HashSet::new(),
            0,
        )?;
        Ok(match (left_identity, right_identity) {
            (Some(left), Some(right)) if left == right => ContractMatch::Yes,
            (Some(_), Some(_)) => ContractMatch::No,
            _ => ContractMatch::Unknown,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_type_identity_with_budget(
        &self,
        uri: &Url,
        type_ref: &TypeRef,
        substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<TypeIdentity>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let Some(lookup_identifier) = self.contract_lookup_identifier_with_budget(
            document,
            type_ref.span,
            cancel,
            budget,
            "contract type identity",
        )?
        else {
            return Ok(None);
        };
        let mut state = ResolutionState::new();
        let receivers = self.type_receivers_for_type_ref_with_budget(
            uri,
            document,
            type_ref.span.start,
            type_ref,
            lookup_identifier,
            None,
            substitution,
            &mut state,
            cancel,
            budget,
        )?;
        if state.receiver_resolution_uncertain() {
            return Ok(None);
        }
        let Some(resolved) = resolved_type_from_receivers(receivers) else {
            return Ok(None);
        };
        self.semantic_type_identity_with_budget(&resolved, cancel, budget, &mut HashSet::new(), 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn semantic_type_identity_with_budget(
        &self,
        resolved: &ResolvedType,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        depth: usize,
    ) -> Result<Option<TypeIdentity>, String> {
        if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
            return Ok(None);
        }
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let ResolvedType::Named(instance) = resolved else {
            return Ok(type_identity_from_resolved_type(resolved));
        };
        let Some(candidate) = self.type_symbol_candidate(instance) else {
            return Ok(None);
        };
        let Some(symbol) = self.symbol(&candidate) else {
            return Ok(None);
        };
        if symbol.generic_parameter.is_some() {
            return Ok(None);
        }
        if let Some(alias_ref) = symbol
            .type_ref
            .as_ref()
            .filter(|_| symbol.type_kind == TypeKind::Other)
        {
            let alias_key = (
                instance.uri.clone(),
                instance.key.clone(),
                instance.substitution.clone(),
            );
            if !active_aliases.insert(alias_key.clone()) {
                return Ok(None);
            }
            let identity = (|| {
                let Some(document) = self.documents.get(&instance.uri) else {
                    return Ok(None);
                };
                let Some(identifier) = self.contract_lookup_identifier_with_budget(
                    document,
                    alias_ref.span,
                    cancel,
                    budget,
                    "contract type alias identity",
                )?
                else {
                    return Ok(None);
                };
                let mut state = ResolutionState::new();
                let receivers = self.type_receivers_for_type_ref_with_budget(
                    &instance.uri,
                    document,
                    alias_ref.span.start,
                    alias_ref,
                    identifier,
                    Some(symbol.scope),
                    &instance.substitution,
                    &mut state,
                    cancel,
                    budget,
                )?;
                if state.receiver_resolution_uncertain() {
                    return Ok(None);
                }
                let Some(underlying) = resolved_type_from_receivers(receivers) else {
                    return Ok(None);
                };
                self.semantic_type_identity_with_budget(
                    &underlying,
                    cancel,
                    budget,
                    active_aliases,
                    depth.saturating_add(1),
                )
            })();
            active_aliases.remove(&alias_key);
            return identity;
        }

        let mut args = Vec::with_capacity(instance.parameter_names.len());
        for name in &instance.parameter_names {
            let Some(argument) = instance.substitution.get(name) else {
                return Ok(None);
            };
            let Some(argument) = self.semantic_type_identity_with_budget(
                argument,
                cancel,
                budget,
                active_aliases,
                depth.saturating_add(1),
            )?
            else {
                return Ok(None);
            };
            args.push(argument);
        }
        Ok(Some(TypeIdentity::Named {
            uri: instance.uri.clone(),
            key: instance.key.clone(),
            kind: instance.kind,
            args,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_type_fingerprint_with_budget(
        &self,
        uri: &Url,
        type_ref: &TypeRef,
        substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<ContractTypeFingerprint>, String> {
        let Some(resolved) = self.resolve_contract_type_ref_with_budget(
            uri,
            type_ref,
            substitution,
            None,
            cancel,
            budget,
        )?
        else {
            return Ok(None);
        };
        self.contract_type_fingerprint_for_resolved_with_budget(
            &resolved,
            cancel,
            budget,
            &mut HashSet::new(),
            &mut HashSet::new(),
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_contract_type_ref_with_budget(
        &self,
        uri: &Url,
        type_ref: &TypeRef,
        substitution: &GenericSubstitution,
        scope_override: Option<usize>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<ResolvedType>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let Some(lookup_identifier) = self.contract_lookup_identifier_with_budget(
            document,
            type_ref.span,
            cancel,
            budget,
            "contract type fingerprint",
        )?
        else {
            return Ok(None);
        };
        let mut state = ResolutionState::new();
        let receivers = self.type_receivers_for_type_ref_with_budget(
            uri,
            document,
            type_ref.span.start,
            type_ref,
            lookup_identifier,
            scope_override,
            substitution,
            &mut state,
            cancel,
            budget,
        )?;
        if state.receiver_resolution_uncertain() {
            return Ok(None);
        }
        Ok(resolved_type_from_receivers(receivers))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_type_fingerprint_for_resolved_with_budget(
        &self,
        resolved: &ResolvedType,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        active_dependencies: &mut HashSet<(Url, usize, usize)>,
        depth: usize,
    ) -> Result<Option<ContractTypeFingerprint>, String> {
        if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
            return Ok(None);
        }
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match resolved {
            ResolvedType::Builtin(builtin) => Ok(Some(ContractTypeFingerprint::Builtin(*builtin))),
            ResolvedType::IntegerLiteral(value) => {
                Ok(Some(ContractTypeFingerprint::IntegerLiteral(*value)))
            }
            ResolvedType::Named(instance) => {
                let Some(candidate) = self.type_symbol_candidate(instance) else {
                    return Ok(None);
                };
                let Some(symbol) = self.symbol(&candidate) else {
                    return Ok(None);
                };
                if symbol.generic_parameter.is_some() {
                    return Ok(None);
                }
                let alias_key = (
                    instance.uri.clone(),
                    instance.key.clone(),
                    instance.substitution.clone(),
                );
                if !active_aliases.insert(alias_key.clone()) {
                    return Ok(None);
                }
                let fingerprint = (|| {
                    let mut args = Vec::with_capacity(instance.parameter_names.len());
                    for name in &instance.parameter_names {
                        let Some(argument) = instance.substitution.get(name) else {
                            return Ok(None);
                        };
                        let Some(argument) = self
                            .contract_type_fingerprint_for_resolved_with_budget(
                                argument,
                                cancel,
                                budget,
                                active_aliases,
                                active_dependencies,
                                depth.saturating_add(1),
                            )?
                        else {
                            return Ok(None);
                        };
                        args.push(argument);
                    }

                    let definition = if let Some(alias_ref) = symbol
                        .type_ref
                        .as_ref()
                        .filter(|_| symbol.type_kind == TypeKind::Other)
                    {
                        let underlying = self.resolve_contract_type_ref_with_budget(
                            &instance.uri,
                            alias_ref,
                            &instance.substitution,
                            Some(symbol.scope),
                            cancel,
                            budget,
                        )?;
                        match underlying {
                            Some(underlying) => {
                                let Some(underlying) = self
                                    .contract_type_fingerprint_for_resolved_with_budget(
                                        &underlying,
                                        cancel,
                                        budget,
                                        active_aliases,
                                        active_dependencies,
                                        depth.saturating_add(1),
                                    )?
                                else {
                                    return Ok(None);
                                };
                                ContractTypeDefinitionFingerprint::Alias(Box::new(underlying))
                            }
                            None => {
                                let Some(source) = self
                                    .contract_type_definition_source_fingerprint_with_dependencies(
                                        instance,
                                        symbol,
                                        cancel,
                                        budget,
                                        active_aliases,
                                        active_dependencies,
                                        depth,
                                    )?
                                else {
                                    return Ok(None);
                                };
                                source
                            }
                        }
                    } else if let Some(shape) = symbol.type_shape.as_ref() {
                        let shape = self.contract_type_shape_fingerprint_with_budget(
                            &instance.uri,
                            shape,
                            &instance.substitution,
                            Some(symbol.scope),
                            cancel,
                            budget,
                            active_aliases,
                            active_dependencies,
                            depth.saturating_add(1),
                        )?;
                        match shape {
                            Some(shape) => {
                                ContractTypeDefinitionFingerprint::Shape(Box::new(shape))
                            }
                            None => {
                                let Some(source) = self
                                    .contract_type_definition_source_fingerprint_with_dependencies(
                                        instance,
                                        symbol,
                                        cancel,
                                        budget,
                                        active_aliases,
                                        active_dependencies,
                                        depth,
                                    )?
                                else {
                                    return Ok(None);
                                };
                                source
                            }
                        }
                    } else {
                        let Some(source) = self
                            .contract_type_definition_source_fingerprint_with_dependencies(
                                instance,
                                symbol,
                                cancel,
                                budget,
                                active_aliases,
                                active_dependencies,
                                depth,
                            )?
                        else {
                            return Ok(None);
                        };
                        source
                    };

                    Ok(Some(ContractTypeFingerprint::Named {
                        uri: instance.uri.clone(),
                        key: instance.key.clone(),
                        kind: instance.kind,
                        args,
                        definition,
                    }))
                })();
                active_aliases.remove(&alias_key);
                fingerprint
            }
        }
    }

    fn contract_type_definition_source_fingerprint(
        &self,
        uri: &Url,
        symbol: &Symbol,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<String>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let Some(source) = document
            .source
            .get(symbol.declaration_span.start..symbol.declaration_span.end)
        else {
            return Ok(None);
        };
        let fingerprint = normalized_contract_type_definition(source);
        budget.require_bytes(fingerprint.len(), cancel)?;
        Ok((!fingerprint.is_empty()).then_some(fingerprint))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_type_definition_source_fingerprint_with_dependencies(
        &self,
        instance: &TypeInstance,
        symbol: &Symbol,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        active_dependencies: &mut HashSet<(Url, usize, usize)>,
        depth: usize,
    ) -> Result<Option<ContractTypeDefinitionFingerprint>, String> {
        let Some(text) = self.contract_type_definition_source_fingerprint(
            &instance.uri,
            symbol,
            cancel,
            budget,
        )?
        else {
            return Ok(None);
        };
        let Some(dependencies) = self.contract_type_definition_dependencies_with_budget(
            instance,
            symbol,
            cancel,
            budget,
            active_aliases,
            active_dependencies,
            depth,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(ContractTypeDefinitionFingerprint::Source {
            text,
            dependencies,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_type_definition_dependencies_with_budget(
        &self,
        instance: &TypeInstance,
        symbol: &Symbol,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        active_dependencies: &mut HashSet<(Url, usize, usize)>,
        depth: usize,
    ) -> Result<Option<Vec<ContractDefinitionDependencyFingerprint>>, String> {
        let Some(document) = self.documents.get(&instance.uri) else {
            return Ok(None);
        };
        let Some(type_declaration) = declared_type_node_for_span_with_budget(
            document.tree.root_node(),
            symbol.span,
            cancel,
            budget,
        )?
        else {
            return Ok(None);
        };
        let identifiers = collect_identifier_nodes_with_budget(type_declaration, cancel, budget)?;
        let mut dependencies = Vec::new();
        let mut seen = HashSet::new();
        for identifier in identifiers {
            check_navigation_cancel(cancel)?;
            if Span::from_node(identifier) == symbol.span {
                continue;
            }
            if identifier_is_decl_arg_name_with_budget(identifier, cancel, budget)?
                || identifier_is_dot_rhs_with_budget(identifier, type_declaration, cancel, budget)?
            {
                continue;
            }
            let Some(name) = document
                .source
                .get(identifier.start_byte()..identifier.end_byte())
            else {
                return Ok(None);
            };
            let type_ref = TypeRef {
                path: vec![canonical_name(name)],
                args: Vec::new(),
                span: Span::from_node(identifier),
            };
            if let Some(resolved) = self.resolve_contract_type_ref_with_budget(
                &instance.uri,
                &type_ref,
                &instance.substitution,
                Some(symbol.scope),
                cancel,
                budget,
            )? {
                if matches!(resolved, ResolvedType::Builtin(_)) {
                    continue;
                }
                let Some(fingerprint) = self.contract_type_fingerprint_for_resolved_with_budget(
                    &resolved,
                    cancel,
                    budget,
                    active_aliases,
                    active_dependencies,
                    depth.saturating_add(1),
                )?
                else {
                    return Ok(None);
                };
                let dependency =
                    ContractDefinitionDependencyFingerprint::Type(Box::new(fingerprint));
                if seen.insert(dependency.clone()) {
                    dependencies.push(dependency);
                }
                continue;
            }

            let mut state = ResolutionState::new();
            let candidates = self.unqualified_references_with_budget_at_scope_and_state(
                &instance.uri,
                document,
                identifier.start_byte(),
                name,
                identifier,
                symbol.scope,
                &mut state,
                cancel,
                budget,
            )?;
            if state.receiver_resolution_uncertain() || candidates.len() != 1 {
                return Ok(None);
            }
            let candidate = &candidates[0];
            if self.candidate_is_conditionally_unknown(candidate) {
                return Ok(None);
            }
            let Some(dependency_symbol) = self.symbol(candidate) else {
                return Ok(None);
            };
            if dependency_symbol.kind == SymbolKind::Unit {
                return Ok(None);
            }
            let Some(dependency) = self.contract_definition_dependency_fingerprint_with_budget(
                candidate,
                dependency_symbol,
                &instance.substitution,
                cancel,
                budget,
                active_aliases,
                active_dependencies,
                depth.saturating_add(1),
            )?
            else {
                return Ok(None);
            };
            if seen.insert(dependency.clone()) {
                dependencies.push(dependency);
            }
        }
        Ok(Some(dependencies))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_definition_dependency_fingerprint_with_budget(
        &self,
        candidate: &Candidate,
        symbol: &Symbol,
        substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        active_dependencies: &mut HashSet<(Url, usize, usize)>,
        depth: usize,
    ) -> Result<Option<ContractDefinitionDependencyFingerprint>, String> {
        let Some(text) = self.contract_type_definition_source_fingerprint(
            &candidate.uri,
            symbol,
            cancel,
            budget,
        )?
        else {
            return Ok(None);
        };

        let dependencies = if matches!(symbol.kind, SymbolKind::Constant | SymbolKind::EnumValue) {
            let key = (
                candidate.uri.clone(),
                symbol.declaration_span.start,
                symbol.declaration_span.end,
            );
            if !active_dependencies.insert(key.clone()) {
                return Ok(None);
            }
            let result = self.contract_value_definition_dependencies_with_budget(
                candidate,
                symbol,
                substitution,
                cancel,
                budget,
                active_aliases,
                active_dependencies,
                depth,
            );
            active_dependencies.remove(&key);
            result?
        } else {
            // Non-value symbols retain the historical source fingerprint. A
            // value expression that resolves to one is not recursively
            // evaluable here, but its declaration text remains useful for
            // supported type-source dependencies. Constant/value references
            // themselves are handled above and fail closed on uncertainty.
            Some(Vec::new())
        };
        let Some(dependencies) = dependencies else {
            return Ok(None);
        };
        Ok(Some(ContractDefinitionDependencyFingerprint::Symbol {
            uri: candidate.uri.clone(),
            key: symbol.key.clone(),
            kind: symbol.type_kind,
            text,
            dependencies,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_value_definition_dependencies_with_budget(
        &self,
        candidate: &Candidate,
        symbol: &Symbol,
        substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        active_dependencies: &mut HashSet<(Url, usize, usize)>,
        depth: usize,
    ) -> Result<Option<Vec<ContractDefinitionDependencyFingerprint>>, String> {
        if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
            return Ok(None);
        }
        let Some(document) = self.documents.get(&candidate.uri) else {
            return Ok(None);
        };
        let Some(declaration) =
            declaration_node_for_symbol_with_budget(document, symbol, cancel, budget)?
        else {
            return Ok(None);
        };
        let Some(value) = declaration
            .child_by_field_name("defaultValue")
            .or_else(|| declaration.child_by_field_name("value"))
        else {
            return Ok(Some(Vec::new()));
        };
        let identifiers = collect_identifier_nodes_with_budget(value, cancel, budget)?;
        let mut dependencies = Vec::new();
        let mut seen = HashSet::new();
        for identifier in identifiers {
            check_navigation_cancel(cancel)?;
            if identifier_is_dot_rhs_with_budget(identifier, value, cancel, budget)? {
                continue;
            }
            let Some(name) = document
                .source
                .get(identifier.start_byte()..identifier.end_byte())
            else {
                return Ok(None);
            };
            let type_ref = TypeRef {
                path: vec![canonical_name(name)],
                args: Vec::new(),
                span: Span::from_node(identifier),
            };
            if let Some(resolved) = self.resolve_contract_type_ref_with_budget(
                &candidate.uri,
                &type_ref,
                substitution,
                Some(symbol.scope),
                cancel,
                budget,
            )? {
                if matches!(resolved, ResolvedType::Builtin(_)) {
                    continue;
                }
                let Some(fingerprint) = self.contract_type_fingerprint_for_resolved_with_budget(
                    &resolved,
                    cancel,
                    budget,
                    active_aliases,
                    active_dependencies,
                    depth.saturating_add(1),
                )?
                else {
                    return Ok(None);
                };
                let dependency =
                    ContractDefinitionDependencyFingerprint::Type(Box::new(fingerprint));
                if seen.insert(dependency.clone()) {
                    dependencies.push(dependency);
                }
                continue;
            }

            let mut state = ResolutionState::new();
            let candidates = self.unqualified_references_with_budget_at_scope_and_state(
                &candidate.uri,
                document,
                identifier.start_byte(),
                name,
                identifier,
                symbol.scope,
                &mut state,
                cancel,
                budget,
            )?;
            if state.receiver_resolution_uncertain() || candidates.len() != 1 {
                return Ok(None);
            }
            let dependency_candidate = &candidates[0];
            if self.candidate_is_conditionally_unknown(dependency_candidate) {
                return Ok(None);
            }
            let Some(dependency_symbol) = self.symbol(dependency_candidate) else {
                return Ok(None);
            };
            if dependency_symbol.kind == SymbolKind::Unit {
                return Ok(None);
            }
            if !matches!(
                dependency_symbol.kind,
                SymbolKind::Constant | SymbolKind::EnumValue
            ) {
                // A static bound/default dependency must be a proven constant
                // value. Text for a variable, field, property, or routine is
                // not a semantic value proof and must not become a reusable
                // identity merely because its declaration text is stable.
                return Ok(None);
            }
            let Some(dependency) = self.contract_definition_dependency_fingerprint_with_budget(
                dependency_candidate,
                dependency_symbol,
                substitution,
                cancel,
                budget,
                active_aliases,
                active_dependencies,
                depth.saturating_add(1),
            )?
            else {
                return Ok(None);
            };
            if seen.insert(dependency.clone()) {
                dependencies.push(dependency);
            }
        }
        Ok(Some(dependencies))
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_type_shape_fingerprint_with_budget(
        &self,
        uri: &Url,
        shape: &TypeShape,
        substitution: &GenericSubstitution,
        scope_override: Option<usize>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        active_aliases: &mut HashSet<(Url, String, GenericSubstitution)>,
        active_dependencies: &mut HashSet<(Url, usize, usize)>,
        depth: usize,
    ) -> Result<Option<ContractTypeShapeFingerprint>, String> {
        if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
            return Ok(None);
        }
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match shape {
            TypeShape::Named(type_ref) => {
                let Some(resolved) = self.resolve_contract_type_ref_with_budget(
                    uri,
                    type_ref,
                    substitution,
                    scope_override,
                    cancel,
                    budget,
                )?
                else {
                    return Ok(None);
                };
                let Some(fingerprint) = self.contract_type_fingerprint_for_resolved_with_budget(
                    &resolved,
                    cancel,
                    budget,
                    active_aliases,
                    active_dependencies,
                    depth.saturating_add(1),
                )?
                else {
                    return Ok(None);
                };
                Ok(Some(ContractTypeShapeFingerprint::Named(Box::new(
                    fingerprint,
                ))))
            }
            TypeShape::Pointer(element) => {
                let Some(element) = self.contract_type_shape_fingerprint_with_budget(
                    uri,
                    element,
                    substitution,
                    scope_override,
                    cancel,
                    budget,
                    active_aliases,
                    active_dependencies,
                    depth.saturating_add(1),
                )?
                else {
                    return Ok(None);
                };
                Ok(Some(ContractTypeShapeFingerprint::Pointer(Box::new(
                    element,
                ))))
            }
            TypeShape::Array { element, dynamic } => {
                if !dynamic {
                    // Static bounds are not represented in TypeShape.  Let
                    // the enclosing declaration's normalized source (and its
                    // semantic dependencies) carry the complete identity.
                    return Ok(None);
                }
                let Some(element) = self.contract_type_shape_fingerprint_with_budget(
                    uri,
                    element,
                    substitution,
                    scope_override,
                    cancel,
                    budget,
                    active_aliases,
                    active_dependencies,
                    depth.saturating_add(1),
                )?
                else {
                    return Ok(None);
                };
                Ok(Some(ContractTypeShapeFingerprint::Array {
                    element: Box::new(element),
                    dynamic: *dynamic,
                }))
            }
            TypeShape::Callable => {
                // Callable parameter/result/mode/convention details are not
                // retained by TypeShape.  Use the declaration-source fallback
                // below instead of comparing a lossy marker.
                Ok(None)
            }
            TypeShape::Unknown => Ok(None),
        }
    }

    fn contract_method_implementation_status(
        &self,
        requirement: &ContractRequirement,
        method_name: &str,
        routines: &[ContractRoutineCandidate],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let candidates = routines
            .iter()
            .filter(|candidate| candidate.symbol_key == method_name)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(ContractMatch::No);
        }
        let Some(requirement_symbol) = self.symbol(&requirement.candidate) else {
            return Ok(ContractMatch::Unknown);
        };
        let mut unknown = false;
        let mut abstract_match = false;
        for candidate in candidates {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if candidate.conditional_unknown(self) {
                unknown = true;
                continue;
            }
            let Some(symbol) = self.symbol(&candidate.candidate) else {
                unknown = true;
                continue;
            };
            if !matches!(
                symbol.visibility,
                Visibility::Public | Visibility::Published
            ) {
                unknown = true;
                continue;
            }
            match self.routines_contract_match(
                requirement_symbol,
                &requirement.candidate.uri,
                &requirement.substitution,
                symbol,
                &candidate.candidate.uri,
                &candidate.substitution,
                cancel,
                budget,
            )? {
                ContractMatch::Yes if symbol.routine_directives.abstract_ => {
                    abstract_match = true;
                }
                ContractMatch::Yes => return Ok(ContractMatch::Yes),
                ContractMatch::No => {}
                ContractMatch::Unknown => unknown = true,
            }
        }
        Ok(if unknown || abstract_match {
            ContractMatch::Unknown
        } else {
            ContractMatch::No
        })
    }

    fn direct_interface_method_declaration(
        &self,
        obligation: &ContractMissingInterfaceMethod,
        class: &TypeInstance,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<DirectInterfaceMethodDeclaration, String> {
        let Some(document) = self.documents.get(&class.uri) else {
            return Ok(DirectInterfaceMethodDeclaration::Ambiguous);
        };
        let indices = document
            .member_symbol_indices_by_owner
            .get(&class.key)
            .cloned()
            .unwrap_or_default();
        budget.require_work(indices.len(), cancel)?;
        let Some(requirement_symbol) = self.symbol(&obligation.requirement.candidate) else {
            return Ok(DirectInterfaceMethodDeclaration::Ambiguous);
        };
        let mut exact = Vec::new();
        let mut uncertain = false;
        let mut nonroutine_collision = false;
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(index) else {
                uncertain = true;
                continue;
            };
            if symbol.owner_type.as_deref() != Some(class.key.as_str())
                || symbol.key != obligation.method_name
            {
                continue;
            }
            if symbol.kind != SymbolKind::Routine {
                nonroutine_collision = true;
                continue;
            }
            if symbol.origin != Origin::Declaration {
                continue;
            }
            match self.routines_contract_match(
                requirement_symbol,
                &obligation.requirement.candidate.uri,
                &obligation.requirement.substitution,
                symbol,
                &class.uri,
                &class.substitution,
                cancel,
                budget,
            )? {
                ContractMatch::Yes => exact.push(index),
                ContractMatch::No => {}
                ContractMatch::Unknown => uncertain = true,
            }
        }
        if uncertain || nonroutine_collision || exact.len() > 1 {
            return Ok(DirectInterfaceMethodDeclaration::Ambiguous);
        }
        if let Some(index) = exact.first().copied() {
            Ok(DirectInterfaceMethodDeclaration::Present { index })
        } else {
            Ok(DirectInterfaceMethodDeclaration::Missing)
        }
    }

    fn interface_method_name_collision_is_safe(
        &self,
        obligation: &ContractMissingInterfaceMethod,
        surface: &ContractClassSurface,
        method_name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        if surface.implicit_compiler_root_is_open {
            return Ok(false);
        }
        if surface
            .nonroutine_names
            .contains(&canonical_name(method_name))
        {
            return Ok(false);
        }
        let mut same_name_routines = Vec::new();
        let method_key = canonical_name(method_name);
        for candidate in &surface.routines {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate.candidate) else {
                return Ok(false);
            };
            if symbol.key != method_key {
                continue;
            }
            // Overload directives do not make result-only, ABI-only, or
            // parameter-mode-only differences legal overload distinctions.
            // Compare the parameter *types* separately from full interface
            // conformance so these same-call-shape collisions are withheld
            // even when every declaration happens to carry `overload`.
            match self.routine_parameter_shape_match(
                &obligation.requirement,
                &candidate.candidate,
                &candidate.substitution,
                cancel,
                budget,
            )? {
                ContractMatch::Yes | ContractMatch::Unknown => return Ok(false),
                ContractMatch::No => {}
            }
            same_name_routines.push(symbol);
        }
        if same_name_routines.is_empty() {
            return Ok(true);
        }
        // A new overload is safe only when every existing same-name routine
        // explicitly belongs to an overload group and the obligation itself
        // is also marked overload. Otherwise this would silently change the
        // class's member binding or hide an incompatible member.
        let Some(interface_method) = self.symbol(&obligation.requirement.candidate) else {
            return Ok(false);
        };
        Ok(interface_method.routine_directives.overload
            && same_name_routines
                .iter()
                .all(|symbol| symbol.routine_directives.overload))
    }

    /// Compare only the number and resolved types of parameters.  Pascal
    /// cannot use a result type, calling convention, or `var`/`out` mode as
    /// the sole distinction for a same-name overload.  Unknown type identity
    /// is deliberately returned to the caller so generation is suppressed.
    #[allow(clippy::too_many_arguments)]
    fn routine_parameter_shape_match(
        &self,
        requirement: &ContractRequirement,
        candidate: &Candidate,
        candidate_substitution: &GenericSubstitution,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let Some(left) = self.symbol(&requirement.candidate) else {
            return Ok(ContractMatch::Unknown);
        };
        let Some(right) = self.symbol(candidate) else {
            return Ok(ContractMatch::Unknown);
        };
        if left.routine_parameters.len() != right.routine_parameters.len() {
            return Ok(ContractMatch::No);
        }
        for (left_parameter, right_parameter) in left
            .routine_parameters
            .iter()
            .zip(&right.routine_parameters)
        {
            let parameter_match = self.contract_parameter_type_match(
                &requirement.candidate.uri,
                left_parameter.type_ref.as_ref(),
                &requirement.substitution,
                &candidate.uri,
                right_parameter.type_ref.as_ref(),
                candidate_substitution,
                cancel,
                budget,
            )?;
            match parameter_match {
                ContractMatch::Yes => {}
                ContractMatch::No => return Ok(ContractMatch::No),
                ContractMatch::Unknown => return Ok(ContractMatch::Unknown),
            }
        }
        Ok(ContractMatch::Yes)
    }

    fn contract_method_resolution_name(
        &self,
        requirement: &ContractRequirement,
        resolutions: &[MethodResolution],
    ) -> Result<Option<String>, String> {
        let Some(interface_method) = self.symbol(&requirement.candidate) else {
            return Ok(None);
        };
        let matches = resolutions
            .iter()
            .filter(|resolution| {
                resolution.interface_owner == requirement.interface.key
                    && resolution.interface_method == interface_method.key
            })
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Ok(None);
        }
        let first = matches[0].implementation_method.clone();
        if matches
            .iter()
            .skip(1)
            .any(|resolution| resolution.implementation_method != first)
        {
            return Err("ambiguous method-resolution clause".to_owned());
        }
        Ok(Some(first))
    }

    fn contract_interface_covers_requirement(
        &self,
        target: &TypeInstance,
        requirement: &TypeInstance,
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let identity = (
            contract_interface_identity(target),
            contract_interface_identity(requirement),
        );
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if let Some(result) = ancestry.interface_coverage.get(&identity) {
            return Ok(*result);
        }
        if target.kind != TypeKind::Interface || requirement.kind != TypeKind::Interface {
            return Ok(ContractMatch::Unknown);
        }
        if contract_interface_identity(target) == contract_interface_identity(requirement) {
            ancestry
                .interface_coverage
                .insert(identity, ContractMatch::Yes);
            return Ok(ContractMatch::Yes);
        }
        if !ancestry.active_interface_coverage.insert(identity.clone()) {
            return Ok(ContractMatch::Unknown);
        }
        if ancestry.active_interface_coverage.len() > MAX_CONTRACT_ANCESTRY_DEPTH {
            ancestry.active_interface_coverage.remove(&identity);
            return Ok(ContractMatch::Unknown);
        }
        let result = (|| {
            let resolution =
                self.resolve_contract_type_with_budget(target, ancestry, cancel, budget)?;
            if resolution.status == AncestryStatus::Unknown {
                return Ok(ContractMatch::Unknown);
            }
            let mut unknown = false;
            for parent in resolution.interfaces {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                match self.contract_interface_covers_requirement(
                    &parent,
                    requirement,
                    ancestry,
                    cancel,
                    budget,
                )? {
                    ContractMatch::Yes => return Ok(ContractMatch::Yes),
                    ContractMatch::No => {}
                    ContractMatch::Unknown => unknown = true,
                }
            }
            Ok(if unknown {
                ContractMatch::Unknown
            } else {
                ContractMatch::No
            })
        })();
        ancestry.active_interface_coverage.remove(&identity);
        if let Ok(result) = &result {
            ancestry.interface_coverage.insert(identity, *result);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn contract_delegation_status(
        &self,
        requirement: &ContractRequirement,
        class: &TypeInstance,
        delegations: &[InterfaceDelegation],
        ancestry: &mut ContractAncestryState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        let mut unknown = false;
        for delegation in delegations {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let declaring_uri = delegation.declaring_uri.as_ref().unwrap_or(&class.uri);
            let declaring_substitution = delegation
                .declaring_substitution
                .as_ref()
                .unwrap_or(&class.substitution);
            let Some(document) = self.documents.get(declaring_uri) else {
                return Ok(ContractMatch::Unknown);
            };
            let Some(identifier) = self.contract_lookup_identifier_with_budget(
                document,
                delegation.interface.span,
                cancel,
                budget,
                "interface delegation",
            )?
            else {
                unknown = true;
                continue;
            };
            let mut state = ResolutionState::new();
            let receivers = self.type_receivers_for_type_ref_with_budget(
                declaring_uri,
                document,
                delegation.interface.span.start,
                &delegation.interface,
                identifier,
                None,
                declaring_substitution,
                &mut state,
                cancel,
                budget,
            )?;
            if state.receiver_resolution_uncertain() {
                unknown = true;
                continue;
            }
            let Some(target) = unique_type_instance(receivers) else {
                unknown = true;
                continue;
            };
            match self.contract_interface_covers_requirement(
                &target,
                &requirement.interface,
                ancestry,
                cancel,
                budget,
            )? {
                ContractMatch::Yes => return Ok(ContractMatch::Yes),
                ContractMatch::Unknown => unknown = true,
                ContractMatch::No => {}
            }
        }
        Ok(if unknown {
            ContractMatch::Unknown
        } else {
            ContractMatch::No
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn semantic_call_analysis_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        call: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<overload::CallAnalysis, String> {
        let Some(entity) = call.child_by_field_name("entity") else {
            return Ok(overload::CallAnalysis::Unsupported);
        };
        let lookup_identifier = callable_lookup_identifier(entity);
        let mut state = ResolutionState::new();
        let mut candidates = self.resolve_candidates_at_with_state_and_budget(
            uri,
            document,
            lookup_identifier.start_byte(),
            lookup_identifier,
            &mut state,
            0,
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(overload::CallAnalysis::Incomplete);
        }
        if overload::state_has_uncertainty(&state) {
            return Ok(overload::CallAnalysis::Incomplete);
        }
        candidates.retain(|candidate| {
            self.symbol(candidate).is_some_and(|symbol| {
                symbol.kind == SymbolKind::Routine && !symbol.unresolved_abbreviated
            })
        });
        if candidates.is_empty() {
            return Ok(overload::CallAnalysis::Unsupported);
        }
        let owner_receivers = callable_owner_node(entity)
            .map(|owner| {
                self.resolve_receivers_with_state_and_budget(
                    uri,
                    document,
                    entity.start_byte(),
                    owner,
                    owner,
                    &mut state,
                    cancel,
                    budget,
                    0,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let owner_instances = owner_receivers
            .into_iter()
            .filter_map(|receiver| match receiver {
                Receiver::Type(instance) => Some(instance),
                Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
            })
            .collect::<Vec<_>>();
        if overload::state_has_uncertainty(&state) {
            return Ok(overload::CallAnalysis::Incomplete);
        }
        let analysis = overload::analyze_call(
            self,
            uri,
            document,
            call,
            &candidates,
            &GenericSubstitution::empty(),
            &owner_instances,
            &mut state,
            0,
            cancel,
            budget,
        )?;
        if overload::state_has_uncertainty(&state) {
            return Ok(overload::CallAnalysis::Incomplete);
        }
        Ok(analysis)
    }

    /// Classify one source identifier using the same conservative proof model
    /// as [`NavigationIndex::semantic_diagnostics_with_cancel`].
    #[allow(dead_code)]
    pub(crate) fn semantic_proof_status_at_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<SemanticProofStatus, String> {
        let Some(document) = self.documents.get(uri) else {
            return Err(format!("document is not indexed: {uri}"));
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(SemanticProofStatus::Incomplete);
        };
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Ok(SemanticProofStatus::Incomplete);
        };
        let mut budget = AssistanceBudget::new(
            MAX_SEMANTIC_DIAGNOSTIC_WORK,
            MAX_SEMANTIC_DIAGNOSTIC_BYTES,
            "semantic proof status",
        );
        self.semantic_proof_at_with_budget(uri, document, offset, identifier, cancel, &mut budget)
    }

    /// Return whether the current identifier has no resolvable or ambiguous
    /// source binding.  This is the missing-unit action boundary: an unknown
    /// compiler-provided System export does not by itself suppress a
    /// provider-specific action, but every concrete resolver uncertainty does.
    pub(crate) fn missing_unit_is_unresolved_at_with_budget(
        &self,
        uri: &Url,
        offset: usize,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        let Some(document) = self.documents.get(uri) else {
            return Err(format!("document is not indexed: {uri}"));
        };
        budget.require_work(1, cancel)?;
        budget.require_bytes(
            identifier
                .end_byte()
                .saturating_sub(identifier.start_byte()),
            cancel,
        )?;
        if !Self::missing_unit_lookup_context_supported_with_budget(
            identifier,
            &document.source,
            cancel,
            budget,
        )? || document.conditionals.is_unknown_at(offset)
            || document
                .opaque_ranges
                .iter()
                .any(|range| range.contains_offset(offset))
            || !self.imports_are_complete_for_proof(document, offset, cancel, budget)?
        {
            return Ok(false);
        }
        let mut state = ResolutionState::new();
        let candidates = self.resolve_candidates_at_with_state_and_budget(
            uri, document, offset, identifier, &mut state, 0, cancel, budget,
        )?;
        Ok(candidates.is_empty()
            && !state.ambiguous
            && !state.member_lookup_incomplete
            && !state.receiver_resolution_uncertain()
            && !state.inaccessible_candidate)
    }

    #[allow(clippy::too_many_arguments)]
    fn semantic_proof_at_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<SemanticProofStatus, String> {
        budget.require_work(1, cancel)?;
        budget.require_bytes(
            identifier
                .end_byte()
                .saturating_sub(identifier.start_byte()),
            cancel,
        )?;
        let non_value =
            is_non_value_identifier_with_budget(identifier, &document.source, cancel, budget)?;
        if document.conditionals.is_unknown_at(offset)
            || document
                .opaque_ranges
                .iter()
                .any(|range| range.contains_offset(offset))
            || non_value
        {
            return Ok(SemanticProofStatus::Incomplete);
        }
        if !self.imports_are_complete_for_proof(document, offset, cancel, budget)? {
            return Ok(SemanticProofStatus::Incomplete);
        }

        let mut state = ResolutionState::new();
        let candidates = self.resolve_candidates_at_with_state_and_budget(
            uri, document, offset, identifier, &mut state, 0, cancel, budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(SemanticProofStatus::Incomplete);
        }
        if !candidates.is_empty() {
            return Ok(SemanticProofStatus::Resolved);
        }
        if state.ambiguous {
            return Ok(SemanticProofStatus::Ambiguous);
        }
        if state.member_lookup_incomplete
            || state.receiver_resolution_uncertain()
            || state.inaccessible_candidate
            || state.implicit_system_namespace_incomplete
        {
            return Ok(SemanticProofStatus::Incomplete);
        }
        Ok(SemanticProofStatus::ProvenAbsent)
    }

    fn missing_unit_lookup_context_supported_with_budget(
        identifier: Node<'_>,
        source: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        let name = canonical_name(node_text_with_budget(identifier, source, cancel, budget)?);
        if is_declaration_identifier(identifier)
            || is_implicit_or_intrinsic_name(&name)
            || is_type_valued_intrinsic_argument_with_budget(identifier, source, cancel, budget)?
        {
            return Ok(false);
        }
        let span = Span::from_node(identifier);
        let mut current = Some(identifier);
        while let Some(node) = current {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if matches!(
                node.kind(),
                "asm"
                    | "declExport"
                    | "declExports"
                    | "declHelper"
                    | "declLabels"
                    | "inherited"
                    | "label"
                    | "goto"
                    | "procAttribute"
                    | "rttiAttributes"
            ) {
                return Ok(false);
            }
            if node.kind() == "exprBinary"
                && node
                    .child_by_field_name("lhs")
                    .is_some_and(|lhs| Span::from_node(lhs).contains(span))
                && node
                    .child_by_field_name("operator")
                    .is_some_and(|operator| node_text(operator, source) == ":=")
                && has_ancestor_kind_with_budget(node, "exprCall", cancel, budget)?
            {
                return Ok(false);
            }
            current = node.parent();
        }
        Ok(true)
    }

    pub(crate) fn missing_unit_use_kind_with_budget(
        identifier: Node<'_>,
        source: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MissingUnitUseKind, String> {
        let span = Span::from_node(identifier);
        let mut current = Some(identifier);
        while let Some(node) = current {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if is_type_reference_node(node)
                || node
                    .child_by_field_name("type")
                    .is_some_and(|type_node| Span::from_node(type_node).contains(span))
                || node
                    .child_by_field_name("parent")
                    .is_some_and(|parent| Span::from_node(parent).contains(span))
            {
                return Ok(MissingUnitUseKind::Type);
            }
            if node.kind() == "exprCall"
                && node
                    .child_by_field_name("entity")
                    .is_some_and(|entity| Span::from_node(entity).contains(span))
            {
                return Ok(MissingUnitUseKind::Callable);
            }
            // A Pascal procedure call may omit parentheses.  In that form the
            // identifier is the complete expression statement rather than an
            // `exprCall` node, so preserve the callable role from the
            // statement boundary instead of treating a routine as a value.
            if node.kind() == "statement"
                && Span::from_node(node).contains(span)
                && node
                    .named_children(&mut node.walk())
                    .next()
                    .is_some_and(|expression| Span::from_node(expression) == span)
            {
                return Ok(MissingUnitUseKind::Callable);
            }
            if node.kind() == "exprBinary"
                && node
                    .child_by_field_name("lhs")
                    .is_some_and(|lhs| Span::from_node(lhs).contains(span))
                && node
                    .child_by_field_name("operator")
                    .is_some_and(|operator| node_text(operator, source) == ":=")
            {
                return Ok(MissingUnitUseKind::WritableValue);
            }
            current = node.parent();
        }
        Ok(MissingUnitUseKind::Value)
    }

    /// Validate the exact binding produced by a proposed provider import.
    ///
    /// The target document has already been rebuilt by the project-scoped
    /// snapshot loader.  Consequently the import binding below is the result
    /// of the authoritative project resolver, rather than a URI inserted by
    /// the code-action planner.  This method only checks that binding and then
    /// runs the ordinary navigation resolver against it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn missing_unit_binds_after_import_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        unit_name: &str,
        provider_uri: &Url,
        symbol_name: &str,
        use_kind: MissingUnitUseKind,
        provider_declaration_fingerprint: u64,
        provider_context_fingerprint: u64,
        use_context_fingerprint: u64,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        check_navigation_cancel(cancel)?;
        let Some(document) = self.documents.get(uri) else {
            return Err(format!("document is not indexed: {uri}"));
        };
        if document.conditional_context.fingerprint() != use_context_fingerprint {
            return Ok(false);
        }
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(false);
        };
        let region = document.region_at(offset);
        let active_uses = document.active_uses_with_budget(region, cancel, budget)?;
        if !active_uses
            .into_iter()
            .any(|unit| canonical_name(unit) == canonical_name(unit_name))
        {
            return Ok(false);
        }
        let import_urls = self.unit_urls_for_import_with_budget(
            document,
            &canonical_name(unit_name),
            cancel,
            budget,
        )?;
        if import_urls.len() != 1 || import_urls[0] != *provider_uri {
            return Ok(false);
        }
        let Some(provider) = self.documents.get(provider_uri) else {
            return Ok(false);
        };
        if provider.conditional_context.fingerprint() != provider_context_fingerprint {
            return Ok(false);
        }
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Ok(false);
        };
        let actual_use_kind =
            Self::missing_unit_use_kind_with_budget(identifier, &document.source, cancel, budget)?;
        if actual_use_kind != use_kind {
            return Ok(false);
        }
        let mut state = ResolutionState::new();
        let candidates = self.resolve_candidates_at_with_state_and_budget(
            uri, document, offset, identifier, &mut state, 0, cancel, budget,
        )?;
        let expected_declaration = candidates.iter().find_map(|candidate| {
            (candidate.uri == *provider_uri)
                .then(|| self.symbol(candidate))
                .flatten()
                .filter(|symbol| {
                    symbol.origin == Origin::Declaration
                        && missing_unit_provider_declaration_fingerprint(symbol, &provider.source)
                            == provider_declaration_fingerprint
                })
        });
        let candidates_match_provider = expected_declaration.is_some()
            && candidates.iter().all(|candidate| {
                candidate.uri == *provider_uri
                    && self.symbol(candidate).is_some_and(|symbol| {
                        canonical_name(&symbol.name) == canonical_name(symbol_name)
                            && use_kind.accepts(symbol.kind)
                            && match symbol.origin {
                                Origin::Declaration => {
                                    missing_unit_provider_declaration_fingerprint(
                                        symbol,
                                        &provider.source,
                                    ) == provider_declaration_fingerprint
                                }
                                Origin::Definition => {
                                    expected_declaration.is_some_and(|expected| {
                                        missing_unit_routine_implementation_status(expected, symbol)
                                            == ContractMatch::Yes
                                    })
                                }
                            }
                    })
            });
        if state.ambiguous
            || state.member_lookup_incomplete
            || state.receiver_resolution_uncertain()
            || state.inaccessible_candidate
            || candidates.is_empty()
            || !candidates_match_provider
        {
            return Ok(false);
        }
        if region == Region::Interface
            && self.missing_unit_interface_cycle_with_cancel(provider_uri, uri, cancel)?
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn missing_unit_interface_cycle_with_cancel(
        &self,
        start: &Url,
        target: &Url,
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        const MAX_INTERFACE_DEPENDENCY_WORK: usize = 256;
        let mut frontier = vec![start.clone()];
        let mut visited = HashSet::new();
        let mut work = 0usize;
        while let Some(uri) = frontier.pop() {
            check_navigation_cancel(cancel)?;
            if uri == *target {
                return Ok(true);
            }
            if !visited.insert(uri.clone()) {
                continue;
            }
            work = work.saturating_add(1);
            if work > MAX_INTERFACE_DEPENDENCY_WORK {
                return Ok(true);
            }
            let Some(document) = self.documents.get(&uri) else {
                return Ok(true);
            };
            let Some(bindings) = document.import_bindings.as_ref() else {
                return Ok(true);
            };
            for unit in &document.interface_uses {
                check_navigation_cancel(cancel)?;
                let Some(dependency) = bindings.get(&canonical_name(unit)) else {
                    return Ok(true);
                };
                if !self.documents.contains_key(dependency) {
                    return Ok(true);
                }
                frontier.push(dependency.clone());
            }
        }
        Ok(false)
    }

    fn imports_are_complete_for_proof(
        &self,
        document: &Document,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        let region = document.region_at(offset);
        let active_uses = document.active_uses_with_budget(region, cancel, budget)?;
        for unit in active_uses {
            check_navigation_cancel(cancel)?;
            let key = canonical_name(unit);
            if document.unknown_imports.contains(&key) {
                return Ok(false);
            }
            let urls = self.unit_urls_for_import_with_budget(document, &key, cancel, budget)?;
            if urls.len() != 1 || urls.iter().any(|url| !self.documents.contains_key(url)) {
                return Ok(false);
            }
            let Some(provider) = urls.first().and_then(|url| self.documents.get(url)) else {
                return Ok(false);
            };
            budget.require_work(
                provider
                    .parser_recovery_spans
                    .len()
                    .saturating_add(provider.conditionals.unknown_spans.len()),
                cancel,
            )?;
            if !provider.parser_recovery_spans.is_empty()
                || !provider.conditionals.unknown_spans.is_empty()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Number of retained parsed documents.
    pub fn document_count(&self) -> usize {
        self.documents.len()
    }

    /// Work and retained byte payload that recovery must destroy with this
    /// index. This is intentionally a bounded-accounting view, not an attempt
    /// to estimate allocator capacity or Rust object layout.
    pub(crate) fn visit_recovery_payload(
        &self,
        mut visit: impl FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for document in self.documents.values() {
            document.parsed.visit_recovery_payload(&mut visit)?;
            if let Some(bindings) = &document.import_bindings {
                for (name, uri) in bindings {
                    visit(name.len())?;
                    visit(uri.as_str().len())?;
                }
            }
        }
        for (unit_name, providers) in &self.units {
            visit(unit_name.len())?;
            for provider in providers {
                visit(provider.as_str().len())?;
            }
        }
        for (unit_name, providers) in &self.auto_import_unit_providers {
            visit(unit_name.len())?;
            for provider in providers {
                visit(provider.as_str().len())?;
            }
        }
        Ok(())
    }

    /// Resolve the identifier at `position` in `uri`.
    ///
    /// Unknown names and unknown receivers return an empty vector. In
    /// particular, this method never performs an unrelated workspace-wide
    /// name search merely because an expression could not be typed.
    pub fn navigate(
        &self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
    ) -> Vec<Location> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Vec::new();
        };
        if document.conditionals.is_unknown_at(offset) {
            return Vec::new();
        }
        if is_ignored_offset(document.tree.root_node(), offset) {
            return Vec::new();
        }
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Vec::new();
        };

        let mut state = ResolutionState::new();
        let mut references =
            self.resolve_candidates_at_with_state(uri, document, offset, identifier, &mut state);
        if references
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Vec::new();
        }

        if let Some(call) = overload::call_for_identifier(identifier) {
            let cancel = AtomicBool::new(false);
            let mut budget = AssistanceBudget::new(
                MAX_NAVIGATION_OVERLOAD_WORK,
                MAX_NAVIGATION_OVERLOAD_BYTES,
                "navigation overload selection",
            );
            let owner_receivers = call
                .child_by_field_name("entity")
                .and_then(callable_owner_node)
                .map(|owner| {
                    self.resolve_receivers_with_state_and_budget(
                        uri,
                        document,
                        offset,
                        owner,
                        owner,
                        &mut state,
                        &cancel,
                        &mut budget,
                        0,
                    )
                })
                .transpose()
                .unwrap_or_default()
                .unwrap_or_default();
            let owner_instances = owner_receivers
                .iter()
                .filter_map(|receiver| match receiver {
                    Receiver::Type(instance) => Some(instance.clone()),
                    Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
                })
                .collect::<Vec<_>>();
            let selection = overload::select(
                self,
                uri,
                document,
                call,
                &references,
                &GenericSubstitution::empty(),
                &owner_instances,
                &mut state,
                0,
                &cancel,
                &mut budget,
            )
            .unwrap_or_default();
            if let Some(group) = selection.selected_group {
                references
                    .retain(|candidate| overload::candidate_in_group(self, candidate, &group));
            } else if selection.no_viable_group {
                references
                    .retain(|candidate| overload::key_for_candidate(self, candidate).is_none());
            }
        }

        self.locations_for(self.expand_property_candidates(references, target), target)
    }

    /// Resolve one navigation target while consuming the caller's request-wide
    /// cancellation and work budget. Callers sharing one budget across many
    /// sites avoid silently multiplying per-site semantic work limits.
    pub(crate) fn navigate_with_cancel_and_budget(
        &self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Location>, String> {
        check_navigation_cancel(cancel)?;
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        budget.require_work(1, cancel)?;
        budget.require_bytes(document.source.len(), cancel)?;
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(Vec::new());
        };
        if document.conditionals.is_unknown_at(offset)
            || is_ignored_offset(document.tree.root_node(), offset)
        {
            return Ok(Vec::new());
        }
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Ok(Vec::new());
        };

        let mut state = ResolutionState::new();
        let mut references = self.resolve_candidates_at_with_state_and_budget(
            uri, document, offset, identifier, &mut state, 0, cancel, budget,
        )?;
        budget.require_work(references.len(), cancel)?;
        if references
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }

        if let Some(call) = overload::call_for_identifier(identifier) {
            let owner_receivers = call
                .child_by_field_name("entity")
                .and_then(callable_owner_node)
                .map(|owner| {
                    self.resolve_receivers_with_state_and_budget(
                        uri, document, offset, owner, owner, &mut state, cancel, budget, 0,
                    )
                })
                .transpose()?
                .unwrap_or_default();
            budget.require_work(owner_receivers.len(), cancel)?;
            let owner_instances = owner_receivers
                .iter()
                .filter_map(|receiver| match receiver {
                    Receiver::Type(instance) => Some(instance.clone()),
                    Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
                })
                .collect::<Vec<_>>();
            budget.require_owned_bytes(
                owner_instances
                    .len()
                    .saturating_mul(std::mem::size_of::<TypeInstance>()),
                cancel,
            )?;
            let selection = overload::select(
                self,
                uri,
                document,
                call,
                &references,
                &GenericSubstitution::empty(),
                &owner_instances,
                &mut state,
                0,
                cancel,
                budget,
            )?;
            if let Some(group) = selection.selected_group {
                references
                    .retain(|candidate| overload::candidate_in_group(self, candidate, &group));
            } else if selection.no_viable_group {
                references
                    .retain(|candidate| overload::key_for_candidate(self, candidate).is_none());
            }
        }

        check_navigation_cancel(cancel)?;
        self.locations_for_with_budget(references, target, cancel, budget)
    }

    fn resolve_candidates_at(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        self.resolve_candidates_at_with_state(uri, document, offset, identifier, &mut state)
    }

    fn unit_reference_candidates_at(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
    ) -> Option<Vec<Candidate>> {
        if let Some(unit_name) = use_name_at(identifier, &document.source) {
            return Some(self.unit_references(document, &unit_name));
        }

        if is_unit_declaration_identifier(identifier) {
            let index = document
                .symbols
                .iter()
                .position(|symbol| symbol.kind == SymbolKind::Unit)?;
            return Some(vec![Candidate {
                uri: uri.clone(),
                index,
            }]);
        }

        let (path_node, parts, cursor_index) = qualified_path_at(identifier, &document.source)?;
        let first = identifier_nodes(path_node).first().copied()?;
        let mut state = ResolutionState::new();
        if !self
            .unqualified_references_with_state(
                uri,
                document,
                first.start_byte(),
                parts.first()?,
                &mut state,
            )
            .is_empty()
            || state.member_lookup_incomplete
            || state.receiver_resolution_uncertain()
        {
            return None;
        }
        let (prefix_len, unit_uris) =
            self.longest_visible_unit_prefix(uri, document, offset, &parts)?;
        (cursor_index < prefix_len).then(|| self.unit_candidates(unit_uris))
    }

    fn unit_reference_candidates_at_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Candidate>>, String> {
        if let Some(unit_name) =
            use_name_at_with_budget(identifier, &document.source, cancel, budget)?
        {
            return Ok(Some(self.unit_references_with_budget(
                document, &unit_name, cancel, budget,
            )?));
        }

        if is_unit_declaration_identifier(identifier) {
            budget.require_work(1, cancel)?;
            budget.require_bytes(uri.as_str().len(), cancel)?;
            let Some(index) = document
                .symbols
                .iter()
                .position(|symbol| symbol.kind == SymbolKind::Unit)
            else {
                return Ok(Some(Vec::new()));
            };
            return Ok(Some(vec![Candidate {
                uri: uri.clone(),
                index,
            }]));
        }

        let Some((path_node, parts, cursor_index)) =
            qualified_path_at_with_budget(identifier, &document.source, cancel, budget)?
        else {
            return Ok(None);
        };
        let identifiers = identifier_nodes(path_node);
        budget.require_work(identifiers.len(), cancel)?;
        let Some(first) = identifiers.first().copied() else {
            return Ok(None);
        };
        let Some(first_name) = parts.first() else {
            return Ok(None);
        };
        let mut state = ResolutionState::new();
        if !self
            .unqualified_references_with_budget_and_state(
                uri,
                document,
                first.start_byte(),
                first_name,
                first,
                &mut state,
                cancel,
                budget,
            )?
            .is_empty()
            || state.member_lookup_incomplete
            || state.receiver_resolution_uncertain()
        {
            return Ok(None);
        }
        let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            uri, document, offset, &parts, cancel, budget,
        )?
        else {
            return Ok(None);
        };
        if cursor_index >= prefix_len {
            return Ok(None);
        }
        Ok(Some(
            self.unit_candidates_with_budget(unit_uris, cancel, budget)?,
        ))
    }

    /// Return the physical byte span of the selected unit spelling.  Unit
    /// references deliberately use the complete bound prefix (for example
    /// `Ns.Provider` in `Ns.Provider.TThing`) rather than one arbitrary
    /// component.  This keeps declaration, `uses`, and qualified references
    /// stable for clients and lets the caller map one semantic occurrence to
    /// one exact UTF-16 range.
    fn unit_occurrence_span(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
        offset: usize,
        _cancel: Option<&AtomicBool>,
    ) -> Option<Span> {
        if let Some(module_name) = enclosing_module_name(identifier) {
            if is_unit_declaration_module(module_name) || has_ancestor_kind(module_name, "declUses")
            {
                return Some(Span::from_node(module_name));
            }
        }

        let (path_node, parts, cursor_index) = qualified_path_at(identifier, &document.source)?;
        let (prefix_len, _) = self.longest_visible_unit_prefix(uri, document, offset, &parts)?;
        if cursor_index >= prefix_len {
            return None;
        }
        let identifiers = identifier_nodes(path_node);
        #[cfg(test)]
        if let Some(cancel) = _cancel {
            test_record_unit_path_nodes(identifiers.len(), cancel);
        }
        let first = identifiers.first()?;
        let last = identifiers.get(prefix_len.saturating_sub(1))?;
        Some(Span {
            start: first.start_byte(),
            end: last.end_byte(),
        })
    }

    fn unit_occurrence_spans_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<HashMap<Span, Span>, String> {
        let mut spans = HashMap::new();
        let mut cursor = document.tree.root_node().walk();
        loop {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let node = cursor.node();
            if node.kind() == "moduleName"
                && (is_unit_declaration_module(node)
                    || has_ancestor_kind_with_budget(node, "declUses", cancel, budget)?)
            {
                let identifiers = identifier_nodes_with_budget(node, cancel, budget)?;
                if let (Some(first), Some(last)) = (identifiers.first(), identifiers.last()) {
                    let span = Span {
                        start: first.start_byte(),
                        end: last.end_byte(),
                    };
                    budget.require_bytes(
                        identifiers
                            .len()
                            .saturating_mul(std::mem::size_of::<(Span, Span)>()),
                        cancel,
                    )?;
                    for identifier in identifiers {
                        check_navigation_cancel(cancel)?;
                        spans.insert(Span::from_node(identifier), span);
                    }
                }
            } else if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot")
                && !is_nested_qualified_identifier_node(node)
            {
                if let Some(parts) =
                    qualified_name_parts_with_budget(node, &document.source, cancel, budget)?
                        .filter(|parts| parts.len() > 1)
                {
                    if let Some((prefix_len, _)) = self.longest_visible_unit_prefix_with_budget(
                        uri,
                        document,
                        node.start_byte(),
                        &parts,
                        cancel,
                        budget,
                    )? {
                        let identifiers = identifier_nodes_with_budget(node, cancel, budget)?;
                        if prefix_len <= identifiers.len() {
                            if let (Some(first), Some(last)) = (
                                identifiers.first(),
                                identifiers.get(prefix_len.saturating_sub(1)),
                            ) {
                                let span = Span {
                                    start: first.start_byte(),
                                    end: last.end_byte(),
                                };
                                budget.require_bytes(
                                    prefix_len.saturating_mul(std::mem::size_of::<(Span, Span)>()),
                                    cancel,
                                )?;
                                for identifier in identifiers.into_iter().take(prefix_len) {
                                    #[cfg(test)]
                                    test_record_unit_path_nodes(1, cancel);
                                    check_navigation_cancel(cancel)?;
                                    spans.insert(Span::from_node(identifier), span);
                                }
                            }
                        }
                    }
                }
            }

            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Ok(spans);
                }
            }
        }
    }

    fn resolve_candidates_at_with_state(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let name = node_text(identifier, &document.source);
        let direct =
            if !document.has_with_context_at(offset) || is_declaration_identifier(identifier) {
                self.direct_symbol_references(uri, identifier)
            } else {
                None
            };
        let candidates = if let Some(unit_references) =
            self.unit_reference_candidates_at(uri, document, offset, identifier)
        {
            unit_references
        } else if let Some(unit_name) = use_name_at(identifier, &document.source) {
            self.unit_references(document, &unit_name)
        } else if let Some(direct) = direct {
            direct
        } else if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            generic
        } else if let Some((path, cursor_index)) =
            qualified_type_path_at(identifier, &document.source)
        {
            self.type_reference_candidates(uri, document, offset, &path, cursor_index)
        } else if let Some(dot) = member_expression_at(identifier) {
            if is_right_hand_member(dot, identifier) {
                self.member_references_with_state(uri, document, offset, dot, &name, state)
            } else {
                self.unqualified_references_with_state(uri, document, offset, &name, state)
            }
        } else {
            self.unqualified_references_with_state(uri, document, offset, &name, state)
        };
        let candidates =
            self.filter_accessible_candidates_with_state(uri, document, offset, candidates, state);
        if state.receiver_resolution_uncertain() {
            Vec::new()
        } else {
            candidates
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_candidates_at_with_state_and_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        state: &mut ResolutionState,
        depth: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let name = node_text_with_budget(identifier, &document.source, cancel, budget)?;
        let direct =
            if !document.has_with_context_at(offset) || is_declaration_identifier(identifier) {
                self.direct_symbol_references_with_budget(uri, identifier, cancel, budget)?
            } else {
                None
            };
        let candidates = if let Some(unit_references) = self
            .unit_reference_candidates_at_with_budget(
                uri, document, offset, identifier, cancel, budget,
            )? {
            Ok(unit_references)
        } else if let Some(unit_name) =
            use_name_at_with_budget(identifier, &document.source, cancel, budget)?
        {
            self.unit_references_with_budget(document, &unit_name, cancel, budget)
        } else if let Some(direct) = direct {
            Ok(direct)
        } else if let Some(generic) = self
            .generic_parameter_references_with_budget(uri, document, identifier, cancel, budget)?
        {
            budget.require_work(generic.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(generic.len()), cancel)?;
            Ok(generic)
        } else if let Some((path, cursor_index)) =
            qualified_type_path_at_with_budget(identifier, &document.source, cancel, budget)?
        {
            self.type_reference_candidates_with_budget(
                uri,
                document,
                offset,
                identifier,
                &path,
                cursor_index,
                cancel,
                budget,
            )
        } else if let Some(dot) = member_expression_at(identifier) {
            if is_right_hand_member(dot, identifier) {
                self.member_references_with_state_and_budget(
                    uri, document, offset, dot, name, identifier, state, depth, cancel, budget,
                )
            } else {
                self.unqualified_references_with_budget_and_state(
                    uri, document, offset, name, identifier, state, cancel, budget,
                )
            }
        } else {
            self.unqualified_references_with_budget_and_state(
                uri, document, offset, name, identifier, state, cancel, budget,
            )
        }?;
        let candidates = self.filter_accessible_candidates_with_state_and_budget(
            uri, document, offset, candidates, state, cancel, budget,
        )?;
        Ok(if state.receiver_resolution_uncertain() {
            Vec::new()
        } else {
            candidates
        })
    }

    fn direct_symbol_references_with_budget(
        &self,
        uri: &Url,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Candidate>>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let span = Span::from_node(identifier);
        let Some(direct) = document.direct_symbol_indices.get(&span) else {
            return Ok(None);
        };
        let mut result_capacity = 0usize;
        for index in direct {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    result_capacity = result_capacity.saturating_add(
                        document
                            .routine_symbol_indices
                            .get(routine_key)
                            .map_or(0, Vec::len),
                    );
                } else {
                    result_capacity = result_capacity.saturating_add(1);
                }
            } else {
                result_capacity = result_capacity.saturating_add(1);
            }
        }
        budget.require_work(result_capacity, cancel)?;
        budget.require_bytes(uri.as_str().len().saturating_mul(result_capacity), cancel)?;
        let mut references = Vec::with_capacity(result_capacity);
        for index in direct {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    if let Some(candidate_indices) =
                        document.routine_symbol_indices.get(routine_key)
                    {
                        for candidate_index in candidate_indices {
                            check_navigation_cancel(cancel)?;
                            references.push(Candidate {
                                uri: uri.clone(),
                                index: *candidate_index,
                            });
                        }
                    }
                }
            } else {
                references.push(Candidate {
                    uri: uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(Some(references))
    }

    fn unit_references_with_budget(
        &self,
        current_document: &Document,
        name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_owned_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        let urls = self.unit_urls_for_import_with_budget(current_document, &key, cancel, budget)?;
        let mut result_capacity = 0usize;
        let mut result_uri_bytes = 0usize;
        for uri in &urls {
            let Some(document) = self.documents.get(uri) else {
                continue;
            };
            budget.require_bytes(document.unit_name.len(), cancel)?;
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(ROOT_SCOPE, document.unit_name.clone()))
            else {
                continue;
            };
            result_capacity = result_capacity.saturating_add(indices.len());
            result_uri_bytes =
                result_uri_bytes.saturating_add(uri.as_str().len().saturating_mul(indices.len()));
        }
        budget.require_work(result_capacity, cancel)?;
        budget.require_bytes(result_uri_bytes, cancel)?;
        let mut result = Vec::with_capacity(result_capacity);
        for uri in urls {
            let Some(document) = self.documents.get(&uri) else {
                continue;
            };
            budget.require_bytes(document.unit_name.len(), cancel)?;
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(ROOT_SCOPE, document.unit_name.clone()))
            else {
                continue;
            };
            for index in indices {
                check_navigation_cancel(cancel)?;
                if document
                    .symbols
                    .get(*index)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                {
                    result.push(Candidate {
                        uri: uri.clone(),
                        index: *index,
                    });
                    break;
                }
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn unqualified_references_with_budget_and_state(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        if let Some(generic) = self
            .generic_parameter_references_with_budget(uri, document, identifier, cancel, budget)?
        {
            budget.require_work(generic.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(generic.len()), cancel)?;
            return Ok(generic);
        }
        let scope = document.scope_at(offset);
        self.unqualified_references_with_budget_at_scope_and_state(
            uri, document, offset, name, identifier, scope, state, cancel, budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn unqualified_references_with_budget_and_state_without_with(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let previous = state.suppress_with_lookup;
        state.suppress_with_lookup = true;
        let result = self.unqualified_references_with_budget_and_state(
            uri, document, offset, name, identifier, state, cancel, budget,
        );
        state.suppress_with_lookup = previous;
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn unqualified_references_with_budget_at_scope_and_state(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        scope: usize,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        if let Some(generic) = self
            .generic_parameter_references_with_budget(uri, document, identifier, cancel, budget)?
        {
            budget.require_work(generic.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(generic.len()), cancel)?;
            return Ok(generic
                .into_iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.key == key)
                })
                .collect());
        }
        if !state.suppress_with_lookup {
            match self.with_lookup_for_name_with_budget(
                uri, document, offset, name, state, cancel, budget,
            )? {
                WithLookup::Found(candidates) => return Ok(candidates),
                WithLookup::Unknown => {
                    state.mark_member_lookup_incomplete();
                    return Ok(Vec::new());
                }
                WithLookup::NotFound => {}
            }
        }
        let owner_type =
            document.owner_type_at_identifier_with_budget(identifier, scope, cancel, budget)?;

        let scope_chain = self.scope_chain_from_with_budget(document, scope, cancel, budget)?;
        budget.require_bytes(
            key.len()
                .saturating_mul(scope_chain.len().saturating_add(1)),
            cancel,
        )?;
        for scope_id in scope_chain {
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(scope_id, key.clone()))
            else {
                continue;
            };
            budget.require_work(indices.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(indices.len()), cancel)?;
            let mut local = Vec::with_capacity(indices.len());
            for index in indices {
                check_navigation_cancel(cancel)?;
                let Some(symbol) = document.symbols.get(*index) else {
                    continue;
                };
                if scope_id != ROOT_SCOPE
                    && !symbol.local_only
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                    && symbol.kind != SymbolKind::Unit
                    && !symbol.unresolved_abbreviated
                    && symbol_is_available_at(document, symbol, uri, uri, offset)
                {
                    local.push(Candidate {
                        uri: uri.clone(),
                        index: *index,
                    });
                }
            }
            if !local.is_empty() {
                return Ok(local);
            }
        }

        if let Some(owner_type) = owner_type.as_deref() {
            let helper_target = if document.offset_is_in_helper_declaration(offset) {
                OwnerHelperLookup::None
            } else {
                self.helper_target_for_owner_status_with_budget(
                    uri, document, owner_type, cancel, budget,
                )?
            };
            if helper_target == OwnerHelperLookup::Unknown {
                state.mark_member_lookup_incomplete();
            }
            let helper_target = helper_target.selected();
            let (member_uri, member_type, member_scope, member_substitution, helper_owner) =
                helper_target.map_or_else(
                    || {
                        (
                            uri.clone(),
                            owner_type.to_owned(),
                            ROOT_SCOPE,
                            GenericSubstitution::empty(),
                            None,
                        )
                    },
                    |target| {
                        (
                            target.uri,
                            target.key,
                            target.scope,
                            target.substitution,
                            target.helper_owner,
                        )
                    },
                );
            let members = if let Some(helper_owner) = helper_owner.as_ref() {
                self.member_candidates_for_helper_owner_in_context_with_budget(
                    uri,
                    &member_uri,
                    &member_type,
                    member_scope,
                    &member_substitution,
                    helper_owner,
                    Some(&key),
                    member_uri == *uri,
                    cancel,
                    budget,
                )?
            } else {
                self.member_references_for_type_in_context_with_budget(
                    uri,
                    document,
                    offset,
                    &member_uri,
                    &member_type,
                    member_scope,
                    &member_substitution,
                    &key,
                    member_uri == *uri,
                    cancel,
                    budget,
                )?
            };
            if !members.ancestry_known
                || (members.implicit_member_known && members.candidates.is_empty())
                || !members.ambiguous_names.is_empty()
                || members
                    .candidates
                    .iter()
                    .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                state.mark_member_lookup_incomplete();
            }
            if !members.candidates.is_empty() {
                return Ok(members.candidates);
            }
        }

        let region = document.region_at(offset);
        let Some(root_indices) = document
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, key.clone()))
        else {
            return self.imported_references_with_budget(
                document,
                region,
                &key,
                identifier,
                owner_type.as_deref(),
                state,
                cancel,
                budget,
            );
        };
        budget.require_work(root_indices.len(), cancel)?;
        budget.require_bytes(
            uri.as_str().len().saturating_mul(root_indices.len()),
            cancel,
        )?;
        let mut current = Vec::with_capacity(root_indices.len());
        for index in root_indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if !symbol.local_only
                && symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
                && symbol.kind != SymbolKind::Unit
                && !symbol.unresolved_abbreviated
                && symbol_is_available_at(document, symbol, uri, uri, offset)
                && symbol_visible_in_region(symbol, region)
            {
                current.push(Candidate {
                    uri: uri.clone(),
                    index: *index,
                });
            }
        }
        if !current.is_empty() {
            if self.is_unknown_global_fallback_for_owner(
                document,
                identifier,
                owner_type.as_deref(),
                &current,
            ) {
                state.mark_receiver_uncertain();
                return Ok(Vec::new());
            }
            return Ok(current);
        }

        self.imported_references_with_budget(
            document,
            region,
            &key,
            identifier,
            owner_type.as_deref(),
            state,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn imported_references_with_budget(
        &self,
        document: &Document,
        region: Region,
        key: &str,
        identifier: Node<'_>,
        owner_type: Option<&str>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let active_uses = document.active_uses_with_budget(region, cancel, budget)?;
        let mut imported = Vec::new();
        for unit in active_uses {
            check_navigation_cancel(cancel)?;
            if document.unknown_imports.contains(unit.as_str()) {
                return Ok(Vec::new());
            }
            for unit_uri in self.unit_urls_for_import_with_budget(document, unit, cancel, budget)? {
                imported.extend(
                    self.exported_references_for_key_with_budget(&unit_uri, key, cancel, budget)?,
                );
            }
        }
        if !imported.is_empty() {
            if self
                .is_unknown_global_fallback_for_owner(document, identifier, owner_type, &imported)
            {
                state.mark_receiver_uncertain();
                return Ok(Vec::new());
            }
            return Ok(imported);
        }

        let implicit_system =
            self.implicit_system_references_with_budget(key, state, cancel, budget)?;
        if !implicit_system.is_empty() {
            return Ok(implicit_system);
        }

        Ok(Vec::new())
    }

    fn implicit_system_references_with_budget(
        &self,
        key: &str,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let status = self.implicit_system_namespace_status();
        let ImplicitSystemNamespaceStatus::SourceBacked { uri } = status else {
            // The compiler's implicit System namespace is not represented by
            // a complete source catalogue here.  A failed value lookup,
            // including an assignment target, therefore cannot prove that
            // the name is absent: compiler-provided writable globals may be
            // missing from the retained source index.
            state.mark_implicit_system_namespace_incomplete();
            return Ok(Vec::new());
        };
        let Some(provider) = self.documents.get(&uri) else {
            state.mark_implicit_system_namespace_incomplete();
            return Ok(Vec::new());
        };
        budget.require_work(
            provider
                .parser_recovery_spans
                .len()
                .saturating_add(provider.conditionals.unknown_spans.len()),
            cancel,
        )?;
        if !provider.parser_recovery_spans.is_empty()
            || !provider.conditionals.unknown_spans.is_empty()
        {
            state.mark_implicit_system_namespace_incomplete();
            return Ok(Vec::new());
        }

        let candidates = self.exported_references_for_key_with_budget(&uri, key, cancel, budget)?;
        if candidates.is_empty() {
            // Source-backed names are usable as positive evidence, but an
            // arbitrary System.pas does not establish the compiler's full
            // implicit export surface.
            state.mark_implicit_system_namespace_incomplete();
        }
        Ok(candidates)
    }

    fn implicit_system_namespace_status(&self) -> ImplicitSystemNamespaceStatus {
        let Some(urls) = self.units.get("system") else {
            return ImplicitSystemNamespaceStatus::Unavailable;
        };
        let Some(uri) = urls.first().cloned() else {
            return ImplicitSystemNamespaceStatus::Unavailable;
        };
        if urls.len() == 1 {
            ImplicitSystemNamespaceStatus::SourceBacked { uri }
        } else {
            ImplicitSystemNamespaceStatus::Ambiguous
        }
    }

    fn exported_references_for_key_with_budget(
        &self,
        uri: &Url,
        key: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        budget.require_bytes(key.len(), cancel)?;
        let key = key.to_owned();
        let Some(indices) = document.symbol_indices_by_scope_key.get(&(ROOT_SCOPE, key)) else {
            return Ok(Vec::new());
        };
        budget.require_work(indices.len(), cancel)?;
        budget.require_bytes(uri.as_str().len().saturating_mul(indices.len()), cancel)?;
        #[cfg(test)]
        test_record_materialization(&TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS);
        let mut result = Vec::with_capacity(indices.len());
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
                && symbol.kind != SymbolKind::Unit
                && !symbol.local_only
                && (symbol.region == Region::Interface
                    || (symbol.kind == SymbolKind::Routine
                        && symbol.origin == Origin::Definition
                        && symbol.routine_key.as_ref().is_some_and(|routine_key| {
                            document.interface_routine_keys.contains(routine_key)
                        })))
            {
                result.push(Candidate {
                    uri: uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        dot: Node<'_>,
        member_name: &str,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        depth: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            state.mark_member_lookup_incomplete();
            return Ok(Vec::new());
        };
        budget.require_bytes(member_name.len(), cancel)?;
        let key = canonical_name(member_name);
        let receivers = self.resolve_receivers_with_state_and_budget(
            current_uri,
            current_document,
            offset,
            lhs,
            lookup_identifier,
            state,
            cancel,
            budget,
            depth.saturating_add(1),
        )?;
        if receivers.is_empty() {
            state.mark_member_lookup_incomplete();
            return Ok(Vec::new());
        }
        if receivers.len() != 1 {
            state.mark_member_lookup_incomplete();
            state.mark_ambiguous();
        }
        let mut references = Vec::new();
        for receiver in receivers {
            budget.require_work(1, cancel)?;
            match receiver {
                Receiver::Unit(unit_uri) => {
                    references.extend(self.exported_references_for_key_with_budget(
                        &unit_uri, &key, cancel, budget,
                    )?);
                }
                Receiver::Type(instance) => {
                    let lookup = self.member_references_for_instance_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        &instance,
                        &key,
                        instance.uri == *current_uri,
                        cancel,
                        budget,
                    )?;
                    if !lookup.ancestry_known
                        || (lookup.implicit_member_known && lookup.candidates.is_empty())
                        || lookup.ambiguous_names.contains(&key)
                    {
                        state.mark_member_lookup_incomplete();
                    }
                    if lookup
                        .candidates
                        .iter()
                        .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
                    {
                        state.mark_member_lookup_incomplete();
                    }
                    references.extend(lookup.candidates);
                }
                Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => {
                    state.mark_member_lookup_incomplete();
                }
            }
        }
        Ok(references)
    }

    fn type_reference_candidates(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        cursor_index: usize,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        if let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, parts)
        {
            if cursor_index < prefix_len {
                return self.unit_candidates(unit_uris);
            }
            if cursor_index == prefix_len && parts.len() == prefix_len + 1 {
                let Some(type_name) = parts.get(prefix_len) else {
                    return Vec::new();
                };
                let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
                return unit_uris
                    .iter()
                    .flat_map(|unit_uri| {
                        self.type_candidates_in_unit(unit_uri, type_name, allow_implementation)
                    })
                    .collect();
            }
        }
        self.type_receivers_for_parts(
            current_uri,
            current_document,
            offset,
            parts,
            None,
            &mut state,
        )
        .into_iter()
        .flat_map(|receiver| match receiver {
            Receiver::Type(instance) => self.type_candidates_in_unit(
                &instance.uri,
                &instance.key,
                instance.uri == *current_uri,
            ),
            Receiver::Unit(_) => Vec::new(),
            Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Vec::new(),
        })
        .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn type_reference_candidates_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        lookup_identifier: Node<'_>,
        parts: &[String],
        cursor_index: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_work(parts.len(), cancel)?;
        let mut state = ResolutionState::new();
        if parts.len() == 1 {
            let candidates = self.unqualified_references_with_budget_and_state(
                current_uri,
                current_document,
                offset,
                &parts[0],
                lookup_identifier,
                &mut state,
                cancel,
                budget,
            )?;
            if candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            return Ok(candidates
                .into_iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.kind == SymbolKind::Type)
                })
                .collect());
        }
        if let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )? {
            if cursor_index < prefix_len {
                return self.unit_candidates_with_budget(unit_uris, cancel, budget);
            }
            if cursor_index == prefix_len && parts.len() == prefix_len + 1 {
                let Some(type_name) = parts.get(prefix_len) else {
                    return Ok(Vec::new());
                };
                let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
                let mut result = Vec::new();
                for unit_uri in unit_uris {
                    result.extend(self.type_candidates_in_unit_with_budget(
                        &unit_uri,
                        type_name,
                        allow_implementation,
                        cancel,
                        budget,
                    )?);
                }
                return Ok(result);
            }
        }
        let receivers = self.resolve_qualified_receiver_path_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            lookup_identifier,
            &mut state,
            cancel,
            budget,
        )?;
        let mut result = Vec::new();
        for receiver in receivers {
            budget.require_work(1, cancel)?;
            if let Receiver::Type(instance) = receiver {
                result.extend(self.type_candidates_in_unit_with_budget(
                    &instance.uri,
                    &instance.key,
                    instance.uri == *current_uri,
                    cancel,
                    budget,
                )?);
            }
        }
        Ok(result)
    }

    fn unit_candidates_with_budget(
        &self,
        unit_uris: Vec<Url>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let mut result = Vec::new();
        for uri in unit_uris {
            budget.require_work(1, cancel)?;
            budget.require_bytes(uri.as_str().len(), cancel)?;
            let Some(document) = self.documents.get(&uri) else {
                continue;
            };
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(ROOT_SCOPE, document.unit_name.clone()))
            else {
                continue;
            };
            for index in indices {
                budget.require_work(1, cancel)?;
                if document
                    .symbols
                    .get(*index)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                {
                    result.push(Candidate {
                        uri: uri.clone(),
                        index: *index,
                    });
                    break;
                }
            }
        }
        Ok(result)
    }

    fn unit_candidates(&self, unit_uris: Vec<Url>) -> Vec<Candidate> {
        unit_uris
            .into_iter()
            .filter_map(|uri| {
                let document = self.documents.get(&uri)?;
                let index = document
                    .symbols
                    .iter()
                    .position(|symbol| symbol.kind == SymbolKind::Unit)?;
                Some(Candidate { uri, index })
            })
            .collect()
    }

    fn remove_uri_from_unit(&mut self, unit_name: &str, uri: &Url) {
        let mut remove_unit = false;
        if let Some(urls) = self.units.get_mut(unit_name) {
            urls.retain(|candidate| candidate != uri);
            remove_unit = urls.is_empty();
        }
        if remove_unit {
            self.units.remove(unit_name);
        }
    }

    fn direct_symbol_references(&self, uri: &Url, identifier: Node<'_>) -> Option<Vec<Candidate>> {
        let document = self.documents.get(uri)?;
        let span = Span::from_node(identifier);
        let direct = document.direct_symbol_indices.get(&span)?.clone();
        if direct.is_empty() {
            return None;
        }

        let mut references = Vec::new();
        for index in direct {
            let symbol = &document.symbols[index];
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    for candidate_index in document
                        .routine_symbol_indices
                        .get(routine_key)
                        .into_iter()
                        .flatten()
                        .copied()
                    {
                        references.push(Candidate {
                            uri: uri.clone(),
                            index: candidate_index,
                        });
                    }
                }
            } else {
                references.push(Candidate {
                    uri: uri.clone(),
                    index,
                });
            }
        }
        Some(references)
    }

    fn unit_references(&self, current_document: &Document, name: &str) -> Vec<Candidate> {
        let key = canonical_name(name);
        self.unit_urls_for_import(current_document, &key)
            .into_iter()
            .filter_map(|uri| {
                let document = self.documents.get(&uri)?;
                let index = document
                    .symbol_indices_by_scope_key
                    .get(&(ROOT_SCOPE, document.unit_name.clone()))?
                    .iter()
                    .copied()
                    .find(|index| {
                        document
                            .symbols
                            .get(*index)
                            .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                    })?;
                Some(Candidate {
                    uri: uri.clone(),
                    index,
                })
            })
            .collect()
    }

    fn unit_urls_for_import(&self, current_document: &Document, name: &str) -> Vec<Url> {
        if current_document.unknown_imports.contains(name) {
            return Vec::new();
        }
        if let Some(bindings) = &current_document.import_bindings {
            return bindings.get(name).cloned().into_iter().collect();
        }
        self.units.get(name).cloned().unwrap_or_default()
    }

    fn unit_urls_for_import_with_budget(
        &self,
        current_document: &Document,
        name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Url>, String> {
        if current_document.unknown_imports.contains(name) {
            return Ok(Vec::new());
        }
        if let Some(bindings) = &current_document.import_bindings {
            let Some(uri) = bindings.get(name) else {
                return Ok(Vec::new());
            };
            budget.require_work(1, cancel)?;
            budget.require_bytes(uri.as_str().len(), cancel)?;
            #[cfg(test)]
            test_record_materialization(&TEST_UNIT_URL_VECTOR_MATERIALIZATIONS);
            return Ok(vec![uri.clone()]);
        }
        let Some(urls) = self.units.get(name) else {
            return Ok(Vec::new());
        };
        budget.require_work(urls.len(), cancel)?;
        if urls.is_empty() {
            return Ok(Vec::new());
        }
        budget.require_bytes(
            urls.iter().map(|url| url.as_str().len()).sum::<usize>(),
            cancel,
        )?;
        #[cfg(test)]
        test_record_materialization(&TEST_UNIT_URL_VECTOR_MATERIALIZATIONS);
        Ok(urls.clone())
    }

    fn generic_parameter_references(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
    ) -> Option<Vec<Candidate>> {
        if is_identifier_in_qualified_path(identifier, &document.source) {
            return None;
        }
        let span = Span::from_node(identifier);
        let key = canonical_name(&node_text(identifier, &document.source));
        let routine_keys = document
            .symbols
            .iter()
            .filter(|symbol| {
                #[cfg(test)]
                test_record_materialization(&TEST_SEMANTIC_GENERIC_SYMBOL_VISITS);
                symbol.kind == SymbolKind::Routine
                    && !symbol.generic_parameters.is_empty()
                    && symbol.declaration_span.contains(span)
                    && symbol
                        .generic_parameters
                        .iter()
                        .any(|parameter| parameter.name == key)
            })
            .filter_map(|symbol| symbol.routine_key.as_ref())
            .collect::<HashSet<_>>();
        if routine_keys.is_empty() {
            return None;
        }
        let candidates = document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                #[cfg(test)]
                test_record_materialization(&TEST_SEMANTIC_GENERIC_SYMBOL_VISITS);
                symbol.kind == SymbolKind::Type
                    && symbol.generic_parameter.as_deref() == Some(key.as_str())
                    && symbol
                        .routine_key
                        .as_ref()
                        .is_some_and(|routine_key| routine_keys.contains(routine_key))
            })
            .map(|(index, _)| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect::<Vec<_>>();
        (!candidates.is_empty()).then_some(candidates)
    }

    #[allow(clippy::too_many_arguments)]
    fn generic_parameter_references_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Candidate>>, String> {
        if is_identifier_in_qualified_path_with_budget(identifier, cancel, budget)? {
            return Ok(None);
        }
        let span = Span::from_node(identifier);
        let key = canonical_name(node_text_with_budget(
            identifier,
            &document.source,
            cancel,
            budget,
        )?);
        let mut routine_keys = HashSet::new();
        for symbol in &document.symbols {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            budget.require_bytes(symbol.key.len().saturating_add(symbol.name.len()), cancel)?;
            #[cfg(test)]
            test_record_materialization(&TEST_SEMANTIC_GENERIC_SYMBOL_VISITS);
            if symbol.kind == SymbolKind::Routine
                && !symbol.generic_parameters.is_empty()
                && symbol.declaration_span.contains(span)
                && symbol
                    .generic_parameters
                    .iter()
                    .any(|parameter| parameter.name == key)
            {
                if let Some(routine_key) = symbol.routine_key.as_ref() {
                    routine_keys.insert(routine_key.clone());
                }
            }
        }
        if routine_keys.is_empty() {
            return Ok(None);
        }

        let mut candidates = Vec::new();
        for (index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            budget.require_bytes(symbol.key.len().saturating_add(symbol.name.len()), cancel)?;
            #[cfg(test)]
            test_record_materialization(&TEST_SEMANTIC_GENERIC_SYMBOL_VISITS);
            if symbol.kind == SymbolKind::Type
                && symbol.generic_parameter.as_deref() == Some(key.as_str())
                && symbol
                    .routine_key
                    .as_ref()
                    .is_some_and(|routine_key| routine_keys.contains(routine_key))
            {
                candidates.push(Candidate {
                    uri: uri.clone(),
                    index,
                });
            }
        }
        Ok((!candidates.is_empty()).then_some(candidates))
    }

    fn with_lookup_for_name(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        state: &mut ResolutionState,
    ) -> WithLookup {
        let key = canonical_name(name);

        let overlay = state.with_receivers.clone();
        if !overlay.is_empty() {
            for slot in overlay.iter().rev() {
                match slot {
                    WithReceiverSlot::Known(receivers) => {
                        for receiver in receivers.iter().rev() {
                            match self
                                .with_receiver_lookup(uri, document, offset, receiver, &key, state)
                            {
                                WithLookup::NotFound => {}
                                lookup => return lookup,
                            }
                        }
                    }
                    WithReceiverSlot::Unknown => return WithLookup::Unknown,
                }
            }
        }

        if overlay.is_empty() {
            for (context, receiver_index) in document.with_receiver_contexts_at(offset) {
                let Some(receivers) = self.resolve_with_context_receiver_prefix(
                    uri,
                    document,
                    context,
                    receiver_index,
                    state,
                ) else {
                    return WithLookup::Unknown;
                };
                for slot in receivers.iter().rev() {
                    match slot {
                        WithReceiverSlot::Known(receivers) => {
                            for receiver in receivers.iter().rev() {
                                match self.with_receiver_lookup(
                                    uri, document, offset, receiver, &key, state,
                                ) {
                                    WithLookup::NotFound => {}
                                    lookup => return lookup,
                                }
                            }
                        }
                        WithReceiverSlot::Unknown => return WithLookup::Unknown,
                    }
                }
            }
        }

        let contexts = document.with_contexts_at(offset);
        if contexts.len() > MAX_WITH_CONTEXT_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return WithLookup::Unknown;
        }
        for context in contexts {
            let Some(receiver_slots) =
                self.resolve_with_context_receivers(uri, document, context, state)
            else {
                return WithLookup::Unknown;
            };
            for slot in receiver_slots.iter().rev() {
                match slot {
                    WithReceiverSlot::Known(receivers) => {
                        for receiver in receivers.iter().rev() {
                            match self
                                .with_receiver_lookup(uri, document, offset, receiver, &key, state)
                            {
                                WithLookup::NotFound => {}
                                lookup => return lookup,
                            }
                        }
                    }
                    WithReceiverSlot::Unknown => return WithLookup::Unknown,
                }
            }
        }

        WithLookup::NotFound
    }

    fn resolve_with_context_receivers(
        &self,
        uri: &Url,
        document: &Document,
        context: &WithContext,
        state: &mut ResolutionState,
    ) -> Option<Vec<WithReceiverSlot>> {
        let cache_key = (uri.clone(), context.body);
        if let Some(cached) = state.with_context_resolutions.get(&cache_key) {
            if let Some(receivers) = cached {
                return Some(receivers.clone());
            }
            state.mark_receiver_uncertain();
            return None;
        }
        if state.with_context_depth >= MAX_WITH_CONTEXT_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            state.with_context_resolutions.insert(cache_key, None);
            return None;
        }
        state.with_context_depth += 1;
        let initial_overlay = state.with_receivers.clone();
        let mut overlay = initial_overlay.clone();
        let mut receiver_slots = Vec::new();
        let mut result = Some(Vec::new());
        for receiver_span in &context.receiver_spans {
            let Some(node) = document
                .tree
                .root_node()
                .named_descendant_for_byte_range(receiver_span.start, receiver_span.end)
                .filter(|node| Span::from_node(*node) == *receiver_span)
            else {
                result = None;
                break;
            };
            state.with_receivers = overlay.clone();
            let uncertain_before = state.receiver_resolution_uncertain();
            let resolved =
                self.resolve_receivers_with_state(uri, document, receiver_span.start, node, state);
            let slot_unknown = resolved.is_empty() || state.receiver_resolution_uncertain();
            state.receiver_uncertain = uncertain_before;
            state.with_receivers = initial_overlay.clone();
            if slot_unknown {
                receiver_slots.push(WithReceiverSlot::Unknown);
                overlay.push(WithReceiverSlot::Unknown);
                state.with_receivers = overlay.clone();
                continue;
            }
            receiver_slots.push(WithReceiverSlot::Known(resolved.clone()));
            overlay.push(WithReceiverSlot::Known(resolved.clone()));
            state.with_receivers = overlay.clone();
        }
        state.with_receivers = initial_overlay;
        if result.is_some() {
            result = Some(receiver_slots);
        }
        state
            .with_context_resolutions
            .insert(cache_key, result.clone());
        state.with_context_depth -= 1;
        result
    }

    fn resolve_with_context_receiver_prefix(
        &self,
        uri: &Url,
        document: &Document,
        context: &WithContext,
        receiver_index: usize,
        state: &mut ResolutionState,
    ) -> Option<Vec<WithReceiverSlot>> {
        if receiver_index == 0 {
            return Some(Vec::new());
        }
        if state.with_context_depth >= MAX_WITH_CONTEXT_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return None;
        }
        state.with_context_depth += 1;
        let previous = state.with_receivers.clone();
        let mut overlay = previous.clone();
        let mut receiver_slots = Vec::new();
        let mut result = Some(Vec::new());
        for receiver_span in context.receiver_spans.iter().take(receiver_index) {
            let Some(node) = document
                .tree
                .root_node()
                .named_descendant_for_byte_range(receiver_span.start, receiver_span.end)
                .filter(|node| Span::from_node(*node) == *receiver_span)
            else {
                result = None;
                break;
            };
            state.with_receivers = overlay.clone();
            let uncertain_before = state.receiver_resolution_uncertain();
            let resolved =
                self.resolve_receivers_with_state(uri, document, receiver_span.start, node, state);
            state.with_receivers = previous.clone();
            if (!uncertain_before && state.receiver_resolution_uncertain()) || resolved.is_empty() {
                state.mark_receiver_uncertain();
                result = None;
                break;
            }
            receiver_slots.push(WithReceiverSlot::Known(resolved.clone()));
            overlay.push(WithReceiverSlot::Known(resolved));
        }
        state.with_receivers = previous;
        state.with_context_depth -= 1;
        if result.is_some() {
            result = Some(receiver_slots);
        }
        result
    }

    fn with_receiver_lookup(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        receiver: &Receiver,
        key: &str,
        state: &mut ResolutionState,
    ) -> WithLookup {
        let Receiver::Type(instance) = receiver else {
            return WithLookup::NotFound;
        };
        let lookup = self.member_references_for_instance(
            uri,
            document,
            offset,
            instance,
            key,
            instance.uri == *uri,
        );
        if !lookup.ancestry_known
            || (lookup.implicit_member_known && lookup.candidates.is_empty())
            || lookup.ambiguous_names.contains(key)
            || lookup
                .candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            state.mark_receiver_uncertain();
            return WithLookup::Unknown;
        }
        if lookup.candidates.is_empty() {
            WithLookup::NotFound
        } else {
            self.remember_with_member_substitutions(
                uri,
                offset,
                instance,
                &lookup.candidates,
                state,
            );
            WithLookup::Found(lookup.candidates)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn with_lookup_for_name_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<WithLookup, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        let overlay = state.with_receivers.clone();
        for slot in overlay.iter().rev() {
            match slot {
                WithReceiverSlot::Known(receivers) => {
                    for receiver in receivers.iter().rev() {
                        match self.with_receiver_lookup_with_budget(
                            uri, document, offset, receiver, &key, state, cancel, budget,
                        )? {
                            WithLookup::NotFound => {}
                            lookup => return Ok(lookup),
                        }
                    }
                }
                WithReceiverSlot::Unknown => return Ok(WithLookup::Unknown),
            }
        }

        if overlay.is_empty() {
            let receiver_contexts = document.with_receiver_contexts_at(offset);
            budget.require_work(receiver_contexts.len(), cancel)?;
            for (context, receiver_index) in receiver_contexts {
                let Some(receivers) = self.resolve_with_context_receiver_prefix_with_budget(
                    uri,
                    document,
                    context,
                    receiver_index,
                    state,
                    cancel,
                    budget,
                )?
                else {
                    return Ok(WithLookup::Unknown);
                };
                for slot in receivers.iter().rev() {
                    match slot {
                        WithReceiverSlot::Known(receivers) => {
                            for receiver in receivers.iter().rev() {
                                match self.with_receiver_lookup_with_budget(
                                    uri, document, offset, receiver, &key, state, cancel, budget,
                                )? {
                                    WithLookup::NotFound => {}
                                    lookup => return Ok(lookup),
                                }
                            }
                        }
                        WithReceiverSlot::Unknown => return Ok(WithLookup::Unknown),
                    }
                }
            }
        }

        let contexts = document.with_contexts_at(offset);
        if contexts.len() > MAX_WITH_CONTEXT_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return Ok(WithLookup::Unknown);
        }
        budget.require_work(contexts.len(), cancel)?;
        for context in contexts {
            let Some(receiver_slots) = self.resolve_with_context_receivers_with_budget(
                uri, document, context, state, cancel, budget,
            )?
            else {
                return Ok(WithLookup::Unknown);
            };
            for slot in receiver_slots.iter().rev() {
                match slot {
                    WithReceiverSlot::Known(receivers) => {
                        for receiver in receivers.iter().rev() {
                            match self.with_receiver_lookup_with_budget(
                                uri, document, offset, receiver, &key, state, cancel, budget,
                            )? {
                                WithLookup::NotFound => {}
                                lookup => return Ok(lookup),
                            }
                        }
                    }
                    WithReceiverSlot::Unknown => return Ok(WithLookup::Unknown),
                }
            }
        }

        Ok(WithLookup::NotFound)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_with_context_receivers_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        context: &WithContext,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<WithReceiverSlot>>, String> {
        let cache_key = (uri.clone(), context.body);
        if let Some(cached) = state.with_context_resolutions.get(&cache_key) {
            if let Some(receivers) = cached {
                return Ok(Some(receivers.clone()));
            }
            state.mark_receiver_uncertain();
            return Ok(None);
        }
        if state.with_context_depth >= MAX_WITH_CONTEXT_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            state.with_context_resolutions.insert(cache_key, None);
            return Ok(None);
        }
        state.with_context_depth += 1;
        let result = (|| {
            budget.require_work(context.receiver_spans.len(), cancel)?;
            let initial_overlay = state.with_receivers.clone();
            let mut overlay = initial_overlay.clone();
            let mut receiver_slots = Vec::new();
            for receiver_span in &context.receiver_spans {
                check_navigation_cancel(cancel)?;
                let Some(node) = document
                    .tree
                    .root_node()
                    .named_descendant_for_byte_range(receiver_span.start, receiver_span.end)
                    .filter(|node| Span::from_node(*node) == *receiver_span)
                else {
                    state.mark_receiver_uncertain();
                    return Ok(None);
                };
                state.with_receivers = overlay.clone();
                let uncertain_before = state.receiver_resolution_uncertain();
                let resolved = self.resolve_receivers_with_state_and_budget(
                    uri,
                    document,
                    receiver_span.start,
                    node,
                    node,
                    state,
                    cancel,
                    budget,
                    0,
                );
                let slot_unknown = resolved
                    .as_ref()
                    .map_or(true, |resolved| resolved.is_empty())
                    || state.receiver_resolution_uncertain();
                state.with_receivers = initial_overlay.clone();
                let resolved = resolved?;
                state.receiver_uncertain = uncertain_before;
                if slot_unknown {
                    receiver_slots.push(WithReceiverSlot::Unknown);
                    overlay.push(WithReceiverSlot::Unknown);
                    state.with_receivers = overlay.clone();
                    continue;
                }
                receiver_slots.push(WithReceiverSlot::Known(resolved.clone()));
                overlay.push(WithReceiverSlot::Known(resolved.clone()));
                state.with_receivers = overlay.clone();
            }
            state.with_receivers = initial_overlay;
            Ok(Some(receiver_slots))
        })();
        if let Ok(resolved) = &result {
            state
                .with_context_resolutions
                .insert(cache_key, resolved.clone());
        }
        state.with_context_depth -= 1;
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_with_context_receiver_prefix_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        context: &WithContext,
        receiver_index: usize,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<WithReceiverSlot>>, String> {
        if receiver_index == 0 {
            return Ok(Some(Vec::new()));
        }
        if state.with_context_depth >= MAX_WITH_CONTEXT_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return Ok(None);
        }
        state.with_context_depth += 1;
        let result = (|| {
            budget.require_work(receiver_index, cancel)?;
            let previous = state.with_receivers.clone();
            let mut overlay = previous.clone();
            let mut receiver_slots = Vec::new();
            for receiver_span in context.receiver_spans.iter().take(receiver_index) {
                check_navigation_cancel(cancel)?;
                let Some(node) = document
                    .tree
                    .root_node()
                    .named_descendant_for_byte_range(receiver_span.start, receiver_span.end)
                    .filter(|node| Span::from_node(*node) == *receiver_span)
                else {
                    state.mark_receiver_uncertain();
                    state.with_receivers = previous;
                    return Ok(None);
                };
                state.with_receivers = overlay.clone();
                let uncertain_before = state.receiver_resolution_uncertain();
                let resolved = self.resolve_receivers_with_state_and_budget(
                    uri,
                    document,
                    receiver_span.start,
                    node,
                    node,
                    state,
                    cancel,
                    budget,
                    0,
                )?;
                state.with_receivers = previous.clone();
                if (!uncertain_before && state.receiver_resolution_uncertain())
                    || resolved.is_empty()
                {
                    state.mark_receiver_uncertain();
                    return Ok(None);
                }
                receiver_slots.push(WithReceiverSlot::Known(resolved.clone()));
                overlay.push(WithReceiverSlot::Known(resolved));
            }
            state.with_receivers = previous;
            Ok(Some(receiver_slots))
        })();
        state.with_context_depth -= 1;
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn with_receiver_lookup_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        receiver: &Receiver,
        key: &str,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<WithLookup, String> {
        let Receiver::Type(instance) = receiver else {
            return Ok(WithLookup::NotFound);
        };
        let lookup = self.member_references_for_instance_with_budget(
            uri,
            document,
            offset,
            instance,
            key,
            instance.uri == *uri,
            cancel,
            budget,
        )?;
        if !lookup.ancestry_known
            || (lookup.implicit_member_known && lookup.candidates.is_empty())
            || lookup.ambiguous_names.contains(key)
            || lookup
                .candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            state.mark_receiver_uncertain();
            return Ok(WithLookup::Unknown);
        }
        if lookup.candidates.is_empty() {
            Ok(WithLookup::NotFound)
        } else {
            self.remember_with_member_substitutions_with_budget(
                uri,
                offset,
                instance,
                &lookup.candidates,
                state,
                cancel,
                budget,
            )?;
            Ok(WithLookup::Found(lookup.candidates))
        }
    }

    fn remember_with_member_substitutions(
        &self,
        current_uri: &Url,
        offset: usize,
        instance: &TypeInstance,
        candidates: &[Candidate],
        state: &mut ResolutionState,
    ) {
        for candidate in candidates {
            let Some(symbol) = self.symbol(candidate) else {
                continue;
            };
            let owner_key = symbol.owner_type.as_deref().unwrap_or(&instance.key);
            let substitution = self
                .member_owner_substitution(
                    &instance.uri,
                    &instance.key,
                    &instance.substitution,
                    &candidate.uri,
                    owner_key,
                    state,
                )
                .or_else(|| {
                    self.candidate_is_helper_member(candidate)
                        .then(|| instance.substitution.clone())
                });
            let Some(substitution) = substitution else {
                state.mark_receiver_uncertain();
                continue;
            };
            let substitutions = state
                .with_member_substitutions
                .entry((current_uri.clone(), offset, candidate.clone()))
                .or_default();
            if !substitutions.contains(&substitution) {
                substitutions.push(substitution);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn remember_with_member_substitutions_with_budget(
        &self,
        current_uri: &Url,
        offset: usize,
        instance: &TypeInstance,
        candidates: &[Candidate],
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<(), String> {
        budget.require_work(candidates.len(), cancel)?;
        for candidate in candidates {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = self.symbol(candidate) else {
                continue;
            };
            let owner_key = symbol.owner_type.as_deref().unwrap_or(&instance.key);
            let substitution = self
                .member_owner_substitution_with_budget(
                    &instance.uri,
                    &instance.key,
                    &instance.substitution,
                    &candidate.uri,
                    owner_key,
                    state,
                    cancel,
                    budget,
                )?
                .or_else(|| {
                    self.candidate_is_helper_member(candidate)
                        .then(|| instance.substitution.clone())
                });
            let Some(substitution) = substitution else {
                state.mark_receiver_uncertain();
                continue;
            };
            let substitutions = state
                .with_member_substitutions
                .entry((current_uri.clone(), offset, candidate.clone()))
                .or_default();
            if !substitutions.contains(&substitution) {
                substitutions.push(substitution);
            }
        }
        Ok(())
    }

    fn unqualified_references(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        self.unqualified_references_with_state(uri, document, offset, name, &mut state)
    }

    fn unqualified_references_with_state(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Vec::new();
        };
        if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            return generic;
        }
        self.unqualified_references_at_scope_with_state(
            uri,
            document,
            offset,
            name,
            identifier,
            document.scope_at(offset),
            state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn unqualified_references_at_scope_with_state(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        scope: usize,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let key = canonical_name(name);
        if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            return generic
                .into_iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.key == key)
                })
                .collect();
        }

        match self.with_lookup_for_name(uri, document, offset, name, state) {
            WithLookup::Found(candidates) => return candidates,
            WithLookup::Unknown => return Vec::new(),
            WithLookup::NotFound => {}
        }

        // Search lexical scopes from the innermost outward. A local symbol
        // shadows both the unit's declarations and imported declarations.
        for scope_id in document.scope_chain_from(scope) {
            let local = document
                .symbol_indices_by_scope_key
                .get(&(scope_id, key.clone()))
                .into_iter()
                .flatten()
                .copied()
                .filter(|index| {
                    document.symbols.get(*index).is_some_and(|symbol| {
                        scope_id != ROOT_SCOPE
                            && !symbol.local_only
                            && symbol.owner_type.is_none()
                            && symbol.generic_parameter.is_none()
                            && symbol.kind != SymbolKind::Unit
                            && !symbol.unresolved_abbreviated
                            && symbol_is_available_at(document, symbol, uri, uri, offset)
                    })
                })
                .map(|index| Candidate {
                    uri: uri.clone(),
                    index,
                })
                .collect::<Vec<_>>();
            if !local.is_empty() {
                return local;
            }
        }

        // Class members are the next lexical scope in Delphi. They must be
        // considered before unit globals and imported declarations, and the
        // owner lookup also covers nested procedures inside a class method.
        if let Some(owner_type) = document.owner_type_at(offset) {
            let helper_target = (!document.offset_is_in_helper_declaration(offset))
                .then(|| self.helper_target_for_owner(uri, document, &owner_type))
                .flatten();
            let (member_uri, member_type, member_scope, member_substitution, helper_owner) =
                helper_target.map_or_else(
                    || {
                        (
                            uri.clone(),
                            owner_type.clone(),
                            ROOT_SCOPE,
                            GenericSubstitution::empty(),
                            None,
                        )
                    },
                    |target| {
                        (
                            target.uri,
                            target.key,
                            target.scope,
                            target.substitution,
                            target.helper_owner,
                        )
                    },
                );
            let members = if let Some(helper_owner) = helper_owner.as_ref() {
                self.member_candidates_for_helper_owner_in_context(
                    uri,
                    &member_uri,
                    &member_type,
                    member_scope,
                    &member_substitution,
                    helper_owner,
                    Some(&key),
                    member_uri == *uri,
                )
            } else {
                self.member_references_for_type_in_context(
                    uri,
                    document,
                    offset,
                    &member_uri,
                    &member_type,
                    member_scope,
                    &member_substitution,
                    &key,
                    member_uri == *uri,
                )
            };
            if !members.candidates.is_empty() {
                return members.candidates;
            }
        }

        let region = document.region_at(offset);
        let current = document
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, key.clone()))
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                document.symbols.get(*index).is_some_and(|symbol| {
                    !symbol.local_only
                        && symbol.owner_type.is_none()
                        && symbol.generic_parameter.is_none()
                        && symbol.kind != SymbolKind::Unit
                        && !symbol.unresolved_abbreviated
                        && symbol_is_available_at(document, symbol, uri, uri, offset)
                        && symbol_visible_in_region(symbol, region)
                })
            })
            .map(|index| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect::<Vec<_>>();
        if !current.is_empty() {
            if self.is_unknown_global_fallback(document, identifier, offset, &current) {
                return Vec::new();
            }
            return current;
        }

        // Only the uses clauses active at this source position contribute
        // imported names. Imported implementation-only routines are excluded
        // by exported_references_for_document.
        let active_uses = document.active_uses(region);
        if active_uses
            .iter()
            .any(|unit| document.unknown_imports.contains(unit.as_str()))
        {
            return Vec::new();
        }
        let mut imported = Vec::new();
        for unit in active_uses {
            for unit_uri in self.unit_urls_for_import(document, unit) {
                imported.extend(
                    self.exported_references_for_document(&unit_uri)
                        .into_iter()
                        .filter(|candidate| {
                            self.symbol(candidate)
                                .is_some_and(|symbol| symbol.key == key)
                        }),
                );
            }
        }
        if !imported.is_empty() {
            if self.is_unknown_global_fallback(document, identifier, offset, &imported) {
                return Vec::new();
            }
            return imported;
        }

        // No class scope is guessed for an ordinary free procedure.
        Vec::new()
    }

    fn member_references(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        dot: Node<'_>,
        member_name: &str,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        self.member_references_with_state(
            current_uri,
            current_document,
            offset,
            dot,
            member_name,
            &mut state,
        )
    }

    fn member_references_with_state(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        dot: Node<'_>,
        member_name: &str,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            state.mark_member_lookup_incomplete();
            return Vec::new();
        };
        let key = canonical_name(member_name);
        let mut references = Vec::new();
        let receivers =
            self.resolve_receivers_with_state(current_uri, current_document, offset, lhs, state);
        if receivers.is_empty() {
            state.mark_member_lookup_incomplete();
            return Vec::new();
        }
        if receivers.len() != 1 {
            state.mark_member_lookup_incomplete();
            state.mark_ambiguous();
        }
        for receiver in receivers {
            match receiver {
                Receiver::Unit(unit_uri) => references.extend(
                    self.exported_references_for_document(&unit_uri)
                        .into_iter()
                        .filter(|candidate| {
                            self.symbol(candidate).is_some_and(|symbol| {
                                symbol.owner_type.is_none() && symbol.key == key
                            })
                        }),
                ),
                Receiver::Type(instance) => {
                    let members = self.member_references_for_instance(
                        current_uri,
                        current_document,
                        offset,
                        &instance,
                        &key,
                        instance.uri == *current_uri,
                    );
                    if !members.ancestry_known || members.ambiguous_names.contains(&key) {
                        state.mark_member_lookup_incomplete();
                    }
                    if members
                        .candidates
                        .iter()
                        .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
                    {
                        state.mark_member_lookup_incomplete();
                    }
                    references.extend(members.candidates)
                }
                Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => {
                    state.mark_member_lookup_incomplete();
                }
            }
        }
        references
    }

    fn resolve_receivers(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
    ) -> Vec<Receiver> {
        let mut state = ResolutionState::new();
        self.resolve_receivers_with_state(current_uri, current_document, offset, node, &mut state)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_receivers_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        depth: usize,
    ) -> Result<Vec<Receiver>, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if depth >= MAX_RECEIVER_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        budget.require_work(1, cancel)?;
        if !state.take_receiver_work() {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        match node.kind() {
            "identifier" => {
                let name = node_text_with_budget(node, &current_document.source, cancel, budget)?;
                let is_qualified_receiver = node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "exprDot" | "genericDot" | "typerefDot")
                        && parent
                            .child_by_field_name("lhs")
                            .is_some_and(|lhs| Span::from_node(lhs) == Span::from_node(node))
                });
                if is_qualified_receiver {
                    let uncertain_before = state.receiver_resolution_uncertain();
                    let receivers = self.resolve_identifier_receiver_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        name,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    )?;
                    if !receivers.is_empty() {
                        if receivers
                            .iter()
                            .all(|receiver| matches!(receiver, Receiver::Unit(_)))
                        {
                            state.receiver_uncertain = uncertain_before;
                        }
                        return Ok(receivers);
                    }
                    if state.receiver_resolution_uncertain() {
                        return Ok(Vec::new());
                    }
                    let references = self
                        .unqualified_references_with_budget_and_state_without_with(
                            current_uri,
                            current_document,
                            offset,
                            name,
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        )?;
                    if !references.is_empty() {
                        return self.resolve_identifier_receiver_with_budget_without_with(
                            current_uri,
                            current_document,
                            offset,
                            name,
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        );
                    }
                    let unit_urls = self.visible_unit_urls_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        name,
                        cancel,
                        budget,
                    )?;
                    if !unit_urls.is_empty() {
                        state.receiver_uncertain = uncertain_before;
                    }
                    return Ok(unit_urls.into_iter().map(Receiver::Unit).collect());
                }
                let receivers = self.resolve_identifier_receiver_with_budget(
                    current_uri,
                    current_document,
                    offset,
                    name,
                    lookup_identifier,
                    state,
                    cancel,
                    budget,
                )?;
                Ok(receivers)
            }
            "exprParens" => first_named_child(node)
                .map(|operand| {
                    self.resolve_receivers_with_state_and_budget(
                        current_uri,
                        current_document,
                        offset,
                        operand,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                        depth.saturating_add(1),
                    )
                })
                .unwrap_or_else(|| Ok(Vec::new())),
            "exprCall" => self.resolve_call_receivers_with_budget(
                current_uri,
                current_document,
                offset,
                node,
                lookup_identifier,
                state,
                depth,
                cancel,
                budget,
            ),
            "exprTpl" | "typerefTpl" => {
                let Some(type_ref) = type_ref_from_node(node, &current_document.source) else {
                    return Ok(Vec::new());
                };
                let receivers = self.type_receivers_for_type_ref_with_budget(
                    current_uri,
                    current_document,
                    type_ref.span.start,
                    &type_ref,
                    lookup_identifier,
                    None,
                    &GenericSubstitution::empty(),
                    state,
                    cancel,
                    budget,
                )?;
                Ok(receivers)
            }
            "exprBinary" | "exprAs" => {
                let Some(operator) = node.child_by_field_name("operator") else {
                    return Ok(Vec::new());
                };
                if node_text_with_budget(operator, &current_document.source, cancel, budget)?
                    .eq_ignore_ascii_case("as")
                {
                    node.child_by_field_name("rhs")
                        .map(|rhs| {
                            self.resolve_cast_receivers_with_state_and_budget(
                                current_uri,
                                current_document,
                                offset,
                                rhs,
                                state,
                                cancel,
                                budget,
                            )
                        })
                        .unwrap_or_else(|| Ok(Vec::new()))
                } else {
                    Ok(Vec::new())
                }
            }
            "exprDot" | "genericDot" | "typerefDot" => {
                if let Some(parts) = qualified_name_parts_with_budget(
                    node,
                    &current_document.source,
                    cancel,
                    budget,
                )? {
                    budget.require_work(parts.len(), cancel)?;
                    return self.resolve_qualified_receiver_path_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        &parts,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    );
                }
                let Some(lhs) = node.child_by_field_name("lhs") else {
                    return Ok(Vec::new());
                };
                let Some(rhs) = node.child_by_field_name("rhs") else {
                    return Ok(Vec::new());
                };
                let Some(rhs_name) = qualified_name_parts_with_budget(
                    rhs,
                    &current_document.source,
                    cancel,
                    budget,
                )?
                .and_then(|parts| parts.last().cloned()) else {
                    return Ok(Vec::new());
                };
                let receivers = self.resolve_receivers_with_state_and_budget(
                    current_uri,
                    current_document,
                    offset,
                    lhs,
                    lookup_identifier,
                    state,
                    cancel,
                    budget,
                    depth.saturating_add(1),
                )?;
                let mut result = Vec::new();
                for receiver in receivers {
                    budget.require_work(1, cancel)?;
                    match receiver {
                        Receiver::Unit(unit_uri) => {
                            result.extend(self.type_receivers_in_unit_with_budget(
                                &unit_uri,
                                &rhs_name,
                                unit_uri == *current_uri,
                                cancel,
                                budget,
                            )?)
                        }
                        Receiver::Type(instance) => {
                            result.extend(self.member_type_receivers_with_budget(
                                current_uri,
                                current_document,
                                offset,
                                &instance.uri,
                                &instance.key,
                                &rhs_name,
                                instance.uri == *current_uri,
                                instance.scope,
                                &instance.substitution,
                                instance.helper_owner.as_ref(),
                                lookup_identifier,
                                state,
                                cancel,
                                budget,
                            )?)
                        }
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => {}
                    }
                }
                Ok(result)
            }
            _ => Ok(Vec::new()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_cast_receivers_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_node: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let Some(parts) =
            qualified_name_parts_with_budget(type_node, &current_document.source, cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        let receivers = self.type_receivers_for_parts_with_budget(
            current_uri,
            current_document,
            offset,
            &parts,
            type_node,
            None,
            state,
            cancel,
            budget,
        )?;
        if receivers.is_empty() {
            if let Some(builtin) = overload::builtin_type(&parts.join(".")) {
                return Ok(vec![Receiver::Builtin(builtin)]);
            }
        }
        Ok(receivers)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_call_receivers_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        call: Node<'_>,
        _lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        depth: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let Some(entity) = call.child_by_field_name("entity") else {
            return Ok(Vec::new());
        };
        let owner_receivers = callable_owner_node(entity)
            .map(|lhs| {
                self.resolve_receivers_with_state_and_budget(
                    current_uri,
                    current_document,
                    offset,
                    lhs,
                    lhs,
                    state,
                    cancel,
                    budget,
                    depth.saturating_add(1),
                )
            })
            .transpose()?
            .unwrap_or_default();
        let owner_instances = owner_receivers
            .iter()
            .filter_map(|receiver| match receiver {
                Receiver::Type(instance) => Some(instance.clone()),
                Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
            })
            .collect::<Vec<_>>();
        let owner_substitution =
            substitution_from_receivers(owner_receivers).unwrap_or_else(GenericSubstitution::empty);
        let callable_identifier = callable_lookup_identifier(entity);
        let candidates = self.resolve_candidates_at_with_state_and_budget(
            current_uri,
            current_document,
            entity.start_byte(),
            callable_identifier,
            state,
            depth.saturating_add(1),
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }

        let mut type_candidates = Vec::new();
        let mut routine_candidates = Vec::new();
        for candidate in candidates {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            match symbol.kind {
                SymbolKind::Type => type_candidates.push(candidate),
                SymbolKind::Routine if !symbol.unresolved_abbreviated => {
                    routine_candidates.push(candidate)
                }
                _ => {}
            }
        }

        if !type_candidates.is_empty() {
            if !routine_candidates.is_empty() {
                state.mark_receiver_uncertain();
                return Ok(Vec::new());
            }
            return Ok(type_candidates
                .into_iter()
                .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
                .collect());
        }
        if routine_candidates.is_empty() {
            return Ok(Vec::new());
        }

        let selection = overload::select(
            self,
            current_uri,
            current_document,
            call,
            &routine_candidates,
            &owner_substitution,
            &owner_instances,
            state,
            depth,
            cancel,
            budget,
        )?;
        let Some(selected_group) = selection.selected_group else {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        };
        let result_substitution = selection
            .generic_substitution
            .unwrap_or_else(|| owner_substitution.clone());
        let routine_candidates = routine_candidates
            .into_iter()
            .filter(|candidate| overload::candidate_in_group(self, candidate, &selected_group))
            .collect::<Vec<_>>();

        let mut result_type_source: Option<(Url, ResultTypeAnnotation)> = None;
        let mut constructor = false;
        for candidate in routine_candidates {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            constructor |= symbol.routine_kind == RoutineKind::Constructor;
            if let Some(annotation) = symbol.result_type_annotation() {
                if result_type_source
                    .as_ref()
                    .is_some_and(|(_, current)| current.name != annotation.name)
                {
                    state.mark_receiver_uncertain();
                    return Ok(Vec::new());
                }
                result_type_source = Some((candidate.uri.clone(), annotation));
            }
        }

        let Some((result_uri, annotation)) = result_type_source else {
            if !constructor {
                return Ok(Vec::new());
            }
            let Some(lhs) = entity.child_by_field_name("lhs") else {
                return Ok(Vec::new());
            };
            let receivers = self.resolve_receivers_with_state_and_budget(
                current_uri,
                current_document,
                offset,
                lhs,
                callable_identifier,
                state,
                cancel,
                budget,
                depth.saturating_add(1),
            )?;
            return Ok(receivers);
        };
        let Some(result_document) = self.documents.get(&result_uri) else {
            return Ok(Vec::new());
        };
        self.result_type_receivers_with_budget(
            &result_uri,
            result_document,
            &annotation,
            &result_substitution,
            state,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_qualified_receiver_path_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if parts.len() < 2 || parts.len() > MAX_RECEIVER_WORK {
            return Ok(Vec::new());
        }
        budget.require_work(parts.len(), cancel)?;
        let Some(first) = parts.first() else {
            return Ok(Vec::new());
        };
        let unit_prefix = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )?;
        let first_is_bound = !self
            .unqualified_references_with_budget_and_state_without_with(
                current_uri,
                current_document,
                offset,
                first,
                lookup_identifier,
                state,
                cancel,
                budget,
            )?
            .is_empty();
        if first.eq_ignore_ascii_case("Self") || first_is_bound {
            let mut receivers = if first_is_bound {
                self.resolve_identifier_receiver_with_budget_without_with(
                    current_uri,
                    current_document,
                    offset,
                    first,
                    lookup_identifier,
                    state,
                    cancel,
                    budget,
                )?
            } else {
                self.resolve_identifier_receiver_with_budget(
                    current_uri,
                    current_document,
                    offset,
                    first,
                    lookup_identifier,
                    state,
                    cancel,
                    budget,
                )?
            };
            for member_name in &parts[1..] {
                budget.require_work(1, cancel)?;
                receivers = receivers
                    .into_iter()
                    .map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers_with_budget(
                            current_uri,
                            current_document,
                            offset,
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            instance.helper_owner.as_ref(),
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        ),
                        Receiver::Unit(unit_uri) => self.type_receivers_in_unit_with_budget(
                            &unit_uri,
                            member_name,
                            unit_uri == *current_uri,
                            cancel,
                            budget,
                        ),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
            }
            return Ok(receivers);
        }

        if let Some(unit_uris) = self.visible_unit_urls_for_path_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )? {
            return Ok(unit_uris.into_iter().map(Receiver::Unit).collect());
        }

        if let Some((prefix_len, unit_uris)) = unit_prefix {
            let Some(type_name) = parts.get(prefix_len) else {
                return Ok(Vec::new());
            };
            let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
            let mut receivers = Vec::new();
            for unit_uri in unit_uris {
                receivers.extend(self.type_receivers_in_unit_with_budget(
                    &unit_uri,
                    type_name,
                    allow_implementation,
                    cancel,
                    budget,
                )?);
            }
            for member_name in &parts[prefix_len + 1..] {
                budget.require_work(1, cancel)?;
                receivers = receivers
                    .into_iter()
                    .map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers_with_budget(
                            current_uri,
                            current_document,
                            offset,
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            instance.helper_owner.as_ref(),
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        ),
                        Receiver::Unit(_) => Ok(Vec::new()),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
            }
            return Ok(receivers);
        }

        let mut receivers = self.resolve_identifier_receiver_with_budget(
            current_uri,
            current_document,
            offset,
            first,
            lookup_identifier,
            state,
            cancel,
            budget,
        )?;
        for member_name in &parts[1..] {
            budget.require_work(1, cancel)?;
            receivers = receivers
                .into_iter()
                .map(|receiver| match receiver {
                    Receiver::Type(instance) => self.member_type_receivers_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        &instance.uri,
                        &instance.key,
                        member_name,
                        instance.uri == *current_uri,
                        instance.scope,
                        &instance.substitution,
                        instance.helper_owner.as_ref(),
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    ),
                    Receiver::Unit(unit_uri) => self.type_receivers_in_unit_with_budget(
                        &unit_uri,
                        member_name,
                        unit_uri == *current_uri,
                        cancel,
                        budget,
                    ),
                    Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
        }
        Ok(receivers)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_identifier_receiver_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if name.eq_ignore_ascii_case("Self") {
            let scope = self.budgeted_scope_at(current_document, offset, cancel, budget)?;
            let Some(owner_type) =
                current_document.owner_type_at_identifier(lookup_identifier, scope)
            else {
                return Ok(Vec::new());
            };
            if let Some(helped_type) = self.helper_target_for_owner_with_budget(
                current_uri,
                current_document,
                &owner_type,
                cancel,
                budget,
            )? {
                return Ok(vec![Receiver::Type(helped_type)]);
            }
            return Ok(vec![Receiver::Type(self.type_instance_for_key(
                current_uri,
                &owner_type,
                ROOT_SCOPE,
                &GenericSubstitution::empty(),
            ))]);
        }
        if name.eq_ignore_ascii_case("Result") {
            let scope = self.budgeted_scope_at(current_document, offset, cancel, budget)?;
            if let Some(annotation) = current_document.result_type_annotation_for_body_scope(scope)
            {
                return self.result_type_receivers_with_budget(
                    current_uri,
                    current_document,
                    &annotation,
                    &GenericSubstitution::empty(),
                    state,
                    cancel,
                    budget,
                );
            }
        }
        let receiver_lookup_scope = state.begin_receiver_lookup_scope();
        let references = self.unqualified_references_with_budget_and_state(
            current_uri,
            current_document,
            offset,
            name,
            lookup_identifier,
            state,
            cancel,
            budget,
        )?;
        let had_bound_reference = !references.is_empty();
        let references = self.filter_accessible_candidates_with_state_and_budget(
            current_uri,
            current_document,
            offset,
            references,
            state,
            cancel,
            budget,
        )?;
        if had_bound_reference && references.is_empty() {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        if !references.is_empty() {
            if references
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            let mut result = Vec::new();
            for reference in references {
                budget.require_work(1, cancel)?;
                let Some(symbol) = self.symbol(&reference) else {
                    continue;
                };
                let substitutions = state
                    .with_member_substitutions
                    .get(&(current_uri.clone(), offset, reference.clone()))
                    .cloned()
                    .unwrap_or_else(|| vec![GenericSubstitution::empty()]);
                if substitutions.len() > 1 {
                    state.mark_receiver_uncertain();
                    return Ok(Vec::new());
                }
                match symbol.kind {
                    SymbolKind::Type => {
                        if let Some(receiver) = self.type_receiver_for_candidate(&reference) {
                            result.push(receiver);
                        }
                    }
                    SymbolKind::Variable
                    | SymbolKind::Parameter
                    | SymbolKind::Field
                    | SymbolKind::Property => {
                        if let Some(declaration_document) = self.documents.get(&reference.uri) {
                            let substitution = substitutions
                                .first()
                                .expect("non-empty receiver substitution list");
                            result.extend(self.type_receivers_for_symbol_type_with_budget(
                                &reference.uri,
                                declaration_document,
                                symbol,
                                lookup_identifier,
                                None,
                                substitution,
                                state,
                                cancel,
                                budget,
                            )?);
                        }
                    }
                    _ => {}
                }
            }
            if result.is_empty() {
                state.mark_receiver_uncertain();
            }
            return Ok(result);
        }
        let urls = self.visible_unit_urls_with_budget(
            current_uri,
            current_document,
            offset,
            name,
            cancel,
            budget,
        )?;
        if matches!(
            self.proven_unique_unit_receiver_with_budget(
                current_uri,
                current_document,
                offset,
                &urls,
                state,
                cancel,
                budget,
            )?,
            Some(UnitExportDomain::Closed),
        ) {
            state.complete_receiver_lookup_as_closed_unit(receiver_lookup_scope);
        }
        Ok(urls.into_iter().map(Receiver::Unit).collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_identifier_receiver_with_budget_without_with(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let previous = state.suppress_with_lookup;
        state.suppress_with_lookup = true;
        let result = self.resolve_identifier_receiver_with_budget(
            current_uri,
            current_document,
            offset,
            name,
            lookup_identifier,
            state,
            cancel,
            budget,
        );
        state.suppress_with_lookup = previous;
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn result_type_receivers_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        annotation: &ResultTypeAnnotation,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let Some(lookup_identifier) = assistance::identifier_at_with_budget(
            current_document.tree.root_node(),
            annotation.offset,
            cancel,
            budget,
            "receiver result type",
        )?
        else {
            return Ok(overload::builtin_type(&annotation.name)
                .map(|builtin| vec![Receiver::Builtin(builtin)])
                .unwrap_or_default());
        };
        self.type_receivers_for_type_ref_with_budget(
            current_uri,
            current_document,
            annotation.type_ref.span.start,
            &annotation.type_ref,
            lookup_identifier,
            Some(annotation.scope),
            substitution,
            state,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_path_with_budget_at_scope(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &str,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        budget.require_work(
            path.split('.').filter(|part| !part.is_empty()).count(),
            cancel,
        )?;
        budget.require_bytes(path.len(), cancel)?;
        let parts = path
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        budget.require_work(parts.len(), cancel)?;
        let receivers = self.type_receivers_for_parts_with_budget(
            current_uri,
            current_document,
            offset,
            &parts,
            lookup_identifier,
            scope_override,
            state,
            cancel,
            budget,
        )?;
        if receivers.is_empty() {
            if let Some(builtin) = overload::builtin_type(path) {
                return Ok(vec![Receiver::Builtin(builtin)]);
            }
        }
        Ok(receivers)
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_type_ref_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_ref: &TypeRef,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        self.type_receivers_for_type_ref_with_budget_at_depth(
            current_uri,
            current_document,
            offset,
            type_ref,
            lookup_identifier,
            scope_override,
            substitution,
            state,
            cancel,
            budget,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_type_ref_with_budget_at_depth(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_ref: &TypeRef,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        depth: usize,
    ) -> Result<Vec<Receiver>, String> {
        if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        budget.require_work(
            type_ref.path.len().saturating_add(type_ref.args.len()),
            cancel,
        )?;
        budget.require_bytes(type_ref.display().len(), cancel)?;
        if type_ref.path.len() == 1 {
            if let Some(resolved) = substitution.get(&type_ref.path[0]) {
                return Ok(resolved.clone().into_receiver().into_iter().collect());
            }
        }
        let bases = self.type_receivers_for_parts_with_budget(
            current_uri,
            current_document,
            offset,
            &type_ref.path,
            lookup_identifier,
            scope_override,
            state,
            cancel,
            budget,
        )?;
        let mut result = Vec::new();
        for base in bases {
            let Receiver::Type(mut instance) = base else {
                if type_ref.args.is_empty() {
                    result.push(base);
                }
                continue;
            };
            if instance.parameter_names.is_empty() {
                if !type_ref.args.is_empty() {
                    continue;
                }
                result.push(Receiver::Type(instance));
                continue;
            }
            if type_ref.args.len() != instance.parameter_names.len() {
                continue;
            }
            let mut actuals = Vec::with_capacity(type_ref.args.len());
            let mut valid = true;
            for argument in &type_ref.args {
                let receivers = self.type_receivers_for_type_ref_with_budget_at_depth(
                    current_uri,
                    current_document,
                    argument.span.start,
                    argument,
                    lookup_identifier,
                    scope_override,
                    substitution,
                    state,
                    cancel,
                    budget,
                    depth.saturating_add(1),
                )?;
                let Some(resolved) = resolved_type_from_receivers(receivers) else {
                    valid = false;
                    break;
                };
                actuals.push(resolved);
            }
            if !valid {
                continue;
            }
            for (name, actual) in instance.parameter_names.iter().zip(actuals) {
                instance.substitution.insert(name, actual);
            }
            if !self
                .generic_type_constraints_satisfied_with_budget(&instance, state, cancel, budget)?
            {
                continue;
            }
            result.push(Receiver::Type(instance));
        }
        if result.is_empty() && type_ref.args.is_empty() {
            if let Some(builtin) = overload::builtin_type(&type_ref.display()) {
                result.push(Receiver::Builtin(builtin));
            }
        }
        Ok(result)
    }

    fn generic_type_constraints_satisfied_with_budget(
        &self,
        instance: &TypeInstance,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        let Some(candidate) = self.type_symbol_candidate(instance) else {
            return Ok(false);
        };
        let Some(symbol) = self.symbol(&candidate) else {
            return Ok(false);
        };
        if symbol.generic_parameters.is_empty() {
            return Ok(true);
        }
        // A constraint can expand its own parameter into a larger generic
        // argument on every recursive pass (for example
        // `TNode<T: TNode<TWrap<T>>>`).  Including the substitution in the
        // active key therefore makes every pass look new and lets the call
        // stack grow until the process overflows.  Re-entering the same
        // source generic declaration while proving one specialization is
        // already enough to classify the relationship as unknown.
        let resolution_key = (instance.uri.clone(), instance.key.clone());
        if !state
            .active_generic_constraints
            .insert(resolution_key.clone())
        {
            state.mark_receiver_uncertain();
            return Ok(false);
        }
        let mut ancestry = AncestryResolutionState::new();
        let result = overload::generic_constraints_satisfied(
            self,
            &candidate,
            symbol,
            &instance.substitution,
            state,
            &mut ancestry,
            cancel,
            budget,
        );
        state.active_generic_constraints.remove(&resolution_key);
        match result? {
            overload::ConstraintOutcome::Proven => Ok(true),
            overload::ConstraintOutcome::Unknown => {
                state.mark_receiver_uncertain();
                Ok(false)
            }
            overload::ConstraintOutcome::Contradictory => Ok(false),
        }
    }

    fn generic_type_constraints_satisfied(
        &self,
        instance: &TypeInstance,
        state: &mut ResolutionState,
    ) -> bool {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "generic type constraint",
        );
        self.generic_type_constraints_satisfied_with_budget(instance, state, &cancel, &mut budget)
            .unwrap_or(false)
    }

    fn type_symbol_candidate(&self, instance: &TypeInstance) -> Option<Candidate> {
        let document = self.documents.get(&instance.uri)?;
        let indices = document.type_symbol_indices.get(&instance.key)?;
        if indices.len() != 1 {
            return None;
        }
        Some(Candidate {
            uri: instance.uri.clone(),
            index: *indices.first()?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_type_ref(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_ref: &TypeRef,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if type_ref.path.len() == 1 {
            if let Some(resolved) = substitution.get(&type_ref.path[0]) {
                return resolved.clone().into_receiver().into_iter().collect();
            }
        }
        let bases = self.type_receivers_for_parts(
            current_uri,
            current_document,
            offset,
            &type_ref.path,
            scope_override,
            state,
        );
        let mut result = Vec::new();
        for base in bases {
            let Receiver::Type(mut instance) = base else {
                if type_ref.args.is_empty() {
                    result.push(base);
                }
                continue;
            };
            if instance.parameter_names.is_empty() {
                if type_ref.args.is_empty() {
                    result.push(Receiver::Type(instance));
                }
                continue;
            }
            if type_ref.args.len() != instance.parameter_names.len() {
                continue;
            }
            let mut actuals = Vec::with_capacity(type_ref.args.len());
            let mut valid = true;
            for argument in &type_ref.args {
                let receivers = self.type_receivers_for_type_ref(
                    current_uri,
                    current_document,
                    argument.span.start,
                    argument,
                    scope_override,
                    substitution,
                    state,
                );
                let Some(resolved) = resolved_type_from_receivers(receivers) else {
                    valid = false;
                    break;
                };
                actuals.push(resolved);
            }
            if !valid {
                continue;
            }
            for (name, actual) in instance.parameter_names.iter().zip(actuals) {
                instance.substitution.insert(name, actual);
            }
            if !self.generic_type_constraints_satisfied(&instance, state) {
                continue;
            }
            result.push(Receiver::Type(instance));
        }
        if result.is_empty() && type_ref.args.is_empty() {
            if let Some(builtin) = overload::builtin_type(&type_ref.display()) {
                result.push(Receiver::Builtin(builtin));
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_symbol_type_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        symbol: &Symbol,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if let Some(type_ref) = symbol.type_ref.as_ref() {
            return self.type_receivers_for_type_ref_with_budget(
                current_uri,
                current_document,
                type_ref.span.start,
                type_ref,
                lookup_identifier,
                scope_override,
                substitution,
                state,
                cancel,
                budget,
            );
        }
        let Some(type_name) = symbol.type_name.as_deref() else {
            return Ok(Vec::new());
        };
        self.type_receivers_for_path_with_budget_at_scope(
            current_uri,
            current_document,
            symbol.span.start,
            type_name,
            lookup_identifier,
            scope_override,
            state,
            cancel,
            budget,
        )
    }

    fn type_receivers_for_symbol_type(
        &self,
        current_uri: &Url,
        current_document: &Document,
        symbol: &Symbol,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if let Some(type_ref) = symbol.type_ref.as_ref() {
            return self.type_receivers_for_type_ref(
                current_uri,
                current_document,
                type_ref.span.start,
                type_ref,
                scope_override,
                substitution,
                state,
            );
        }
        let Some(type_name) = symbol.type_name.as_deref() else {
            return Vec::new();
        };
        self.type_receivers_for_path_at_scope(
            current_uri,
            current_document,
            symbol.span.start,
            type_name,
            scope_override,
            state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_parts_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        budget.require_work(parts.len(), cancel)?;
        if parts.len() == 1 {
            let candidates = if let Some(scope) = scope_override {
                self.unqualified_references_with_budget_at_scope_and_state(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    lookup_identifier,
                    scope,
                    state,
                    cancel,
                    budget,
                )?
            } else {
                self.unqualified_references_with_budget_and_state(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    lookup_identifier,
                    state,
                    cancel,
                    budget,
                )?
            };
            if candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            let mut result = Vec::new();
            for candidate in candidates {
                budget.require_work(1, cancel)?;
                let Some(symbol) = self.symbol(&candidate) else {
                    continue;
                };
                if symbol.kind == SymbolKind::Type {
                    if let Some(receiver) = self.type_receiver_for_candidate(&candidate) {
                        result.push(receiver);
                    }
                }
            }
            return Ok(result);
        }

        let root_candidates = if let Some(scope) = scope_override {
            self.unqualified_references_with_budget_at_scope_and_state(
                current_uri,
                current_document,
                offset,
                &parts[0],
                lookup_identifier,
                scope,
                state,
                cancel,
                budget,
            )?
        } else {
            self.unqualified_references_with_budget_and_state(
                current_uri,
                current_document,
                offset,
                &parts[0],
                lookup_identifier,
                state,
                cancel,
                budget,
            )?
        };
        if !root_candidates.is_empty() {
            if root_candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            let mut receivers = Vec::new();
            for candidate in root_candidates {
                budget.require_work(1, cancel)?;
                let Some(symbol) = self.symbol(&candidate) else {
                    continue;
                };
                if symbol.kind != SymbolKind::Type {
                    return Ok(Vec::new());
                }
                let Some(receiver) = self.type_receiver_for_candidate(&candidate) else {
                    return Ok(Vec::new());
                };
                receivers.push(receiver);
            }
            for member_name in &parts[1..] {
                budget.require_work(1, cancel)?;
                receivers = receivers
                    .into_iter()
                    .map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers_with_budget(
                            current_uri,
                            current_document,
                            offset,
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            instance.helper_owner.as_ref(),
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        ),
                        Receiver::Unit(_) => Ok(Vec::new()),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
            }
            return Ok(receivers);
        }

        let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        let Some(type_name) = parts.get(prefix_len) else {
            return Ok(Vec::new());
        };
        let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        let mut receivers = Vec::new();
        for unit_uri in unit_uris {
            receivers.extend(self.type_receivers_in_unit_with_budget(
                &unit_uri,
                type_name,
                allow_implementation,
                cancel,
                budget,
            )?);
        }
        for member_name in &parts[prefix_len + 1..] {
            budget.require_work(1, cancel)?;
            receivers = receivers
                .into_iter()
                .map(|receiver| match receiver {
                    Receiver::Type(instance) => self.member_type_receivers_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        &instance.uri,
                        &instance.key,
                        member_name,
                        instance.uri == *current_uri,
                        instance.scope,
                        &instance.substitution,
                        instance.helper_owner.as_ref(),
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    ),
                    Receiver::Unit(_) => Ok(Vec::new()),
                    Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
        }
        Ok(receivers)
    }

    fn type_receivers_in_unit_with_budget(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let candidates = self.type_candidates_in_unit_with_budget(
            unit_uri,
            name,
            allow_implementation,
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for candidate in candidates {
            budget.require_work(1, cancel)?;
            let Some(_symbol) = self.symbol(&candidate) else {
                continue;
            };
            if let Some(receiver) = self.type_receiver_for_candidate(&candidate) {
                result.push(receiver);
            }
        }
        Ok(result)
    }

    fn budgeted_scope_at(
        &self,
        document: &Document,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<usize, String> {
        match document
            .scope_intervals
            .at_with_budget(offset, cancel, budget)?
        {
            Some(scope) => Ok(scope),
            None => Ok(ROOT_SCOPE),
        }
    }

    fn scope_chain_with_budget(
        &self,
        document: &Document,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<usize>, String> {
        let scope = self.budgeted_scope_at(document, offset, cancel, budget)?;
        self.scope_chain_from_with_budget(document, scope, cancel, budget)
    }

    fn scope_chain_from_with_budget(
        &self,
        document: &Document,
        mut current: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<usize>, String> {
        let mut result = Vec::new();
        loop {
            budget.require_work(1, cancel)?;
            result.push(current);
            let Some(parent) = document.scopes[current].parent else {
                break;
            };
            current = parent;
        }
        Ok(result)
    }

    fn member_owner_substitution(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
    ) -> Option<GenericSubstitution> {
        let mut active = HashSet::new();
        self.member_owner_substitution_inner(
            type_uri,
            type_key,
            substitution,
            owner_uri,
            owner_key,
            state,
            &mut active,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn routine_owner_substitution_with_budget(
        &self,
        current_uri: &Url,
        offset: usize,
        candidate: &Candidate,
        fallback: &GenericSubstitution,
        owner_instances: &[TypeInstance],
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        let Some(symbol) = self.symbol(candidate) else {
            return Ok(None);
        };
        if owner_instances.is_empty() {
            if let Some(substitutions) = state.with_member_substitutions.get(&(
                current_uri.clone(),
                offset,
                candidate.clone(),
            )) {
                if substitutions.len() != 1 {
                    return Ok(None);
                }
                let mut result = substitutions[0].clone();
                for parameter in &symbol.generic_parameters {
                    result.remove(&parameter.name);
                }
                return Ok(Some(result));
            }
        }
        let Some(owner_key) = symbol.owner_type.as_deref() else {
            let mut result = fallback.clone();
            for parameter in &symbol.generic_parameters {
                result.remove(&parameter.name);
            }
            return Ok(Some(result));
        };
        let mut result = None;
        for instance in owner_instances {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(substitution) = self.member_owner_substitution_with_budget(
                &instance.uri,
                &instance.key,
                &instance.substitution,
                &candidate.uri,
                owner_key,
                state,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            if result
                .as_ref()
                .is_some_and(|current: &GenericSubstitution| current != &substitution)
            {
                return Ok(None);
            }
            result = Some(substitution);
        }
        let mut result = result.unwrap_or_else(|| fallback.clone());
        for parameter in &symbol.generic_parameters {
            result.remove(&parameter.name);
        }
        Ok(Some(result))
    }

    #[allow(clippy::too_many_arguments)]
    fn member_owner_substitution_inner(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
        active: &mut HashSet<(Url, String)>,
    ) -> Option<GenericSubstitution> {
        if type_uri == owner_uri && type_key == owner_key {
            return Some(substitution.clone());
        }
        if !active.insert((type_uri.clone(), type_key.to_owned())) {
            return None;
        }
        let result = (|| {
            let document = self.documents.get(type_uri)?;
            let entries = document.type_ancestry.get(type_key)?;
            if entries.len() != 1 {
                return None;
            }
            let entry = &entries[0];
            for parent in entry.parents.iter().filter(|parent| {
                matches!(
                    (entry.kind, parent.relation),
                    (TypeKind::Class, ParentRelation::Superclass)
                        | (TypeKind::Interface, ParentRelation::InterfaceParent)
                )
            }) {
                let type_ref = parent.type_ref.as_ref()?;
                let receivers = self.type_receivers_for_type_ref(
                    type_uri,
                    document,
                    type_ref.span.start,
                    type_ref,
                    None,
                    substitution,
                    state,
                );
                let parent_instance = unique_type_instance(receivers)?;
                if let Some(result) = self.member_owner_substitution_inner(
                    &parent_instance.uri,
                    &parent_instance.key,
                    &parent_instance.substitution,
                    owner_uri,
                    owner_key,
                    state,
                    active,
                ) {
                    return Some(result);
                }
            }
            None
        })();
        active.remove(&(type_uri.clone(), type_key.to_owned()));
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_owner_substitution_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        let mut active = HashSet::new();
        self.member_owner_substitution_with_budget_inner(
            type_uri,
            type_key,
            substitution,
            owner_uri,
            owner_key,
            state,
            &mut active,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn member_owner_substitution_with_budget_inner(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
        active: &mut HashSet<(Url, String)>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        if type_uri == owner_uri && type_key == owner_key {
            return Ok(Some(substitution.clone()));
        }
        if !active.insert((type_uri.clone(), type_key.to_owned())) {
            return Ok(None);
        }
        let result = (|| {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(document) = self.documents.get(type_uri) else {
                return Ok(None);
            };
            let Some(entries) = document.type_ancestry.get(type_key) else {
                return Ok(None);
            };
            if entries.len() != 1 {
                return Ok(None);
            }
            let entry = &entries[0];
            for parent in entry.parents.iter().filter(|parent| {
                matches!(
                    (entry.kind, parent.relation),
                    (TypeKind::Class, ParentRelation::Superclass)
                        | (TypeKind::Interface, ParentRelation::InterfaceParent)
                )
            }) {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                let Some(type_ref) = parent.type_ref.as_ref() else {
                    return Ok(None);
                };
                let lookup_identifier = assistance::identifier_at_with_budget(
                    document.tree.root_node(),
                    type_ref.span.start,
                    cancel,
                    budget,
                    "generic parent substitution",
                )?
                .unwrap_or_else(|| document.tree.root_node());
                let receivers = self.type_receivers_for_type_ref_with_budget(
                    type_uri,
                    document,
                    type_ref.span.start,
                    type_ref,
                    lookup_identifier,
                    None,
                    substitution,
                    state,
                    cancel,
                    budget,
                )?;
                let Some(parent_instance) = unique_type_instance(receivers) else {
                    return Ok(None);
                };
                if let Some(result) = self.member_owner_substitution_with_budget_inner(
                    &parent_instance.uri,
                    &parent_instance.key,
                    &parent_instance.substitution,
                    owner_uri,
                    owner_key,
                    state,
                    active,
                    cancel,
                    budget,
                )? {
                    return Ok(Some(result));
                }
            }
            Ok(None)
        })();
        active.remove(&(type_uri.clone(), type_key.to_owned()));
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_type_receivers_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_uri: &Url,
        type_key: &str,
        name: &str,
        allow_implementation: bool,
        type_scope: usize,
        substitution: &GenericSubstitution,
        helper_owner: Option<&HelperOwner>,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let member_key = canonical_name(name);
        let resolution_key = (
            type_uri.clone(),
            type_key.to_owned(),
            member_key.clone(),
            type_scope,
            substitution.clone(),
        );
        if !state.active_members.insert(resolution_key.clone()) {
            return Ok(Vec::new());
        }
        let lookup = if let Some(helper_owner) = helper_owner {
            self.member_candidates_for_helper_owner_in_context_with_budget(
                current_uri,
                type_uri,
                type_key,
                type_scope,
                substitution,
                helper_owner,
                Some(&member_key),
                allow_implementation,
                cancel,
                budget,
            )?
        } else {
            self.member_references_for_type_in_context_with_budget(
                current_uri,
                current_document,
                offset,
                type_uri,
                type_key,
                type_scope,
                substitution,
                &member_key,
                allow_implementation,
                cancel,
                budget,
            )?
        };
        if !lookup.ancestry_known || (lookup.implicit_member_known && lookup.candidates.is_empty())
        {
            state.mark_receiver_uncertain();
            state.active_members.remove(&resolution_key);
            return Ok(Vec::new());
        }
        let candidates = lookup.candidates;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            state.active_members.remove(&resolution_key);
            return Ok(Vec::new());
        }
        let candidates = self.filter_accessible_candidates_with_state_and_budget(
            current_uri,
            current_document,
            offset,
            candidates,
            state,
            cancel,
            budget,
        )?;
        if state.receiver_resolution_uncertain() {
            state.active_members.remove(&resolution_key);
            return Ok(Vec::new());
        }
        let constructor_keys = candidates
            .iter()
            .filter_map(|candidate| {
                let symbol = self.symbol(candidate)?;
                (symbol.kind == SymbolKind::Routine
                    && symbol.routine_kind == RoutineKind::Constructor)
                    .then(|| {
                        symbol
                            .routine_key
                            .clone()
                            .map(|key| (candidate.uri.clone(), key))
                    })
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let constructor_is_unique = constructor_keys.len() == 1;
        let mut result = Vec::new();
        for candidate in candidates {
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine && symbol.routine_kind == RoutineKind::Constructor
            {
                if constructor_is_unique {
                    result.push(Receiver::Type(TypeInstance {
                        uri: type_uri.clone(),
                        key: type_key.to_owned(),
                        kind: TypeKind::Other,
                        scope: type_scope,
                        parameter_names: Vec::new(),
                        substitution: substitution.clone(),
                        helper_owner: None,
                    }));
                }
                continue;
            }
            let owner_key = symbol.owner_type.as_deref().unwrap_or(type_key);
            let member_substitution = self
                .member_owner_substitution_with_budget(
                    type_uri,
                    type_key,
                    substitution,
                    &candidate.uri,
                    owner_key,
                    state,
                    cancel,
                    budget,
                )?
                .or_else(|| {
                    self.candidate_is_helper_member(&candidate)
                        .then(|| substitution.clone())
                });
            let Some(member_substitution) = member_substitution else {
                continue;
            };
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            result.extend(self.type_receivers_for_symbol_type_with_budget(
                &candidate.uri,
                document,
                symbol,
                lookup_identifier,
                None,
                &member_substitution,
                state,
                cancel,
                budget,
            )?);
        }
        state.active_members.remove(&resolution_key);
        Ok(result)
    }

    fn resolve_receivers_with_state(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "receiver resolution",
        );
        self.resolve_receivers_with_state_and_budget(
            current_uri,
            current_document,
            offset,
            node,
            node,
            state,
            &cancel,
            &mut budget,
            0,
        )
        .unwrap_or_default()
    }

    fn type_receivers_for_path_at_scope(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &str,
        scope_override: Option<usize>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let parts = path
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let receivers = self.type_receivers_for_parts(
            current_uri,
            current_document,
            offset,
            &parts,
            scope_override,
            state,
        );
        if receivers.is_empty() {
            if let Some(builtin) = overload::builtin_type(path) {
                return vec![Receiver::Builtin(builtin)];
            }
        }
        receivers
    }

    fn type_receivers_for_parts(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        scope_override: Option<usize>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if parts.is_empty() {
            return Vec::new();
        }
        if !state.take_type_work(parts.len()) {
            state.mark_receiver_uncertain();
            return Vec::new();
        }

        if parts.len() == 1 {
            let candidates = if let Some(scope) = scope_override {
                let Some(identifier) = identifier_at(current_document.tree.root_node(), offset)
                else {
                    return Vec::new();
                };
                self.unqualified_references_at_scope_with_state(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    identifier,
                    scope,
                    state,
                )
            } else {
                self.unqualified_references_with_state(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    state,
                )
            };
            if candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Vec::new();
            }
            return candidates
                .into_iter()
                .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
                .collect();
        }

        let root_candidates = if let Some(scope) = scope_override {
            let Some(identifier) = identifier_at(current_document.tree.root_node(), offset) else {
                return Vec::new();
            };
            self.unqualified_references_at_scope_with_state(
                current_uri,
                current_document,
                offset,
                &parts[0],
                identifier,
                scope,
                state,
            )
        } else {
            self.unqualified_references_with_state(
                current_uri,
                current_document,
                offset,
                &parts[0],
                state,
            )
        };
        if !root_candidates.is_empty() {
            if root_candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Vec::new();
            }
            if root_candidates.iter().any(|candidate| {
                self.symbol(candidate)
                    .is_none_or(|symbol| symbol.kind != SymbolKind::Type)
            }) {
                return Vec::new();
            }
            let mut receivers = root_candidates
                .into_iter()
                .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
                .collect::<Vec<_>>();
            for member_name in &parts[1..] {
                receivers = receivers
                    .into_iter()
                    .flat_map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers(
                            current_uri,
                            current_document,
                            offset,
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            instance.helper_owner.as_ref(),
                            state,
                        ),
                        Receiver::Unit(_) => Vec::new(),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Vec::new(),
                    })
                    .collect();
            }
            return receivers;
        }

        let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, parts)
        else {
            // A qualified type name must not fall back to an unqualified type
            // with the same final component.
            return Vec::new();
        };
        let Some(type_name) = parts.get(prefix_len) else {
            return Vec::new();
        };
        let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        let mut receivers = unit_uris
            .iter()
            .flat_map(|unit_uri| {
                self.type_receivers_in_unit(unit_uri, type_name, allow_implementation)
            })
            .collect::<Vec<_>>();
        for member_name in &parts[prefix_len + 1..] {
            receivers = receivers
                .into_iter()
                .flat_map(|receiver| match receiver {
                    Receiver::Type(instance) => self.member_type_receivers(
                        current_uri,
                        current_document,
                        offset,
                        &instance.uri,
                        &instance.key,
                        member_name,
                        instance.uri == *current_uri,
                        instance.scope,
                        &instance.substitution,
                        instance.helper_owner.as_ref(),
                        state,
                    ),
                    Receiver::Unit(_) => Vec::new(),
                    Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Vec::new(),
                })
                .collect();
        }
        receivers
    }

    fn longest_visible_unit_prefix(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
    ) -> Option<(usize, Vec<Url>)> {
        if parts.len() > MAX_RECEIVER_WORK {
            return None;
        }

        let region = current_document.region_at(offset);
        let mut best = None;
        let mut consider = |unit_name: &str, unit_uris: Vec<Url>| {
            let unit_parts = unit_name.split('.').collect::<Vec<_>>();
            if unit_parts.len() >= parts.len()
                || !unit_parts.iter().enumerate().all(|(index, unit_part)| {
                    canonical_name(unit_part) == canonical_name(&parts[index])
                })
            {
                return;
            }
            if best
                .as_ref()
                .is_none_or(|(prefix_len, _)| *prefix_len < unit_parts.len())
            {
                best = Some((unit_parts.len(), unit_uris));
            }
        };

        consider(
            current_document.unit_name.as_str(),
            vec![current_uri.clone()],
        );
        for used in current_document.active_uses(region) {
            let unit_uris = self.unit_urls_for_import(current_document, used);
            if !unit_uris.is_empty() {
                consider(used, unit_uris);
            }
        }
        best
    }

    fn visible_unit_urls_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Url>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        if current_document.unit_name == key {
            budget.require_work(1, cancel)?;
            budget.require_bytes(current_uri.as_str().len(), cancel)?;
            return Ok(vec![current_uri.clone()]);
        }

        let region = current_document.region_at(offset);
        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        if !active_uses.iter().any(|used| used.as_str() == key) {
            return Ok(Vec::new());
        }
        self.unit_urls_for_import_with_budget(current_document, &key, cancel, budget)
    }

    #[allow(clippy::too_many_arguments)]
    fn proven_unique_unit_receiver_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        unit_urls: &[Url],
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<UnitExportDomain>, String> {
        let Some(unit_uri) = unit_urls.first().filter(|_| unit_urls.len() == 1) else {
            return Ok(None);
        };
        let Some(unit_document) = self.documents.get(unit_uri) else {
            return Ok(None);
        };
        budget.require_work(
            unit_document
                .parser_recovery_spans
                .len()
                .saturating_add(unit_document.conditionals.unknown_spans.len()),
            cancel,
        )?;
        if !unit_document.parser_recovery_spans.is_empty()
            || !unit_document.conditionals.unknown_spans.is_empty()
        {
            return Ok(None);
        }

        let Some(indices) = unit_document
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, unit_document.unit_name.clone()))
        else {
            return Ok(None);
        };
        budget.require_work(indices.len(), cancel)?;
        let mut unit_candidate = None;
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = unit_document.symbols.get(*index) else {
                continue;
            };
            if symbol.kind != SymbolKind::Unit {
                continue;
            }
            if unit_candidate.is_some() {
                return Ok(None);
            }
            unit_candidate = Some(Candidate {
                uri: unit_uri.clone(),
                index: *index,
            });
        }
        let Some(candidate) = unit_candidate else {
            return Ok(None);
        };
        if self.candidate_is_conditionally_unknown(&candidate) {
            return Ok(None);
        }
        Ok(
            match self.candidate_access_decision_with_budget(
                current_uri,
                current_document,
                offset,
                &candidate,
                state,
                cancel,
                budget,
            )? {
                AccessDecision::Visible => Some(self.unit_export_domain_for_uri(unit_uri)),
                AccessDecision::Unknown => {
                    state.mark_receiver_uncertain();
                    None
                }
                AccessDecision::Inaccessible => {
                    state.mark_inaccessible_candidate();
                    None
                }
            },
        )
    }

    fn unit_export_domain_for_uri(&self, unit_uri: &Url) -> UnitExportDomain {
        let Some(unit_document) = self.documents.get(unit_uri) else {
            return UnitExportDomain::Unknown;
        };
        if unit_document.unit_name.is_empty() {
            return UnitExportDomain::Unknown;
        }
        if unit_document.unit_name == "system" {
            // A selected System document proves the source unit identity and
            // its source exports, but not the compiler/runtime export surface
            // that is absent from that source catalogue.  Other retained
            // System documents do not change the selected document's domain.
            UnitExportDomain::OpenImplicitSystem
        } else {
            UnitExportDomain::Closed
        }
    }

    fn visible_unit_urls_for_path_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Url>>, String> {
        budget.require_work(parts.len().saturating_add(1), cancel)?;
        let path_bytes = parts
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(parts.len().saturating_sub(1));
        budget.require_bytes(path_bytes, cancel)?;
        let key = canonical_path(parts);
        if current_document.unit_name == key {
            budget.require_bytes(current_uri.as_str().len(), cancel)?;
            return Ok(Some(vec![current_uri.clone()]));
        }

        let region = current_document.region_at(offset);
        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        if !active_uses.iter().any(|used| used.as_str() == key) {
            return Ok(None);
        }
        let urls = self.unit_urls_for_import_with_budget(current_document, &key, cancel, budget)?;
        Ok((!urls.is_empty()).then_some(urls))
    }

    fn longest_visible_unit_prefix_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<(usize, Vec<Url>)>, String> {
        if parts.len() > MAX_RECEIVER_WORK {
            return Ok(None);
        }
        budget.require_work(parts.len().saturating_add(1), cancel)?;

        let region = current_document.region_at(offset);
        let mut best = None;
        if let Some(prefix_len) = matching_unit_prefix_len_with_budget(
            &current_document.unit_name,
            parts,
            cancel,
            budget,
        )? {
            budget.require_work(1, cancel)?;
            budget.require_bytes(current_uri.as_str().len(), cancel)?;
            best = Some((prefix_len, vec![current_uri.clone()]));
        }

        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        for used in active_uses {
            check_navigation_cancel(cancel)?;
            let Some(prefix_len) =
                matching_unit_prefix_len_with_budget(used, parts, cancel, budget)?
            else {
                continue;
            };
            if best
                .as_ref()
                .is_some_and(|(best_prefix_len, _)| *best_prefix_len >= prefix_len)
            {
                continue;
            }
            let unit_uris =
                self.unit_urls_for_import_with_budget(current_document, used, cancel, budget)?;
            if !unit_uris.is_empty() {
                best = Some((prefix_len, unit_uris));
            }
        }
        Ok(best)
    }

    fn type_receivers_in_unit(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
    ) -> Vec<Receiver> {
        let candidates = self.type_candidates_in_unit(unit_uri, name, allow_implementation);
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Vec::new();
        }
        candidates
            .into_iter()
            .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
            .collect()
    }

    fn type_receiver_for_candidate(&self, candidate: &Candidate) -> Option<Receiver> {
        let symbol = self.symbol(candidate)?;
        (symbol.kind == SymbolKind::Type && symbol.generic_parameter.is_none())
            .then(|| Receiver::Type(type_instance_from_symbol(candidate, symbol)))
    }

    fn type_candidates_in_unit(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
    ) -> Vec<Candidate> {
        let key = canonical_name(name);
        let Some(document) = self.documents.get(unit_uri) else {
            return Vec::new();
        };
        document
            .type_symbol_indices
            .get(&key)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                document.symbols.get(*index).is_some_and(|symbol| {
                    !symbol.local_only
                        && (symbol.region == Region::Interface
                            || (allow_implementation && symbol.region == Region::Implementation))
                })
            })
            .map(|index| Candidate {
                uri: unit_uri.clone(),
                index,
            })
            .collect()
    }

    fn type_candidates_in_unit_with_budget(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        let Some(document) = self.documents.get(unit_uri) else {
            return Ok(Vec::new());
        };
        let Some(indices) = document.type_symbol_indices.get(&key) else {
            return Ok(Vec::new());
        };
        budget.require_work(indices.len(), cancel)?;
        budget.require_bytes(
            unit_uri.as_str().len().saturating_mul(indices.len()),
            cancel,
        )?;
        #[cfg(test)]
        test_record_materialization(&TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS);
        let mut result = Vec::with_capacity(indices.len());
        for index in indices {
            check_navigation_cancel(cancel)?;
            if document.symbols.get(*index).is_some_and(|symbol| {
                !symbol.local_only
                    && (symbol.region == Region::Interface
                        || (allow_implementation && symbol.region == Region::Implementation))
            }) {
                result.push(Candidate {
                    uri: unit_uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(result)
    }

    fn active_helper_for_type_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        target: &TypeInstance,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<HelperSelection, String> {
        if !matches!(target.kind, TypeKind::Class | TypeKind::Record) {
            return Ok(HelperSelection::None);
        }

        let region = current_document.region_at(offset);
        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        let mut visible = Vec::new();
        let mut unknown_ranks = Vec::new();
        let mut seen = HashSet::new();

        budget.require_work(current_document.helpers.len(), cancel)?;
        for (index, helper) in current_document.helpers.iter().enumerate() {
            if helper_visible_at(helper, true, region, offset) {
                let identity = (current_uri.clone(), index);
                if seen.insert(identity) {
                    visible.push((
                        current_uri.clone(),
                        index,
                        HelperRank {
                            target_specificity: 0,
                            local: true,
                            import_order: 0,
                            declaration_order: helper.name_span.start,
                        },
                    ));
                }
            }
        }

        for (import_order, unit) in active_uses.into_iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if current_document.unknown_imports.contains(unit.as_str()) {
                unknown_ranks.push(HelperRank {
                    target_specificity: usize::MAX,
                    local: false,
                    import_order,
                    declaration_order: usize::MAX,
                });
                continue;
            }
            for unit_uri in
                self.unit_urls_for_import_with_budget(current_document, unit, cancel, budget)?
            {
                let Some(unit_document) = self.documents.get(&unit_uri) else {
                    continue;
                };
                budget.require_work(unit_document.helpers.len(), cancel)?;
                for (index, helper) in unit_document.helpers.iter().enumerate() {
                    if !helper_visible_at(helper, false, region, offset) {
                        continue;
                    }
                    let identity = (unit_uri.clone(), index);
                    if seen.insert(identity) {
                        visible.push((
                            unit_uri.clone(),
                            index,
                            HelperRank {
                                target_specificity: 0,
                                local: false,
                                import_order,
                                declaration_order: helper.name_span.start,
                            },
                        ));
                    }
                }
            }
        }

        let mut matches = Vec::new();
        for (helper_uri, helper_index, rank) in visible {
            check_navigation_cancel(cancel)?;
            let Some(helper_document) = self.documents.get(&helper_uri) else {
                continue;
            };
            let Some(helper) = helper_document.helpers.get(helper_index) else {
                continue;
            };
            if helper.kind != target.kind {
                continue;
            }
            match self.helper_target_matches_with_budget(
                &helper_uri,
                helper_document,
                helper,
                target,
                cancel,
                budget,
            )? {
                HelperTargetMatch::No => {}
                HelperTargetMatch::Yes(distance) => matches.push((
                    rank.with_target_distance(distance),
                    helper_uri,
                    helper_index,
                )),
                HelperTargetMatch::Unknown if helper_target_may_match(helper, target) => {
                    unknown_ranks.push(rank.with_unknown_target())
                }
                HelperTargetMatch::Unknown => {}
            }
        }

        let Some(best_rank) = matches.iter().map(|(rank, _, _)| *rank).max() else {
            return Ok(if unknown_ranks.is_empty() {
                HelperSelection::None
            } else {
                HelperSelection::Unknown
            });
        };
        if unknown_ranks.iter().any(|rank| *rank >= best_rank) {
            return Ok(HelperSelection::Unknown);
        }

        let best = matches
            .into_iter()
            .filter(|(rank, _, _)| *rank == best_rank)
            .collect::<Vec<_>>();
        if best.len() != 1 {
            return Ok(HelperSelection::Unknown);
        }
        let (_, uri, index) = best
            .into_iter()
            .next()
            .expect("a non-empty best helper set");
        Ok(HelperSelection::Selected { uri, index })
    }

    fn helper_target_matches_with_budget(
        &self,
        helper_uri: &Url,
        helper_document: &Document,
        helper: &HelperDefinition,
        target: &TypeInstance,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<HelperTargetMatch, String> {
        if helper.kind != target.kind {
            return Ok(HelperTargetMatch::No);
        }
        let Some(resolved) = self.helper_target_instance_with_budget(
            helper_uri,
            helper_document,
            helper,
            cancel,
            budget,
        )?
        else {
            return Ok(HelperTargetMatch::Unknown);
        };
        if target_instances_match_for_helper(
            &resolved,
            target,
            helper.target.args.is_empty(),
            helper_uri,
            &helper.generic_parameters,
        ) {
            return Ok(HelperTargetMatch::Yes(0));
        }

        if target.kind != TypeKind::Class {
            return Ok(HelperTargetMatch::No);
        }

        let mut ancestry_state = AncestryResolutionState::new();
        let distance = self.type_ancestor_distance_with_budget(
            target,
            &resolved,
            &mut ancestry_state,
            cancel,
            budget,
        )?;
        let distance = match distance {
            TypeAncestorMatch::No => return Ok(HelperTargetMatch::No),
            TypeAncestorMatch::Distance(distance) => distance,
            TypeAncestorMatch::Unknown => return Ok(HelperTargetMatch::Unknown),
        };

        let mut resolution_state = ResolutionState::new();
        let Some(substitution) = self.member_owner_substitution_with_budget(
            &target.uri,
            &target.key,
            &target.substitution,
            &resolved.uri,
            &resolved.key,
            &mut resolution_state,
            cancel,
            budget,
        )?
        else {
            return Ok(HelperTargetMatch::Unknown);
        };
        let ancestor = TypeInstance {
            uri: resolved.uri.clone(),
            key: resolved.key.clone(),
            kind: resolved.kind,
            scope: ROOT_SCOPE,
            parameter_names: resolved.parameter_names.clone(),
            substitution,
            helper_owner: None,
        };
        Ok(
            if target_instances_match_for_helper(
                &resolved,
                &ancestor,
                helper.target.args.is_empty(),
                helper_uri,
                &helper.generic_parameters,
            ) {
                HelperTargetMatch::Yes(distance)
            } else {
                HelperTargetMatch::No
            },
        )
    }

    fn helper_target_instance_with_budget(
        &self,
        helper_uri: &Url,
        helper_document: &Document,
        helper: &HelperDefinition,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<TypeInstance>, String> {
        let Some(lookup_identifier) = assistance::identifier_at_with_budget(
            helper_document.tree.root_node(),
            helper.target.span.start,
            cancel,
            budget,
            "class/record helper target",
        )?
        else {
            return Ok(None);
        };
        let substitution =
            symbolic_generic_substitution(helper_uri, &helper.generic_parameters, ROOT_SCOPE);
        let mut state = ResolutionState::new();
        let receivers = self.type_receivers_for_type_ref_with_budget(
            helper_uri,
            helper_document,
            helper.target.span.start,
            &helper.target,
            lookup_identifier,
            None,
            &substitution,
            &mut state,
            cancel,
            budget,
        )?;
        if state.receiver_resolution_uncertain() {
            return Ok(None);
        }
        Ok(unique_type_instance(receivers))
    }

    fn type_ancestor_distance_with_budget(
        &self,
        current: &TypeInstance,
        sought: &TypeInstance,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<TypeAncestorMatch, String> {
        check_navigation_cancel(cancel)?;
        if current.uri == sought.uri && current.key == sought.key {
            return Ok(TypeAncestorMatch::Distance(0));
        }

        let ancestry = self.resolve_type_ancestry_with_budget(
            &current.uri,
            &current.key,
            state,
            cancel,
            budget,
        )?;
        if ancestry.status != AncestryStatus::Complete {
            return Ok(TypeAncestorMatch::Unknown);
        }

        let mut best_distance: Option<usize> = None;
        let mut unknown = false;
        for (parent_uri, parent_key) in ancestry.parents {
            check_navigation_cancel(cancel)?;
            let parent = self.type_instance_for_key(
                &parent_uri,
                &parent_key,
                ROOT_SCOPE,
                &GenericSubstitution::empty(),
            );
            match self.type_ancestor_distance_with_budget(&parent, sought, state, cancel, budget)? {
                TypeAncestorMatch::No => {}
                TypeAncestorMatch::Distance(distance) => {
                    let distance = distance.saturating_add(1);
                    best_distance = Some(best_distance.map_or(distance, |best| best.min(distance)));
                }
                TypeAncestorMatch::Unknown => unknown = true,
            }
        }

        Ok(match (best_distance, unknown) {
            (Some(_), true) => TypeAncestorMatch::Unknown,
            (Some(distance), false) => TypeAncestorMatch::Distance(distance),
            (None, true) => TypeAncestorMatch::Unknown,
            (None, false) => TypeAncestorMatch::No,
        })
    }

    fn helper_target_for_owner_with_budget(
        &self,
        helper_uri: &Url,
        helper_document: &Document,
        owner_key: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<TypeInstance>, String> {
        Ok(self
            .helper_target_for_owner_status_with_budget(
                helper_uri,
                helper_document,
                owner_key,
                cancel,
                budget,
            )?
            .selected())
    }

    fn helper_target_for_owner_status_with_budget(
        &self,
        helper_uri: &Url,
        helper_document: &Document,
        owner_key: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<OwnerHelperLookup, String> {
        let helpers = helper_document
            .helpers
            .iter()
            .enumerate()
            .filter(|(_, helper)| helper.key == owner_key)
            .collect::<Vec<_>>();
        if helpers.is_empty() {
            return Ok(OwnerHelperLookup::None);
        }
        if helpers.len() != 1 {
            return Ok(OwnerHelperLookup::Unknown);
        }
        let (helper_index, helper) = helpers[0];
        let Some(mut target) = self.helper_target_instance_with_budget(
            helper_uri,
            helper_document,
            helper,
            cancel,
            budget,
        )?
        else {
            return Ok(OwnerHelperLookup::Unknown);
        };
        target.helper_owner = Some(HelperOwner {
            uri: helper_uri.clone(),
            index: helper_index,
        });
        Ok(OwnerHelperLookup::Selected(target))
    }

    fn helper_target_for_owner(
        &self,
        helper_uri: &Url,
        helper_document: &Document,
        owner_key: &str,
    ) -> Option<TypeInstance> {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "helper owner target",
        );
        self.helper_target_for_owner_with_budget(
            helper_uri,
            helper_document,
            owner_key,
            &cancel,
            &mut budget,
        )
        .ok()
        .flatten()
    }

    #[allow(clippy::too_many_arguments)]
    fn helper_member_candidates_for_definition_with_budget(
        &self,
        current_uri: &Url,
        helper_uri: &Url,
        helper_index: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        let identity = (
            helper_uri.clone(),
            helper_index,
            member_key.map(str::to_owned),
            allow_implementation,
        );
        if !state.active_helpers.insert(identity.clone()) {
            return Ok(MemberLookup::unknown(Vec::new()));
        }

        let result = (|| {
            let Some(helper_document) = self.documents.get(helper_uri) else {
                return Ok(MemberLookup::unknown(Vec::new()));
            };
            let Some(helper) = helper_document.helpers.get(helper_index) else {
                return Ok(MemberLookup::unknown(Vec::new()));
            };
            let direct = self.direct_member_candidates_with_budget(
                helper_uri,
                &helper.key,
                ROOT_SCOPE,
                member_key,
                allow_implementation && helper_uri == current_uri,
                cancel,
                budget,
            )?;
            let Some(parent) = helper.parent.as_ref() else {
                return Ok(MemberLookup::known(direct));
            };
            let Some((parent_uri, parent_index)) = self.helper_parent_definition_with_budget(
                helper_uri,
                helper_document,
                parent,
                cancel,
                budget,
            )?
            else {
                return Ok(MemberLookup::unknown(Vec::new()));
            };
            let parent_lookup = self.helper_member_candidates_for_definition_with_budget(
                current_uri,
                &parent_uri,
                parent_index,
                member_key,
                allow_implementation && parent_uri == *current_uri,
                state,
                cancel,
                budget,
            )?;
            if !parent_lookup.ancestry_known {
                return Ok(MemberLookup::unknown(Vec::new()));
            }
            Ok(self.merge_member_candidates(direct, vec![parent_lookup], member_key))
        })();
        state.active_helpers.remove(&identity);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_candidates_for_helper_owner_in_context_with_budget(
        &self,
        current_uri: &Url,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        _substitution: &GenericSubstitution,
        helper_owner: &HelperOwner,
        member_key: Option<&str>,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        let mut state = AncestryResolutionState::new();
        let helper = self.helper_member_candidates_for_definition_with_budget(
            current_uri,
            &helper_owner.uri,
            helper_owner.index,
            member_key,
            allow_implementation,
            &mut state,
            cancel,
            budget,
        )?;
        if !helper.ancestry_known {
            return Ok(helper);
        }

        let ordinary = self.member_candidates_for_type_with_state_and_budget(
            type_uri,
            type_key,
            type_scope,
            member_key,
            allow_implementation,
            &mut state,
            cancel,
            budget,
        )?;
        let ordinary_ancestry_known = ordinary.ancestry_known;
        if let Some(member_key) = member_key {
            let lookup =
                if !helper.candidates.is_empty() || helper.ambiguous_names.contains(member_key) {
                    helper
                } else {
                    ordinary
                };
            return Ok(lookup.with_ancestry_known(ordinary_ancestry_known));
        }

        let helper_keys = helper
            .candidates
            .iter()
            .filter_map(|candidate| self.symbol(candidate).map(|symbol| symbol.key.clone()))
            .collect::<HashSet<_>>();
        let mut candidates = helper.candidates;
        for candidate in ordinary.candidates {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if !helper_keys.contains(&symbol.key) {
                candidates.push(candidate);
            }
        }
        let mut ambiguous_names = helper.ambiguous_names;
        for name in ordinary.ambiguous_names {
            ambiguous_names.insert(name);
        }
        Ok((if ambiguous_names.is_empty() {
            MemberLookup::known(candidates)
        } else {
            MemberLookup::ambiguous(candidates, ambiguous_names)
        })
        .with_ancestry_known(ordinary_ancestry_known))
    }

    #[allow(clippy::too_many_arguments)]
    fn member_candidates_for_helper_owner_in_context(
        &self,
        current_uri: &Url,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        _substitution: &GenericSubstitution,
        helper_owner: &HelperOwner,
        member_key: Option<&str>,
        allow_implementation: bool,
    ) -> MemberLookup {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "lexical helper member lookup",
        );
        self.member_candidates_for_helper_owner_in_context_with_budget(
            current_uri,
            type_uri,
            type_key,
            type_scope,
            _substitution,
            helper_owner,
            member_key,
            allow_implementation,
            &cancel,
            &mut budget,
        )
        .unwrap_or_else(|_| MemberLookup::unknown(Vec::new()))
    }

    fn helper_parent_definition_with_budget(
        &self,
        helper_uri: &Url,
        helper_document: &Document,
        parent: &TypeRef,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<(Url, usize)>, String> {
        let Some(lookup_identifier) = assistance::identifier_at_with_budget(
            helper_document.tree.root_node(),
            parent.span.start,
            cancel,
            budget,
            "helper ancestry",
        )?
        else {
            return Ok(None);
        };
        let mut state = ResolutionState::new();
        let receivers = self.type_receivers_for_type_ref_with_budget(
            helper_uri,
            helper_document,
            parent.span.start,
            parent,
            lookup_identifier,
            None,
            &GenericSubstitution::empty(),
            &mut state,
            cancel,
            budget,
        )?;
        if state.receiver_resolution_uncertain() || receivers.len() != 1 {
            return Ok(None);
        }
        let Some(Receiver::Type(parent_instance)) = receivers.into_iter().next() else {
            return Ok(None);
        };
        let Some(parent_document) = self.documents.get(&parent_instance.uri) else {
            return Ok(None);
        };
        let matches = parent_document
            .helpers
            .iter()
            .enumerate()
            .filter(|(_, helper)| helper.key == parent_instance.key)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Ok(None);
        }
        Ok(Some((parent_instance.uri, matches[0])))
    }

    #[allow(clippy::too_many_arguments)]
    fn member_candidates_for_type_in_context_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        substitution: &GenericSubstitution,
        member_key: Option<&str>,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        let mut ancestry_state = AncestryResolutionState::new();
        let ordinary = self.member_candidates_for_type_with_state_and_budget(
            type_uri,
            type_key,
            type_scope,
            member_key,
            allow_implementation,
            &mut ancestry_state,
            cancel,
            budget,
        )?;
        if !ordinary.ancestry_known {
            return Ok(ordinary);
        }
        if current_document.offset_is_in_helper_declaration(offset) {
            return Ok(ordinary);
        }

        let target = self.type_instance_for_key(type_uri, type_key, type_scope, substitution);
        let helper = match self.active_helper_for_type_with_budget(
            current_uri,
            current_document,
            offset,
            &target,
            cancel,
            budget,
        )? {
            HelperSelection::None => MemberLookup::known(Vec::new()),
            HelperSelection::Unknown => MemberLookup::unknown(Vec::new()),
            HelperSelection::Selected { uri, index } => self
                .helper_member_candidates_for_definition_with_budget(
                    current_uri,
                    &uri,
                    index,
                    member_key,
                    allow_implementation,
                    &mut ancestry_state,
                    cancel,
                    budget,
                )?,
        };
        if !helper.ancestry_known {
            return Ok(MemberLookup::unknown(ordinary.candidates));
        }

        if let Some(member_key) = member_key {
            return Ok(
                if !helper.candidates.is_empty() || helper.ambiguous_names.contains(member_key) {
                    helper
                } else {
                    ordinary
                },
            );
        }

        let helper_keys = helper
            .candidates
            .iter()
            .filter_map(|candidate| self.symbol(candidate).map(|symbol| symbol.key.clone()))
            .collect::<HashSet<_>>();
        let mut candidates = helper.candidates;
        for candidate in ordinary.candidates {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if !helper_keys.contains(&symbol.key) {
                candidates.push(candidate);
            }
        }
        let mut ambiguous_names = ordinary.ambiguous_names;
        for name in helper.ambiguous_names {
            ambiguous_names.insert(name);
        }
        if ambiguous_names.is_empty() {
            Ok(MemberLookup::known(candidates))
        } else {
            Ok(MemberLookup::ambiguous(candidates, ambiguous_names))
        }
    }

    fn type_instance_for_key(
        &self,
        uri: &Url,
        key: &str,
        scope: usize,
        substitution: &GenericSubstitution,
    ) -> TypeInstance {
        let mut instance = TypeInstance {
            uri: uri.clone(),
            key: key.to_owned(),
            kind: self.type_kind(uri, key).unwrap_or(TypeKind::Other),
            scope,
            parameter_names: Vec::new(),
            substitution: substitution.clone(),
            helper_owner: None,
        };
        if let Some(document) = self.documents.get(uri) {
            if let Some(indices) = document.type_symbol_indices.get(key) {
                if indices.len() == 1 {
                    if let Some(symbol) = document.symbols.get(indices[0]) {
                        instance.parameter_names = symbol
                            .generic_parameters
                            .iter()
                            .map(|parameter| parameter.name.clone())
                            .collect();
                        instance.kind = symbol.type_kind;
                    }
                }
            }
        }
        instance
    }

    #[allow(clippy::too_many_arguments)]
    fn member_type_receivers(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_uri: &Url,
        type_key: &str,
        name: &str,
        allow_implementation: bool,
        type_scope: usize,
        substitution: &GenericSubstitution,
        helper_owner: Option<&HelperOwner>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let member_key = canonical_name(name);
        let resolution_key = (
            type_uri.clone(),
            type_key.to_owned(),
            member_key.clone(),
            type_scope,
            substitution.clone(),
        );
        if !state.active_members.insert(resolution_key.clone()) {
            return Vec::new();
        }
        let lookup = if let Some(helper_owner) = helper_owner {
            self.member_candidates_for_helper_owner_in_context(
                current_uri,
                type_uri,
                type_key,
                type_scope,
                substitution,
                helper_owner,
                Some(&member_key),
                allow_implementation,
            )
        } else {
            self.member_references_for_type_in_context(
                current_uri,
                current_document,
                offset,
                type_uri,
                type_key,
                type_scope,
                substitution,
                &member_key,
                allow_implementation,
            )
        };
        if !lookup.ancestry_known || (lookup.implicit_member_known && lookup.candidates.is_empty())
        {
            state.mark_receiver_uncertain();
            state.active_members.remove(&resolution_key);
            return Vec::new();
        }
        let candidates = lookup.candidates;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            state.active_members.remove(&resolution_key);
            return Vec::new();
        }
        let mut accessible = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            match self.candidate_access_decision(
                current_uri,
                current_document,
                offset,
                &candidate,
                state,
            ) {
                AccessDecision::Visible => accessible.push(candidate),
                AccessDecision::Unknown => state.mark_receiver_uncertain(),
                AccessDecision::Inaccessible => {}
            }
        }
        let candidates = accessible;
        if state.receiver_resolution_uncertain() {
            state.active_members.remove(&resolution_key);
            return Vec::new();
        }
        let constructor_keys = candidates
            .iter()
            .filter_map(|candidate| {
                let symbol = self.symbol(candidate)?;
                (symbol.kind == SymbolKind::Routine
                    && symbol.routine_kind == RoutineKind::Constructor)
                    .then(|| {
                        symbol
                            .routine_key
                            .clone()
                            .map(|key| (candidate.uri.clone(), key))
                    })
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let constructor_is_unique = constructor_keys.len() == 1;
        let mut result = Vec::new();
        for candidate in candidates {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine && symbol.routine_kind == RoutineKind::Constructor
            {
                if constructor_is_unique {
                    result.push(Receiver::Type(TypeInstance {
                        uri: type_uri.clone(),
                        key: type_key.to_owned(),
                        kind: TypeKind::Other,
                        scope: type_scope,
                        parameter_names: Vec::new(),
                        substitution: substitution.clone(),
                        helper_owner: None,
                    }));
                }
                continue;
            }
            let owner_key = symbol.owner_type.as_deref().unwrap_or(type_key);
            let member_substitution = self
                .member_owner_substitution(
                    type_uri,
                    type_key,
                    substitution,
                    &candidate.uri,
                    owner_key,
                    state,
                )
                .or_else(|| {
                    self.candidate_is_helper_member(&candidate)
                        .then(|| substitution.clone())
                });
            let Some(member_substitution) = member_substitution else {
                continue;
            };
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            result.extend(self.type_receivers_for_symbol_type(
                &candidate.uri,
                document,
                symbol,
                None,
                &member_substitution,
                state,
            ));
        }
        state.active_members.remove(&resolution_key);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_for_type_in_context(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        substitution: &GenericSubstitution,
        member_key: &str,
        allow_implementation: bool,
    ) -> MemberLookup {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "contextual member lookup",
        );
        self.member_references_for_type_in_context_with_budget(
            current_uri,
            current_document,
            offset,
            type_uri,
            type_key,
            type_scope,
            substitution,
            member_key,
            allow_implementation,
            &cancel,
            &mut budget,
        )
        .unwrap_or_else(|_| MemberLookup::unknown(Vec::new()))
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_for_type_in_context_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        substitution: &GenericSubstitution,
        member_key: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        self.member_candidates_for_type_in_context_with_budget(
            current_uri,
            current_document,
            offset,
            type_uri,
            type_key,
            type_scope,
            substitution,
            Some(member_key),
            allow_implementation,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_for_instance_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        instance: &TypeInstance,
        member_key: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        if let Some(helper_owner) = instance.helper_owner.as_ref() {
            self.member_candidates_for_helper_owner_in_context_with_budget(
                current_uri,
                &instance.uri,
                &instance.key,
                instance.scope,
                &instance.substitution,
                helper_owner,
                Some(member_key),
                allow_implementation,
                cancel,
                budget,
            )
        } else {
            self.member_references_for_type_in_context_with_budget(
                current_uri,
                current_document,
                offset,
                &instance.uri,
                &instance.key,
                instance.scope,
                &instance.substitution,
                member_key,
                allow_implementation,
                cancel,
                budget,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_for_instance(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        instance: &TypeInstance,
        member_key: &str,
        allow_implementation: bool,
    ) -> MemberLookup {
        if let Some(helper_owner) = instance.helper_owner.as_ref() {
            self.member_candidates_for_helper_owner_in_context(
                current_uri,
                &instance.uri,
                &instance.key,
                instance.scope,
                &instance.substitution,
                helper_owner,
                Some(member_key),
                allow_implementation,
            )
        } else {
            self.member_references_for_type_in_context(
                current_uri,
                current_document,
                offset,
                &instance.uri,
                &instance.key,
                instance.scope,
                &instance.substitution,
                member_key,
                allow_implementation,
            )
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn member_references_for_type_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let mut state = AncestryResolutionState::new();
        Ok(self
            .member_candidates_for_type_with_state_and_budget(
                type_uri,
                type_key,
                type_scope,
                Some(member_key),
                allow_implementation,
                &mut state,
                cancel,
                budget,
            )?
            .candidates)
    }

    #[allow(clippy::too_many_arguments)]
    fn member_candidates_for_completion_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        substitution: &GenericSubstitution,
        helper_owner: Option<&HelperOwner>,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        if let Some(helper_owner) = helper_owner {
            self.member_candidates_for_helper_owner_in_context_with_budget(
                current_uri,
                type_uri,
                type_key,
                type_scope,
                substitution,
                helper_owner,
                None,
                allow_implementation,
                cancel,
                budget,
            )
        } else {
            self.member_candidates_for_type_in_context_with_budget(
                current_uri,
                current_document,
                offset,
                type_uri,
                type_key,
                type_scope,
                substitution,
                None,
                allow_implementation,
                cancel,
                budget,
            )
        }
    }

    #[cfg(test)]
    fn member_candidates_for_type_with_state(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        state: &mut AncestryResolutionState,
    ) -> MemberLookup {
        let identity = (
            type_uri.clone(),
            type_key.to_owned(),
            type_scope,
            member_key.map(str::to_owned),
            allow_implementation,
        );
        if let Some(resolved) = state.resolved_members.get(&identity) {
            return resolved.clone();
        }
        if !state.active_members.insert(identity.clone()) {
            return MemberLookup::unknown(Vec::new());
        }

        let result = if !state.take_work() {
            MemberLookup::unknown(Vec::new())
        } else {
            let direct = self.direct_member_candidates(
                type_uri,
                type_key,
                type_scope,
                member_key,
                allow_implementation,
            );
            if self.compiled_shell_members_are_opaque(type_uri) {
                return MemberLookup::unknown(direct);
            }
            let implicit_root_member_known =
                self.implicit_root_member_known(type_uri, type_key, member_key);
            let ancestry = if self.type_requires_ancestry(type_uri, type_key) {
                self.resolve_type_ancestry(type_uri, type_key, state)
            } else {
                complete_ancestry(Vec::new())
            };
            if ancestry.status != AncestryStatus::Complete {
                MemberLookup::unknown(direct).with_implicit_member_known(implicit_root_member_known)
            } else {
                let mut parent_lookups = Vec::with_capacity(ancestry.parents.len());
                let mut ancestry_known = true;
                for (parent_uri, parent_key) in ancestry.parents {
                    let lookup = self.member_candidates_for_type_with_state(
                        &parent_uri,
                        &parent_key,
                        ROOT_SCOPE,
                        member_key,
                        allow_implementation,
                        state,
                    );
                    if !lookup.ancestry_known {
                        ancestry_known = false;
                        break;
                    }
                    parent_lookups.push(lookup);
                }
                if ancestry_known {
                    self.merge_member_candidates(direct, parent_lookups, member_key)
                        .with_implicit_member_known(implicit_root_member_known)
                } else {
                    MemberLookup::unknown(direct)
                        .with_implicit_member_known(implicit_root_member_known)
                }
            }
        };
        state.active_members.remove(&identity);
        state.resolved_members.insert(identity, result.clone());
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_candidates_for_type_with_state_and_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        let identity = (
            type_uri.clone(),
            type_key.to_owned(),
            type_scope,
            member_key.map(str::to_owned),
            allow_implementation,
        );
        if let Some(resolved) = state.resolved_members.get(&identity) {
            return Ok(resolved.clone());
        }
        if !state.active_members.insert(identity.clone()) {
            return Ok(MemberLookup::unknown(Vec::new()));
        }

        let result = (|| {
            check_navigation_cancel(cancel)?;
            if !state.take_work() {
                return Ok(MemberLookup::unknown(Vec::new()));
            }
            let direct = self.direct_member_candidates_with_budget(
                type_uri,
                type_key,
                type_scope,
                member_key,
                allow_implementation,
                cancel,
                budget,
            )?;
            if self.compiled_shell_members_are_opaque(type_uri) {
                return Ok(MemberLookup::unknown(direct));
            }
            let implicit_root_member_known =
                self.implicit_root_member_known(type_uri, type_key, member_key);
            let ancestry = if self.type_requires_ancestry(type_uri, type_key) {
                self.resolve_type_ancestry_with_budget(type_uri, type_key, state, cancel, budget)?
            } else {
                complete_ancestry(Vec::new())
            };
            if ancestry.status != AncestryStatus::Complete {
                return Ok(MemberLookup::unknown(direct)
                    .with_implicit_member_known(implicit_root_member_known));
            }

            let mut parent_lookups = Vec::with_capacity(ancestry.parents.len());
            let mut ancestry_known = true;
            for (parent_uri, parent_key) in ancestry.parents {
                let lookup = self.member_candidates_for_type_with_state_and_budget(
                    &parent_uri,
                    &parent_key,
                    ROOT_SCOPE,
                    member_key,
                    allow_implementation,
                    state,
                    cancel,
                    budget,
                )?;
                if !lookup.ancestry_known {
                    ancestry_known = false;
                    break;
                }
                parent_lookups.push(lookup);
            }
            Ok(if ancestry_known {
                self.merge_member_candidates(direct, parent_lookups, member_key)
                    .with_implicit_member_known(implicit_root_member_known)
            } else {
                MemberLookup::unknown(direct).with_implicit_member_known(implicit_root_member_known)
            })
        })();
        state.active_members.remove(&identity);
        if let Ok(resolved) = &result {
            state.resolved_members.insert(identity, resolved.clone());
        }
        result
    }

    fn direct_member_candidates(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
    ) -> Vec<Candidate> {
        let Some(document) = self.documents.get(type_uri) else {
            return Vec::new();
        };
        let indices = match member_key {
            Some(member_key) => document
                .member_symbol_indices
                .get(&(type_key.to_owned(), member_key.to_owned())),
            None => document.member_symbol_indices_by_owner.get(type_key),
        };
        let Some(indices) = indices else {
            return Vec::new();
        };
        indices
            .iter()
            .filter_map(|index| {
                let symbol = document.symbols.get(*index)?;
                (member_symbol_is_visible(document, symbol, type_key, allow_implementation)
                    && (type_scope == ROOT_SCOPE || symbol.scope == type_scope))
                    .then(|| Candidate {
                        uri: type_uri.clone(),
                        index: *index,
                    })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn direct_member_candidates_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(
            type_key
                .len()
                .saturating_add(member_key.map_or(0, str::len)),
            cancel,
        )?;
        let Some(document) = self.documents.get(type_uri) else {
            return Ok(Vec::new());
        };
        let indices = match member_key {
            Some(member_key) => document
                .member_symbol_indices
                .get(&(type_key.to_owned(), member_key.to_owned())),
            None => document.member_symbol_indices_by_owner.get(type_key),
        };
        let Some(indices) = indices else {
            return Ok(Vec::new());
        };
        budget.require_work(indices.len(), cancel)?;
        budget.require_bytes(
            type_uri.as_str().len().saturating_mul(indices.len()),
            cancel,
        )?;
        #[cfg(test)]
        test_record_materialization(&TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS);
        let mut result = Vec::with_capacity(indices.len());
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if member_symbol_is_visible(document, symbol, type_key, allow_implementation)
                && (type_scope == ROOT_SCOPE || symbol.scope == type_scope)
            {
                result.push(Candidate {
                    uri: type_uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(result)
    }

    fn type_requires_ancestry(&self, type_uri: &Url, type_key: &str) -> bool {
        let Some(document) = self.documents.get(type_uri) else {
            return true;
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return true;
        };
        entries.len() != 1 || entries[0].parent_declared
    }

    fn implicit_root_member_known(
        &self,
        type_uri: &Url,
        type_key: &str,
        member_key: Option<&str>,
    ) -> bool {
        let Some(member_key) = member_key else {
            return false;
        };
        let Some(document) = self.documents.get(type_uri) else {
            return false;
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return false;
        };
        entries.len() == 1
            && entries[0].kind == TypeKind::Class
            && !entries[0].parent_declared
            && is_implicit_tobject_member(member_key)
    }

    fn resolve_type_ancestry(
        &self,
        type_uri: &Url,
        type_key: &str,
        state: &mut AncestryResolutionState,
    ) -> TypeAncestryResolution {
        let identity = (type_uri.clone(), type_key.to_owned());
        if let Some(resolved) = state.resolved_types.get(&identity) {
            return resolved.clone();
        }
        if state.active_types.contains(&identity) {
            return unknown_ancestry();
        }
        let Some(document) = self.documents.get(type_uri) else {
            return unknown_ancestry();
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return unknown_ancestry();
        };
        if !state.take_work() {
            return unknown_ancestry();
        }
        state.active_types.insert(identity.clone());
        let mut result = if entries.len() == 1 {
            self.resolve_type_ancestry_entry(type_uri, type_key, &entries[0], document)
        } else {
            unknown_ancestry()
        };
        if result.status == AncestryStatus::Complete {
            for (parent_uri, parent_key) in result.parents.clone() {
                if self
                    .resolve_type_ancestry(&parent_uri, &parent_key, state)
                    .status
                    != AncestryStatus::Complete
                {
                    result = unknown_ancestry();
                    break;
                }
            }
        }
        state.active_types.remove(&identity);
        state.resolved_types.insert(identity, result.clone());
        result
    }

    fn resolve_type_ancestry_entry(
        &self,
        type_uri: &Url,
        type_key: &str,
        entry: &TypeAncestry,
        document: &Document,
    ) -> TypeAncestryResolution {
        let Some(type_indices) = document.type_symbol_indices.get(type_key) else {
            return unknown_ancestry();
        };
        if type_indices.len() != 1 {
            return unknown_ancestry();
        }
        let type_candidate = Candidate {
            uri: type_uri.clone(),
            index: type_indices[0],
        };
        if self.candidate_is_conditionally_unknown(&type_candidate) {
            return unknown_ancestry();
        }
        let Some(type_symbol) = self.symbol(&type_candidate) else {
            return unknown_ancestry();
        };
        if type_symbol.kind != SymbolKind::Type || type_symbol.type_kind != entry.kind {
            return unknown_ancestry();
        }
        if !matches!(entry.kind, TypeKind::Class | TypeKind::Interface) {
            return complete_ancestry(Vec::new());
        }
        if entry.parent_declared && entry.parents.is_empty() {
            return unknown_ancestry();
        }

        let mut parents = Vec::with_capacity(entry.parents.len());
        for parent in entry.parents.iter().filter(|parent| {
            matches!(
                (entry.kind, parent.relation),
                (TypeKind::Class, ParentRelation::Superclass)
                    | (TypeKind::Interface, ParentRelation::InterfaceParent)
            )
        }) {
            if parent.path.is_empty() || document.conditionals.is_unknown_at(parent.span.start) {
                return unknown_ancestry();
            }
            let candidates = dedup_candidates(self.type_parent_candidates(
                type_uri,
                document,
                entry.name_span.start,
                &parent.path,
            ));
            if candidates.len() != 1
                || candidates
                    .iter()
                    .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return unknown_ancestry();
            }
            let candidate = &candidates[0];
            let Some(symbol) = self.symbol(candidate) else {
                return unknown_ancestry();
            };
            if symbol.kind != SymbolKind::Type {
                return unknown_ancestry();
            }
            if entry.kind == TypeKind::Class && symbol.type_kind == TypeKind::Interface {
                continue;
            }
            let expected_kind = match entry.kind {
                TypeKind::Class => TypeKind::Class,
                TypeKind::Interface => TypeKind::Interface,
                _ => continue,
            };
            if symbol.type_kind != expected_kind {
                return unknown_ancestry();
            }
            parents.push((candidate.uri.clone(), symbol.key.clone()));
        }
        complete_ancestry(parents)
    }

    fn type_parent_candidates(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &[String],
    ) -> Vec<Candidate> {
        let Some(first) = path.first() else {
            return Vec::new();
        };
        let region = current_document.region_at(offset);
        if path.len() == 1 {
            let local = self.type_candidates_in_unit(
                current_uri,
                first,
                matches!(region, Region::Implementation | Region::Other),
            );
            if !local.is_empty() {
                return local;
            }
            let active_uses = current_document.active_uses(region);
            if active_uses
                .iter()
                .any(|unit| current_document.unknown_imports.contains(unit.as_str()))
            {
                return Vec::new();
            }
            return dedup_candidates(
                active_uses
                    .into_iter()
                    .flat_map(|unit| {
                        self.unit_urls_for_import(current_document, unit)
                            .into_iter()
                            .flat_map(|unit_uri| {
                                self.type_candidates_in_unit(&unit_uri, first, false)
                            })
                    })
                    .collect(),
            );
        }

        let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, path)
        else {
            return Vec::new();
        };
        if prefix_len.saturating_add(1) != path.len() {
            return Vec::new();
        }
        let Some(type_name) = path.get(prefix_len) else {
            return Vec::new();
        };
        let allow_implementation = matches!(region, Region::Implementation | Region::Other)
            && unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        dedup_candidates(
            unit_uris
                .into_iter()
                .flat_map(|unit_uri| {
                    self.type_candidates_in_unit(&unit_uri, type_name, allow_implementation)
                })
                .collect(),
        )
    }

    fn resolve_direct_type_ancestry_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<TypeAncestryResolution, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_bytes(
            type_uri.as_str().len().saturating_add(type_key.len()),
            cancel,
        )?;
        budget.require_owned_bytes(
            std::mem::size_of::<(Url, String)>()
                .saturating_add(type_uri.as_str().len())
                .saturating_add(type_key.len()),
            cancel,
        )?;
        let Some(document) = self.documents.get(type_uri) else {
            return Ok(unknown_ancestry());
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return Ok(unknown_ancestry());
        };
        if !state.take_work() {
            return Err("type hierarchy ancestry work limit exceeded".into());
        }
        if entries.len() != 1 {
            return Ok(unknown_ancestry());
        }
        let result = self.resolve_type_ancestry_entry_with_budget(
            type_uri,
            type_key,
            &entries[0],
            document,
            cancel,
            budget,
        )?;
        budget.require_owned_bytes(ancestry_resolution_owned_bytes(&result), cancel)?;
        Ok(result)
    }

    fn resolve_type_ancestry_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<TypeAncestryResolution, String> {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(
            std::mem::size_of::<(Url, String)>()
                .saturating_add(type_uri.as_str().len())
                .saturating_add(type_key.len()),
            cancel,
        )?;
        let identity = (type_uri.clone(), type_key.to_owned());
        if let Some(resolved) = state.resolved_types.get(&identity) {
            budget.require_owned_bytes(ancestry_resolution_owned_bytes(resolved), cancel)?;
            return Ok(resolved.clone());
        }
        if state.active_types.contains(&identity) {
            return Ok(unknown_ancestry());
        }
        let Some(document) = self.documents.get(type_uri) else {
            return Ok(unknown_ancestry());
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return Ok(unknown_ancestry());
        };
        if !state.take_work() {
            return Ok(unknown_ancestry());
        }
        budget.require_bytes(
            type_uri.as_str().len().saturating_add(type_key.len()),
            cancel,
        )?;
        budget.require_owned_bytes(
            std::mem::size_of::<(Url, String)>()
                .saturating_add(type_uri.as_str().len())
                .saturating_add(type_key.len()),
            cancel,
        )?;
        budget.require_owned_bytes(
            std::mem::size_of::<(Url, String)>()
                .saturating_add(type_uri.as_str().len())
                .saturating_add(type_key.len()),
            cancel,
        )?;
        state.active_types.insert(identity.clone());
        #[cfg(test)]
        test_record_semantic_ancestry_visit();
        let result = if entries.len() == 1 {
            self.resolve_type_ancestry_entry_with_budget(
                type_uri,
                type_key,
                &entries[0],
                document,
                cancel,
                budget,
            )
        } else {
            Ok(unknown_ancestry())
        };
        let mut result = match result {
            Ok(result) => result,
            Err(error) => {
                state.active_types.remove(&identity);
                return Err(error);
            }
        };
        if result.status == AncestryStatus::Complete {
            for (parent_uri, parent_key) in result.parents.clone() {
                if self
                    .resolve_type_ancestry_with_budget(
                        &parent_uri,
                        &parent_key,
                        state,
                        cancel,
                        budget,
                    )?
                    .status
                    != AncestryStatus::Complete
                {
                    result = unknown_ancestry();
                    break;
                }
            }
        }
        state.active_types.remove(&identity);
        budget.require_owned_bytes(ancestry_resolution_owned_bytes(&result), cancel)?;
        state.resolved_types.insert(identity, result.clone());
        Ok(result)
    }

    fn resolve_type_ancestry_entry_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        entry: &TypeAncestry,
        document: &Document,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<TypeAncestryResolution, String> {
        let Some(type_indices) = document.type_symbol_indices.get(type_key) else {
            return Ok(unknown_ancestry());
        };
        if type_indices.len() != 1 {
            return Ok(unknown_ancestry());
        }
        let type_candidate = Candidate {
            uri: type_uri.clone(),
            index: type_indices[0],
        };
        budget.require_work(1, cancel)?;
        if self.candidate_is_conditionally_unknown(&type_candidate) {
            return Ok(unknown_ancestry());
        }
        let Some(type_symbol) = self.symbol(&type_candidate) else {
            return Ok(unknown_ancestry());
        };
        if type_symbol.kind != SymbolKind::Type || type_symbol.type_kind != entry.kind {
            return Ok(unknown_ancestry());
        }
        if !matches!(entry.kind, TypeKind::Class | TypeKind::Interface) {
            return Ok(complete_ancestry(Vec::new()));
        }
        if entry.parent_declared && entry.parents.is_empty() {
            return Ok(unknown_ancestry());
        }

        budget.require_owned_bytes(
            entry
                .parents
                .len()
                .saturating_mul(std::mem::size_of::<(Url, String)>()),
            cancel,
        )?;
        let mut parents = Vec::with_capacity(entry.parents.len());
        let mut complete = true;
        for parent in entry.parents.iter().filter(|parent| {
            matches!(
                (entry.kind, parent.relation),
                (TypeKind::Class, ParentRelation::Superclass)
                    | (TypeKind::Interface, ParentRelation::InterfaceParent)
            )
        }) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if parent.path.is_empty() || document.conditionals.is_unknown_at(parent.span.start) {
                complete = false;
                continue;
            }
            let candidates = dedup_candidates(self.type_parent_candidates_with_budget(
                type_uri,
                document,
                entry.name_span.start,
                &parent.path,
                cancel,
                budget,
            )?);
            budget.require_work(candidates.len(), cancel)?;
            if candidates.len() != 1
                || candidates
                    .iter()
                    .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                complete = false;
                continue;
            }
            let candidate = &candidates[0];
            let Some(symbol) = self.symbol(candidate) else {
                complete = false;
                continue;
            };
            if symbol.kind != SymbolKind::Type {
                complete = false;
                continue;
            }
            if entry.kind == TypeKind::Class && symbol.type_kind == TypeKind::Interface {
                continue;
            }
            let expected_kind = match entry.kind {
                TypeKind::Class => TypeKind::Class,
                TypeKind::Interface => TypeKind::Interface,
                _ => continue,
            };
            if symbol.type_kind != expected_kind {
                complete = false;
                continue;
            }
            budget.require_owned_bytes(
                std::mem::size_of::<(Url, String)>()
                    .saturating_add(candidate.uri.as_str().len())
                    .saturating_add(symbol.key.len()),
                cancel,
            )?;
            parents.push((candidate.uri.clone(), symbol.key.clone()));
        }
        Ok(if complete {
            complete_ancestry(parents)
        } else {
            incomplete_ancestry(parents)
        })
    }

    fn type_parent_candidates_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &[String],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let Some(first) = path.first() else {
            return Ok(Vec::new());
        };
        budget.require_work(path.len().saturating_add(1), cancel)?;
        budget.require_bytes(
            path.iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(path.len().saturating_sub(1)),
            cancel,
        )?;
        let region = current_document.region_at(offset);
        if path.len() == 1 {
            let local = self.type_candidates_in_unit_with_budget(
                current_uri,
                first,
                matches!(region, Region::Implementation | Region::Other),
                cancel,
                budget,
            )?;
            if !local.is_empty() {
                return Ok(local);
            }
            let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
            if active_uses
                .iter()
                .any(|unit| current_document.unknown_imports.contains(unit.as_str()))
            {
                return Ok(Vec::new());
            }
            let mut imported = Vec::new();
            for unit in active_uses {
                for unit_uri in
                    self.unit_urls_for_import_with_budget(current_document, unit, cancel, budget)?
                {
                    imported.extend(self.type_candidates_in_unit_with_budget(
                        &unit_uri, first, false, cancel, budget,
                    )?);
                }
            }
            return Ok(dedup_candidates(imported));
        }

        let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            path,
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        if prefix_len.saturating_add(1) != path.len() {
            return Ok(Vec::new());
        }
        let Some(type_name) = path.get(prefix_len) else {
            return Ok(Vec::new());
        };
        let allow_implementation = matches!(region, Region::Implementation | Region::Other)
            && unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        let mut result = Vec::new();
        for unit_uri in unit_uris {
            result.extend(self.type_candidates_in_unit_with_budget(
                &unit_uri,
                type_name,
                allow_implementation,
                cancel,
                budget,
            )?);
        }
        Ok(dedup_candidates(result))
    }

    fn merge_member_candidates(
        &self,
        direct: Vec<Candidate>,
        parent_lookups: Vec<MemberLookup>,
        member_key: Option<&str>,
    ) -> MemberLookup {
        let implicit_member_known = parent_lookups
            .iter()
            .any(|lookup| lookup.implicit_member_known);
        if member_key.is_some() {
            let direct_has_routine = direct.iter().any(|candidate| {
                self.symbol(candidate)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Routine)
            });
            if !direct.is_empty() && !direct_has_routine {
                return MemberLookup::known(direct)
                    .with_implicit_member_known(implicit_member_known);
            }
            let direct_routines = direct
                .iter()
                .filter_map(|candidate| {
                    self.symbol(candidate)
                        .filter(|symbol| symbol.kind == SymbolKind::Routine)
                })
                .collect::<Vec<_>>();
            let direct_overrides = direct_routines
                .iter()
                .filter(|symbol| symbol.routine_directives.override_)
                .filter_map(|symbol| symbol.routine_signature.clone())
                .collect::<HashSet<_>>();
            let direct_overload_set = !direct_routines.is_empty()
                && direct_routines
                    .iter()
                    .all(|symbol| symbol.routine_directives.overload);
            let mut selected: Option<Vec<Candidate>> = None;
            for lookup in parent_lookups {
                let candidates = dedup_candidates(
                    lookup
                        .candidates
                        .into_iter()
                        .filter(|candidate| {
                            if direct_routines.is_empty() {
                                return true;
                            }
                            if !direct_overload_set {
                                return false;
                            }
                            self.symbol(candidate).is_some_and(|symbol| {
                                if symbol.kind != SymbolKind::Routine
                                    || !symbol.routine_directives.overload
                                {
                                    return false;
                                }
                                let overridden =
                                    symbol.routine_signature.as_ref().is_some_and(|signature| {
                                        direct_overrides.contains(signature)
                                            && (symbol.routine_directives.virtual_
                                                || symbol.routine_directives.dynamic)
                                    });
                                !overridden
                            })
                        })
                        .collect(),
                );
                if candidates.is_empty() {
                    continue;
                }
                if selected
                    .as_ref()
                    .is_some_and(|current| !candidate_sets_equal(current, &candidates))
                {
                    return MemberLookup::unknown(direct)
                        .with_implicit_member_known(implicit_member_known);
                }
                selected = Some(candidates);
            }
            let mut result = direct;
            if let Some(selected) = selected {
                result.extend(selected);
            }
            return MemberLookup::known(result).with_implicit_member_known(implicit_member_known);
        }

        let direct_keys: HashSet<String> = direct
            .iter()
            .filter_map(|candidate| self.symbol(candidate).map(|symbol| symbol.key.clone()))
            .collect();
        let mut key_order = Vec::new();
        let mut seen_keys = HashSet::new();
        let mut ambiguous_names = parent_lookups
            .iter()
            .flat_map(|lookup| lookup.ambiguous_names.iter().cloned())
            .collect::<HashSet<_>>();
        for key in &direct_keys {
            ambiguous_names.remove(key);
        }
        let branch_maps: Vec<HashMap<String, Vec<Candidate>>> = parent_lookups
            .into_iter()
            .map(|lookup| {
                let mut by_key = HashMap::new();
                for candidate in dedup_candidates(lookup.candidates) {
                    let Some(symbol) = self.symbol(&candidate) else {
                        continue;
                    };
                    if seen_keys.insert(symbol.key.clone()) {
                        key_order.push(symbol.key.clone());
                    }
                    push_unique_candidate(
                        by_key.entry(symbol.key.clone()).or_insert_with(Vec::new),
                        candidate,
                    );
                }
                by_key
            })
            .collect();

        let mut result = direct;
        for key in key_order {
            if direct_keys.contains(&key) {
                continue;
            }
            let mut selected: Option<Vec<Candidate>> = None;
            let mut ambiguous = false;
            for branch in &branch_maps {
                let Some(candidates) = branch.get(&key) else {
                    continue;
                };
                if selected
                    .as_ref()
                    .is_some_and(|current| !candidate_sets_equal(current, candidates))
                {
                    ambiguous = true;
                    break;
                }
                selected = Some(candidates.clone());
            }
            if ambiguous {
                ambiguous_names.insert(key);
            } else if !ambiguous_names.contains(&key) {
                if let Some(selected) = selected {
                    result.extend(selected);
                }
            }
        }
        if ambiguous_names.is_empty() {
            MemberLookup::known(result).with_implicit_member_known(implicit_member_known)
        } else {
            MemberLookup::ambiguous(result, ambiguous_names)
                .with_implicit_member_known(implicit_member_known)
        }
    }

    fn exported_references_for_document(&self, uri: &Url) -> Vec<Candidate> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        #[cfg(test)]
        TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|count| {
            count.set(
                count
                    .get()
                    .saturating_add(document.exported_symbol_indices.len()),
            );
        });
        document
            .exported_symbol_indices
            .iter()
            .filter_map(|index| document.symbols.get(*index).map(|_| *index))
            .map(|index| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect()
    }

    fn symbol(&self, candidate: &Candidate) -> Option<&Symbol> {
        self.documents
            .get(&candidate.uri)
            .and_then(|document| document.symbols.get(candidate.index))
    }

    fn candidate_is_helper_member(&self, candidate: &Candidate) -> bool {
        let Some(symbol) = self.symbol(candidate) else {
            return false;
        };
        let Some(owner_type) = symbol.owner_type.as_deref() else {
            return false;
        };
        let Some(document) = self.documents.get(&candidate.uri) else {
            return false;
        };
        if document
            .type_symbol_indices
            .get(owner_type)
            .is_none_or(|indices| indices.len() != 1)
        {
            return false;
        }
        document
            .helpers
            .iter()
            .filter(|helper| helper.key == owner_type)
            .count()
            == 1
    }

    fn type_kind(&self, uri: &Url, key: &str) -> Option<TypeKind> {
        let document = self.documents.get(uri)?;
        let indices = document.type_symbol_indices.get(key)?;
        if indices.len() != 1 {
            return None;
        }
        let symbol = document.symbols.get(*indices.first()?)?;
        (symbol.kind == SymbolKind::Type).then_some(symbol.type_kind)
    }

    fn expand_property_candidates(
        &self,
        references: Vec<Candidate>,
        target: NavigationTarget,
    ) -> Vec<Candidate> {
        if target == NavigationTarget::Declaration {
            return references;
        }

        references
            .into_iter()
            .flat_map(|candidate| {
                let Some(property) = self.symbol(&candidate) else {
                    return vec![candidate];
                };
                if property.kind != SymbolKind::Property {
                    return vec![candidate];
                }
                let Some(accessor) = property.accessor.as_deref() else {
                    return vec![candidate];
                };
                let Some(owner_type) = property.owner_type.as_deref() else {
                    return vec![candidate];
                };
                let key = canonical_name(accessor);
                let Some(document) = self.documents.get(&candidate.uri) else {
                    return vec![candidate];
                };
                let accessors: Vec<Candidate> = document
                    .symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, symbol)| {
                        symbol.owner_type.as_deref() == Some(owner_type)
                            && symbol.key == key
                            && matches!(symbol.kind, SymbolKind::Field | SymbolKind::Routine)
                    })
                    .map(|(index, _)| Candidate {
                        uri: candidate.uri.clone(),
                        index,
                    })
                    .collect();
                if accessors.is_empty() {
                    vec![candidate]
                } else {
                    accessors
                }
            })
            .collect()
    }

    fn locations_for(&self, references: Vec<Candidate>, target: NavigationTarget) -> Vec<Location> {
        let mut routine_groups: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
        let mut non_routines = Vec::new();
        for candidate in references {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if let (SymbolKind::Routine, Some(routine_key)) = (symbol.kind, &symbol.routine_key) {
                routine_groups
                    .entry((candidate.uri.to_string(), routine_key.clone()))
                    .or_default()
                    .push(candidate);
            } else {
                non_routines.push(candidate);
            }
        }

        let mut selected = non_routines;
        for candidates in routine_groups.into_values() {
            let declarations: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.origin == Origin::Declaration)
                })
                .cloned()
                .collect();
            let interface_declarations: Vec<Candidate> = declarations
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.region == Region::Interface)
                })
                .cloned()
                .collect();
            let definitions: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.origin == Origin::Definition)
                })
                .cloned()
                .collect();

            match target {
                NavigationTarget::Declaration => {
                    if !interface_declarations.is_empty() {
                        selected.extend(interface_declarations);
                    } else if !declarations.is_empty() {
                        selected.extend(declarations);
                    } else {
                        selected.extend(definitions);
                    }
                }
                NavigationTarget::Definition | NavigationTarget::Implementation => {
                    if !definitions.is_empty() {
                        selected.extend(definitions);
                    } else {
                        selected.extend(declarations);
                    }
                }
            }
        }

        let mut locations = Vec::new();
        let mut seen = HashSet::new();
        for candidate in selected {
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            let Some(symbol) = document.symbols.get(candidate.index) else {
                continue;
            };
            let Some(location) = location_for_span(&candidate.uri, &document.source, symbol.span)
            else {
                continue;
            };
            let deduplication_key = (
                location.uri.to_string(),
                location.range.start.line,
                location.range.start.character,
                location.range.end.line,
                location.range.end.character,
            );
            if seen.insert(deduplication_key) {
                locations.push(location);
            }
        }
        locations.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.range.start.line.cmp(&right.range.start.line))
                .then_with(|| left.range.start.character.cmp(&right.range.start.character))
        });
        locations
    }

    fn locations_for_with_budget(
        &self,
        references: Vec<Candidate>,
        target: NavigationTarget,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Location>, String> {
        budget.require_owned_bytes(
            references
                .len()
                .saturating_mul(std::mem::size_of::<Candidate>()),
            cancel,
        )?;
        let reference_uri_bytes = references
            .iter()
            .map(|candidate| candidate.uri.as_str().len())
            .sum::<usize>();
        budget.require_owned_bytes(
            reference_uri_bytes.saturating_mul(3).saturating_add(
                references
                    .len()
                    .saturating_mul(3)
                    .saturating_mul(std::mem::size_of::<Candidate>()),
            ),
            cancel,
        )?;
        let mut routine_groups: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
        let mut non_routines = Vec::with_capacity(references.len());
        budget.require_owned_bytes(
            references
                .len()
                .saturating_mul(std::mem::size_of::<Candidate>()),
            cancel,
        )?;
        for candidate in references {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if let (SymbolKind::Routine, Some(routine_key)) = (symbol.kind, &symbol.routine_key) {
                budget.require_owned_bytes(
                    std::mem::size_of::<((String, String), Vec<Candidate>)>()
                        .saturating_add(candidate.uri.as_str().len())
                        .saturating_add(routine_key.len())
                        .saturating_add(std::mem::size_of::<Candidate>()),
                    cancel,
                )?;
                routine_groups
                    .entry((candidate.uri.to_string(), routine_key.clone()))
                    .or_default()
                    .push(candidate);
            } else {
                non_routines.push(candidate);
            }
        }

        let mut selected = non_routines;
        for candidates in routine_groups.into_values() {
            check_navigation_cancel(cancel)?;
            budget.require_work(candidates.len(), cancel)?;
            let group_uri_bytes = candidates
                .iter()
                .map(|candidate| candidate.uri.as_str().len())
                .sum::<usize>();
            budget.require_owned_bytes(
                candidates
                    .len()
                    .saturating_mul(3)
                    .saturating_mul(std::mem::size_of::<Candidate>())
                    .saturating_add(group_uri_bytes.saturating_mul(3)),
                cancel,
            )?;
            let declarations: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.origin == Origin::Declaration)
                })
                .cloned()
                .collect();
            let interface_declarations: Vec<Candidate> = declarations
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.region == Region::Interface)
                })
                .cloned()
                .collect();
            let definitions: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.origin == Origin::Definition)
                })
                .cloned()
                .collect();
            budget.require_owned_bytes(
                (declarations.len() + interface_declarations.len() + definitions.len())
                    .saturating_mul(std::mem::size_of::<Candidate>()),
                cancel,
            )?;
            match target {
                NavigationTarget::Declaration => {
                    if !interface_declarations.is_empty() {
                        selected.extend(interface_declarations);
                    } else if !declarations.is_empty() {
                        selected.extend(declarations);
                    } else {
                        selected.extend(definitions);
                    }
                }
                NavigationTarget::Definition | NavigationTarget::Implementation => {
                    if !definitions.is_empty() {
                        selected.extend(definitions);
                    } else {
                        selected.extend(declarations);
                    }
                }
            }
        }

        let mut locations = Vec::with_capacity(selected.len());
        let mut seen = HashSet::new();
        budget.require_owned_bytes(
            selected
                .len()
                .saturating_mul(std::mem::size_of::<Location>()),
            cancel,
        )?;
        for candidate in selected {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            budget.require_owned_bytes(candidate.uri.as_str().len(), cancel)?;
            let Some(symbol) = document.symbols.get(candidate.index) else {
                continue;
            };
            let Some(location) = location_for_span(&candidate.uri, &document.source, symbol.span)
            else {
                continue;
            };
            budget.require_owned_bytes(candidate.uri.as_str().len(), cancel)?;
            let deduplication_key = (
                location.uri.to_string(),
                location.range.start.line,
                location.range.start.character,
                location.range.end.line,
                location.range.end.character,
            );
            budget.require_owned_bytes(deduplication_key.0.len(), cancel)?;
            if seen.insert(deduplication_key) {
                locations.push(location);
            }
        }
        budget.require_work(locations.len(), cancel)?;
        locations.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.range.start.line.cmp(&right.range.start.line))
                .then_with(|| left.range.start.character.cmp(&right.range.start.character))
        });
        Ok(locations)
    }
}

fn hash_map_clone_storage_bytes<K, V>(capacity: usize) -> usize {
    capacity
        .max(1)
        .saturating_mul(2)
        .saturating_mul(std::mem::size_of::<(K, V)>() + std::mem::size_of::<usize>())
}

fn clone_units_for_fix_all(
    units: &HashMap<String, Vec<Url>>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<HashMap<String, Vec<Url>>, String> {
    budget.require_owned_bytes(
        hash_map_clone_storage_bytes::<String, Vec<Url>>(units.capacity()),
        cancel,
    )?;
    for (name, providers) in units {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(
            name.len()
                .saturating_add(
                    providers
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Url>()),
                )
                .saturating_add(
                    providers
                        .iter()
                        .map(|provider| provider.as_str().len())
                        .sum::<usize>(),
                ),
            cancel,
        )?;
    }
    Ok(units.clone())
}

fn clone_providers_for_fix_all(
    providers: &HashMap<String, Vec<Url>>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<HashMap<String, Vec<Url>>, String> {
    budget.require_owned_bytes(
        hash_map_clone_storage_bytes::<String, Vec<Url>>(providers.capacity()),
        cancel,
    )?;
    for (name, candidates) in providers {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(
            name.len()
                .saturating_add(
                    candidates
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Url>()),
                )
                .saturating_add(
                    candidates
                        .iter()
                        .map(|candidate| candidate.as_str().len())
                        .sum::<usize>(),
                ),
            cancel,
        )?;
    }
    Ok(providers.clone())
}

fn clone_import_bindings_for_fix_all(
    bindings: Option<&HashMap<String, Url>>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<HashMap<String, Url>>, String> {
    let Some(bindings) = bindings else {
        return Ok(None);
    };
    budget.require_owned_bytes(
        hash_map_clone_storage_bytes::<String, Url>(bindings.capacity()),
        cancel,
    )?;
    for (name, provider) in bindings {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(name.len() + provider.as_str().len(), cancel)?;
    }
    Ok(Some(bindings.clone()))
}

/// Reserve a conservative amount of shared semantic work before parsing the
/// transformed document.  `Document::parse_with_cancel` builds the tree and
/// all derived symbol/index tables in one operation, so charging this bounded
/// reservation before entering it is preferable to charging a cheap source
/// length after the expensive allocations have already happened.
fn rebind_work_reservation(target: &Document) -> usize {
    target
        .symbols
        .len()
        .saturating_mul(64)
        .saturating_add(target.scopes.len().saturating_mul(32))
        .saturating_add(target.helpers.len().saturating_mul(16))
        .saturating_add(target.method_resolutions.len().saturating_mul(16))
        .saturating_add(target.interface_delegations.len().saturating_mul(16))
        .saturating_add(target.generic_parameter_contexts.len().saturating_mul(16))
}

fn rebind_owned_reservation(target: &Document) -> usize {
    target
        .parser_source
        .len()
        .saturating_add(target.unit_name.len())
        .saturating_add(target.unit_display_name.len())
        .saturating_add(
            target
                .symbols
                .len()
                .saturating_mul(std::mem::size_of::<Symbol>() + std::mem::size_of::<String>() * 4),
        )
        .saturating_add(target.scopes.len().saturating_mul(64))
        .saturating_add(target.helpers.len().saturating_mul(64))
        .saturating_add(target.method_resolutions.len().saturating_mul(32))
        .saturating_add(target.interface_delegations.len().saturating_mul(32))
        .saturating_add(target.generic_parameter_contexts.len().saturating_mul(32))
}

const ROOT_SCOPE: usize = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Region {
    Interface,
    Implementation,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Origin {
    Declaration,
    Definition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SymbolKind {
    Unit,
    Type,
    Routine,
    Variable,
    Constant,
    Parameter,
    Field,
    Property,
    EnumValue,
    Label,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Visibility {
    Public,
    Published,
    Private,
    Protected,
    StrictPrivate,
    StrictProtected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessDecision {
    Visible,
    Inaccessible,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum TypeKind {
    Class,
    Record,
    Interface,
    Enum,
    Array,
    DynamicArray,
    Pointer,
    Callable,
    String,
    File,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum TypeIdentity {
    Builtin(BuiltinType),
    IntegerLiteral(i128),
    Named {
        uri: Url,
        key: String,
        kind: TypeKind,
        args: Vec<TypeIdentity>,
    },
}

/// A deferred interface action needs more than nominal `TypeIdentity`: a
/// provider can keep the same named alias while changing its effective array,
/// pointer, or strong-typedef definition.  This bounded structural identity
/// freezes that meaning without weakening the nominal conformance relation
/// used by the rest of navigation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ContractTypeFingerprint {
    Builtin(BuiltinType),
    IntegerLiteral(i128),
    Named {
        uri: Url,
        key: String,
        kind: TypeKind,
        args: Vec<ContractTypeFingerprint>,
        definition: ContractTypeDefinitionFingerprint,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ContractTypeDefinitionFingerprint {
    Alias(Box<ContractTypeFingerprint>),
    Shape(Box<ContractTypeShapeFingerprint>),
    Source {
        text: String,
        dependencies: Vec<ContractDefinitionDependencyFingerprint>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ContractDefinitionDependencyFingerprint {
    Type(Box<ContractTypeFingerprint>),
    Symbol {
        uri: Url,
        key: String,
        kind: TypeKind,
        text: String,
        dependencies: Vec<ContractDefinitionDependencyFingerprint>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ContractTypeShapeFingerprint {
    Named(Box<ContractTypeFingerprint>),
    Pointer(Box<ContractTypeShapeFingerprint>),
    Array {
        element: Box<ContractTypeShapeFingerprint>,
        dynamic: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TypeRef {
    path: Vec<String>,
    args: Vec<TypeRef>,
    span: Span,
}

impl TypeRef {
    fn display(&self) -> String {
        let path = self.path.join(".");
        if self.args.is_empty() {
            return path;
        }
        format!(
            "{path}<{}>",
            self.args
                .iter()
                .map(TypeRef::display)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TypeShape {
    Named(TypeRef),
    Pointer(Box<TypeShape>),
    Array {
        element: Box<TypeShape>,
        dynamic: bool,
    },
    Callable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GenericParameter {
    name: String,
    span: Span,
    constraint: Option<TypeRef>,
    constraint_unsupported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GenericSubstitution(BTreeMap<String, ResolvedType>);

impl GenericSubstitution {
    fn empty() -> Self {
        Self(BTreeMap::new())
    }

    fn get(&self, name: &str) -> Option<&ResolvedType> {
        self.0.get(&canonical_name(name))
    }

    fn insert(&mut self, name: &str, value: ResolvedType) {
        self.0.insert(canonical_name(name), value);
    }

    fn remove(&mut self, name: &str) {
        self.0.remove(&canonical_name(name));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TypeInstance {
    uri: Url,
    key: String,
    kind: TypeKind,
    scope: usize,
    parameter_names: Vec<String>,
    substitution: GenericSubstitution,
    helper_owner: Option<HelperOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HelperOwner {
    uri: Url,
    index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ResolvedType {
    Builtin(BuiltinType),
    IntegerLiteral(i128),
    Named(TypeInstance),
}

impl ResolvedType {
    fn into_receiver(self) -> Option<Receiver> {
        Some(match self {
            Self::Builtin(builtin) => Receiver::Builtin(builtin),
            Self::IntegerLiteral(value) => Receiver::IntegerLiteral(value),
            Self::Named(instance) => Receiver::Type(instance),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RoutineKind {
    Procedure,
    Function,
    Constructor,
    Destructor,
    Operator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn from_node(node: Node<'_>) -> Self {
        Self {
            start: node.start_byte(),
            end: node.end_byte(),
        }
    }

    fn contains(self, other: Span) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    fn contains_offset(self, offset: usize) -> bool {
        self.start <= offset && (offset < self.end || self.start == self.end)
    }
}

#[derive(Debug)]
struct Scope {
    start: usize,
    end: usize,
    parent: Option<usize>,
    owner_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BudgetedLookup<T> {
    Value(T),
    Exhausted,
}

#[derive(Debug, Clone, Copy)]
struct IndexedInterval {
    start: usize,
    end: usize,
    value: usize,
}

#[derive(Debug)]
struct SourceIntervalIndex {
    ranges: Vec<IndexedInterval>,
    query_work: usize,
}

impl SourceIntervalIndex {
    fn from_intervals<I>(intervals: I) -> Self
    where
        I: IntoIterator<Item = (Span, usize)>,
    {
        let intervals = intervals
            .into_iter()
            .filter_map(|(span, value)| {
                (span.start < span.end).then_some(IndexedInterval {
                    start: span.start,
                    end: span.end,
                    value,
                })
            })
            .collect::<Vec<_>>();
        let mut events = Vec::with_capacity(intervals.len().saturating_mul(2));
        for (index, interval) in intervals.iter().enumerate() {
            events.push((interval.start, 1_u8, std::cmp::Reverse(interval.end), index));
            events.push((interval.end, 0_u8, std::cmp::Reverse(interval.start), index));
        }
        events.sort_unstable();

        let mut ranges: Vec<IndexedInterval> = Vec::new();
        let mut active: Vec<usize> = Vec::new();
        let mut previous = 0;
        let mut event_index = 0;
        while event_index < events.len() {
            let offset = events[event_index].0;
            if previous < offset {
                if let Some(&active_index) = active.last() {
                    let value = intervals[active_index].value;
                    match ranges.last_mut() {
                        Some(last) if last.end == previous && last.value == value => {
                            last.end = offset;
                        }
                        _ => {
                            ranges.push(IndexedInterval {
                                start: previous,
                                end: offset,
                                value,
                            });
                        }
                    }
                }
            }

            while event_index < events.len() && events[event_index].0 == offset {
                let (_, kind, _, interval_index) = events[event_index];
                if kind == 0 {
                    if active.last().copied() == Some(interval_index) {
                        active.pop();
                    } else if let Some(position) =
                        active.iter().position(|index| *index == interval_index)
                    {
                        active.remove(position);
                    }
                } else {
                    active.push(interval_index);
                }
                event_index += 1;
            }
            previous = offset;
        }

        let query_work = binary_search_work(ranges.len());
        Self { ranges, query_work }
    }

    fn at(&self, offset: usize) -> Option<usize> {
        let mut low = 0;
        let mut high = self.ranges.len();
        while low < high {
            let middle = low + (high - low) / 2;
            #[cfg(test)]
            TEST_SEMANTIC_INTERVAL_QUERY_COMPARISONS.with(|comparisons| {
                comparisons.set(comparisons.get().saturating_add(1));
            });
            if self.ranges[middle].start <= offset {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.checked_sub(1)
            .and_then(|index| (offset < self.ranges[index].end).then_some(self.ranges[index].value))
    }

    fn at_with_budget(
        &self,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<usize>, String> {
        budget.require_work(self.query_work, cancel)?;
        Ok(self.at(offset))
    }

    fn at_with_budget_or_unknown(
        &self,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<BudgetedLookup<Option<usize>>, String> {
        if !budget.take_work(self.query_work, cancel)? {
            return Ok(BudgetedLookup::Exhausted);
        }
        Ok(BudgetedLookup::Value(self.at(offset)))
    }
}

fn binary_search_work(len: usize) -> usize {
    let mut remaining = len;
    let mut work = 0;
    while remaining > 0 {
        work += 1;
        remaining /= 2;
    }
    work
}

#[derive(Debug)]
pub(super) struct GenericParameterContext {
    span: Span,
    names: HashSet<String>,
    parent: Option<usize>,
}

#[derive(Debug)]
pub(super) struct OwnerTypeContext {
    span: Span,
    owner_type: String,
}

#[derive(Debug, Clone)]
struct WithContext {
    body: Span,
    receiver_spans: Vec<Span>,
}

#[derive(Debug, Clone)]
enum WithLookup {
    NotFound,
    Found(Vec<Candidate>),
    Unknown,
}

#[derive(Debug, Clone)]
enum WithReceiverSlot {
    Known(Vec<Receiver>),
    Unknown,
}

#[derive(Debug)]
pub(super) struct AssistanceBudget {
    remaining_work: usize,
    remaining_bytes: usize,
    work_limit: usize,
    byte_limit: usize,
    operation: &'static str,
    exhausted: bool,
    #[cfg(test)]
    cancel_after_work: Option<usize>,
}

fn check_navigation_cancel(cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(test)]
    test_cancel_after_semantic_ancestry_if_requested(cancel);
    #[cfg(test)]
    if TEST_QUALIFIED_IDENTIFIER_CANCEL_REQUESTED.with(Cell::get) {
        cancel.store(true, Ordering::Relaxed);
    }
    if cancel.load(Ordering::Relaxed) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}

impl AssistanceBudget {
    pub(super) fn new(work_limit: usize, byte_limit: usize, operation: &'static str) -> Self {
        Self {
            remaining_work: work_limit,
            remaining_bytes: byte_limit,
            work_limit,
            byte_limit,
            operation,
            exhausted: false,
            #[cfg(test)]
            cancel_after_work: None,
        }
    }

    #[cfg(test)]
    pub(super) fn cancel_after_work(&mut self, work: usize) {
        self.cancel_after_work = Some(work);
    }

    pub(super) fn take_work(&mut self, amount: usize, cancel: &AtomicBool) -> Result<bool, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if amount > self.remaining_work {
            self.remaining_work = 0;
            self.exhausted = true;
            return Ok(false);
        }
        self.remaining_work -= amount;
        #[cfg(test)]
        if let Some(remaining) = self.cancel_after_work.as_mut() {
            if amount >= *remaining {
                *remaining = 0;
                cancel.store(true, Ordering::Relaxed);
            } else {
                *remaining -= amount;
            }
        }
        Ok(true)
    }

    pub(super) fn take_bytes(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if amount > self.remaining_bytes {
            self.remaining_bytes = 0;
            self.exhausted = true;
            return Ok(false);
        }
        self.remaining_bytes -= amount;
        Ok(true)
    }

    pub(super) fn require_work(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if self.take_work(amount, cancel)? {
            Ok(())
        } else {
            Err(format!(
                "{} exceeds the {}-node traversal limit",
                self.operation, self.work_limit
            ))
        }
    }

    pub(super) fn require_bytes(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if self.take_bytes(amount, cancel)? {
            Ok(())
        } else {
            Err(format!(
                "{} exceeds the {}-byte scan limit",
                self.operation, self.byte_limit
            ))
        }
    }

    pub(super) fn require_owned_bytes(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if self.take_bytes(amount, cancel)? {
            Ok(())
        } else {
            Err(format!(
                "{} exceeds the {}-byte proof/materialization limit",
                self.operation, self.byte_limit
            ))
        }
    }

    pub(super) fn exhausted(&self) -> bool {
        self.exhausted
    }
}

#[derive(Debug)]
struct Symbol {
    span: Span,
    declaration_span: Span,
    selection_span: Span,
    name: String,
    key: String,
    kind: SymbolKind,
    type_kind: TypeKind,
    routine_kind: RoutineKind,
    routine_directives: RoutineDirectives,
    parameter_mode: Option<ParameterMode>,
    scope: usize,
    owner_type: Option<String>,
    owner_type_name: Option<String>,
    visibility: Visibility,
    declaration_ordered: bool,
    generic_parameters: Vec<GenericParameter>,
    generic_parameter: Option<String>,
    type_name: Option<String>,
    type_ref: Option<TypeRef>,
    type_shape: Option<TypeShape>,
    result_type_name: Option<String>,
    result_type_ref: Option<TypeRef>,
    result_type_span: Option<Span>,
    region: Region,
    origin: Origin,
    is_static: bool,
    local_only: bool,
    routine_key: Option<String>,
    routine_signature: Option<String>,
    routine_header_span: Option<Span>,
    routine_parameter_spans: Vec<Span>,
    routine_parameters: Vec<RoutineParameter>,
    type_excerpt_end: Option<usize>,
    body_scope: Option<usize>,
    unresolved_abbreviated: bool,
    accessor: Option<String>,
}

#[derive(Debug, Clone)]
struct RoutineParameter {
    span: Span,
    type_span: Option<Span>,
    type_name: Option<String>,
    type_ref: Option<TypeRef>,
    type_shape: Option<TypeShape>,
    mode: ParameterMode,
    has_default: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct RoutineDirectives {
    overload: bool,
    virtual_: bool,
    dynamic: bool,
    override_: bool,
    reintroduce: bool,
    forward: bool,
    abstract_: bool,
    calling_convention: Option<CallingConvention>,
    calling_convention_unknown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallingConvention {
    Cdecl,
    Stdcall,
    Pascal,
    Register,
    Safecall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterMode {
    Value,
    Var,
    Out,
    Const,
    ConstRef,
}

#[derive(Debug, Clone)]
struct ResultTypeAnnotation {
    name: String,
    type_ref: TypeRef,
    offset: usize,
    scope: usize,
}

impl Symbol {
    fn result_type_annotation(&self) -> Option<ResultTypeAnnotation> {
        self.result_type_name
            .as_ref()
            .map(|name| ResultTypeAnnotation {
                name: name.clone(),
                type_ref: self.result_type_ref.clone().unwrap_or_else(|| TypeRef {
                    path: name
                        .split('.')
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    args: Vec::new(),
                    span: self.result_type_span.unwrap_or(self.span),
                }),
                offset: self
                    .result_type_span
                    .map_or(self.span.start, |span| span.start),
                scope: self.scope,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Candidate {
    uri: Url,
    index: usize,
}

#[derive(Debug, Clone)]
struct ParentType {
    path: Vec<String>,
    type_ref: Option<TypeRef>,
    relation: ParentRelation,
    span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentRelation {
    Superclass,
    ImplementedInterface,
    InterfaceParent,
}

#[derive(Debug, Clone)]
struct TypeAncestry {
    kind: TypeKind,
    name_span: Span,
    parent_declared: bool,
    abstract_: bool,
    parents: Vec<ParentType>,
}

#[derive(Debug, Clone)]
struct MethodResolution {
    class_owner: String,
    interface_owner: String,
    interface_method: String,
    implementation_method: String,
}

#[derive(Debug, Clone)]
struct InterfaceDelegation {
    class_owner: String,
    interface: TypeRef,
    declaring_uri: Option<Url>,
    declaring_substitution: Option<GenericSubstitution>,
}

#[derive(Debug, Clone)]
struct ContractTypeResolution {
    status: AncestryStatus,
    superclass: Option<TypeInstance>,
    interfaces: Vec<TypeInstance>,
}

impl ContractTypeResolution {
    fn complete() -> Self {
        Self {
            status: AncestryStatus::Complete,
            superclass: None,
            interfaces: Vec::new(),
        }
    }

    fn unknown() -> Self {
        Self {
            status: AncestryStatus::Unknown,
            superclass: None,
            interfaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum ContractParentResolution {
    Resolved(Option<TypeInstance>),
    Unknown,
}

#[derive(Debug, Clone)]
struct ContractRoutineCandidate {
    candidate: Candidate,
    substitution: GenericSubstitution,
    symbol_key: String,
}

impl ContractRoutineCandidate {
    fn conditional_unknown(&self, index: &NavigationIndex) -> bool {
        index.candidate_is_conditionally_unknown(&self.candidate)
    }
}

#[derive(Debug, Clone)]
struct ContractRequirement {
    candidate: Candidate,
    substitution: GenericSubstitution,
    interface: TypeInstance,
}

impl ContractRequirement {
    fn identity(
        &self,
        index: &NavigationIndex,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<ContractRequirementIdentity>, String> {
        let Some(symbol) = index.symbol(&self.candidate) else {
            return Err("interface method disappeared".to_owned());
        };
        budget.require_bytes(
            symbol.key.len().saturating_add(
                symbol
                    .routine_signature
                    .as_deref()
                    .unwrap_or_default()
                    .len(),
            ),
            cancel,
        )?;
        let mut parameter_types = Vec::with_capacity(symbol.routine_parameters.len());
        let mut parameter_fingerprints = Vec::with_capacity(symbol.routine_parameters.len());
        for parameter in &symbol.routine_parameters {
            let Some(type_ref) = parameter.type_ref.as_ref() else {
                return Ok(None);
            };
            let Some(type_identity) = index.contract_type_identity_with_budget(
                &self.candidate.uri,
                type_ref,
                &self.substitution,
                cancel,
                budget,
            )?
            else {
                return Ok(None);
            };
            let Some(type_fingerprint) = index.contract_type_fingerprint_with_budget(
                &self.candidate.uri,
                type_ref,
                &self.substitution,
                cancel,
                budget,
            )?
            else {
                // A missing structural identity is not a reusable identity.
                // Treat it as unsupported generation rather than allowing two
                // independently unknown snapshots to compare equal.
                return Ok(None);
            };
            parameter_types.push(type_identity);
            parameter_fingerprints.push(type_fingerprint);
        }
        let (result_type, result_fingerprint) =
            if let Some(type_ref) = symbol.result_type_ref.as_ref() {
                let Some(result_type) = index.contract_type_identity_with_budget(
                    &self.candidate.uri,
                    type_ref,
                    &self.substitution,
                    cancel,
                    budget,
                )?
                else {
                    return Ok(None);
                };
                let Some(result_fingerprint) = index.contract_type_fingerprint_with_budget(
                    &self.candidate.uri,
                    type_ref,
                    &self.substitution,
                    cancel,
                    budget,
                )?
                else {
                    return Ok(None);
                };
                (Some(result_type), Some(result_fingerprint))
            } else {
                (None, None)
            };
        let header = symbol
            .routine_header_span
            .and_then(|_span| index.documents.get(&self.candidate.uri))
            .and_then(|document| {
                symbol
                    .routine_header_span
                    .and_then(|span| document.source.get(span.start..span.end))
            })
            .map(str::to_owned);
        budget.require_bytes(
            header.as_deref().map_or(0, str::len)
                + parameter_types
                    .len()
                    .saturating_mul(std::mem::size_of::<TypeIdentity>())
                + result_type
                    .as_ref()
                    .map_or(0, |_| std::mem::size_of::<TypeIdentity>()),
            cancel,
        )?;
        budget.require_bytes(
            parameter_fingerprints
                .len()
                .saturating_mul(std::mem::size_of::<ContractTypeFingerprint>())
                + result_fingerprint
                    .as_ref()
                    .map_or(0, |_| std::mem::size_of::<ContractTypeFingerprint>()),
            cancel,
        )?;
        Ok(Some(ContractRequirementIdentity {
            name: symbol.key.clone(),
            signature: symbol.routine_signature.clone(),
            result: symbol.result_type_name.clone(),
            routine_kind: symbol.routine_kind,
            generic_shape: generic_shape(&symbol.generic_parameters),
            substitution: self.substitution.clone(),
            parameter_types,
            result_type,
            parameter_fingerprints,
            result_fingerprint,
            header,
        }))
    }
}

#[derive(Debug, Clone)]
struct ContractInterfaceRequirements {
    status: AncestryStatus,
    requirements: Vec<ContractRequirement>,
}

impl ContractInterfaceRequirements {
    fn complete(requirements: Vec<ContractRequirement>) -> Self {
        Self {
            status: AncestryStatus::Complete,
            requirements,
        }
    }

    fn unknown() -> Self {
        Self {
            status: AncestryStatus::Unknown,
            requirements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContractRequirementIdentity {
    name: String,
    signature: Option<String>,
    result: Option<String>,
    routine_kind: RoutineKind,
    generic_shape: String,
    substitution: GenericSubstitution,
    parameter_types: Vec<TypeIdentity>,
    result_type: Option<TypeIdentity>,
    parameter_fingerprints: Vec<ContractTypeFingerprint>,
    result_fingerprint: Option<ContractTypeFingerprint>,
    header: Option<String>,
}

#[derive(Debug, Clone)]
struct ContractMissingInterfaceMethod {
    requirement: ContractRequirement,
    identity: ContractRequirementIdentity,
    method_name: String,
    implementation: ContractMatch,
    unknown_only_because_of_open_compiler_root: bool,
}

#[derive(Debug, Clone, Copy)]
enum DirectInterfaceMethodDeclaration {
    Missing,
    Present { index: usize },
    Ambiguous,
}

#[derive(Debug, Clone)]
struct InterfaceDeclarationInsertion {
    start: usize,
    end: usize,
    indent: String,
    owner_indent: String,
    add_public: bool,
}

#[derive(Debug, Default)]
struct ContractClassSurface {
    routines: Vec<ContractRoutineCandidate>,
    nonroutine_names: HashSet<String>,
    method_resolutions: Vec<MethodResolution>,
    delegations: Vec<InterfaceDelegation>,
    implicit_compiler_root_is_open: bool,
}

type ContractInterfaceIdentity = (Url, String, GenericSubstitution);
type ContractInterfaceCoverageKey = (ContractInterfaceIdentity, ContractInterfaceIdentity);

fn contract_interface_identity(instance: &TypeInstance) -> ContractInterfaceIdentity {
    (
        instance.uri.clone(),
        instance.key.clone(),
        instance.substitution.clone(),
    )
}

fn contract_substitution_is_complete(instance: &TypeInstance) -> bool {
    instance
        .parameter_names
        .iter()
        .all(|name| instance.substitution.get(name).is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContractMatch {
    Yes,
    No,
    Unknown,
}

#[derive(Debug, Default)]
struct ContractAncestryState {
    active_contract_documents: HashSet<Url>,
    contract_documents: HashMap<Url, bool>,
    active_types: HashSet<TypeInstance>,
    types: HashMap<TypeInstance, ContractTypeResolution>,
    active_superclasses: HashSet<TypeInstance>,
    superclasses: HashMap<TypeInstance, ContractParentResolution>,
    active_superclass_routines: HashSet<TypeInstance>,
    active_class_interfaces: HashSet<TypeInstance>,
    active_class_surfaces: HashSet<TypeInstance>,
    active_interfaces: HashSet<ContractInterfaceIdentity>,
    interface_requirements: HashMap<ContractInterfaceIdentity, ContractInterfaceRequirements>,
    interface_coverage: HashMap<ContractInterfaceCoverageKey, ContractMatch>,
    active_interface_coverage: HashSet<ContractInterfaceCoverageKey>,
}

impl ContractAncestryState {
    fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone)]
struct HelperDefinition {
    key: String,
    name_span: Span,
    declaration_span: Span,
    kind: TypeKind,
    generic_parameters: Vec<GenericParameter>,
    target: TypeRef,
    parent: Option<TypeRef>,
    region: Region,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperTargetMatch {
    No,
    Yes(usize),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeAncestorMatch {
    No,
    Distance(usize),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HelperSelection {
    None,
    Selected { uri: Url, index: usize },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerHelperLookup {
    None,
    Selected(TypeInstance),
    Unknown,
}

impl OwnerHelperLookup {
    fn selected(self) -> Option<TypeInstance> {
        match self {
            Self::Selected(target) => Some(target),
            Self::None | Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HelperRank {
    target_specificity: usize,
    local: bool,
    import_order: usize,
    declaration_order: usize,
}

impl HelperRank {
    fn with_target_distance(mut self, distance: usize) -> Self {
        self.target_specificity = usize::MAX.saturating_sub(distance);
        self
    }

    fn with_unknown_target(mut self) -> Self {
        self.target_specificity = usize::MAX;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AncestryStatus {
    Complete,
    Unknown,
}

#[derive(Debug, Clone)]
struct TypeAncestryResolution {
    status: AncestryStatus,
    parents: Vec<(Url, String)>,
}

fn ancestry_resolution_owned_bytes(resolution: &TypeAncestryResolution) -> usize {
    std::mem::size_of::<TypeAncestryResolution>()
        .saturating_add(
            resolution
                .parents
                .len()
                .saturating_mul(std::mem::size_of::<(Url, String)>()),
        )
        .saturating_add(
            resolution
                .parents
                .iter()
                .map(|(uri, key)| uri.as_str().len().saturating_add(key.len()))
                .sum::<usize>(),
        )
}

fn type_ancestry_contains(
    index: &NavigationIndex,
    parents: &[(Url, String)],
    target: &(Url, String),
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    ancestry: &mut AncestryResolutionState,
) -> Result<bool, String> {
    for (parent_uri, parent_key) in parents {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if parent_uri == &target.0 && parent_key == &target.1 {
            return Ok(true);
        }
        let resolution = index
            .resolve_type_ancestry_with_budget(parent_uri, parent_key, ancestry, cancel, budget)?;
        if resolution.status == AncestryStatus::Complete
            && type_ancestry_contains(index, &resolution.parents, target, cancel, budget, ancestry)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize_fingerprint_header_with_budget(
    source: &str,
    binding_names: &HashSet<String>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<String, String> {
    let output_capacity = source.len().saturating_mul(9);
    budget.require_work(1, cancel)?;
    budget.require_owned_bytes(
        output_capacity.saturating_add(std::mem::size_of::<String>()),
        cancel,
    )?;
    let mut normalized = String::with_capacity(output_capacity);
    let mut cursor = 0;
    let bytes = source.as_bytes();
    while cursor < bytes.len() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let is_identifier_start = bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_';
        if !is_identifier_start {
            let next = cursor + source[cursor..].chars().next().unwrap().len_utf8();
            budget.require_bytes(next.saturating_sub(cursor), cancel)?;
            normalized.push_str(&source[cursor..next]);
            cursor = next;
            continue;
        }
        let start = cursor;
        budget.require_bytes(1, cancel)?;
        cursor += 1;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            check_navigation_cancel(cancel)?;
            budget.require_bytes(1, cancel)?;
            cursor += 1;
        }
        let token = &source[start..cursor];
        budget.require_owned_bytes(
            token.len().saturating_add(std::mem::size_of::<String>()),
            cancel,
        )?;
        let canonical = canonical_name(token);
        if binding_names.contains(&canonical) {
            normalized.push_str("<renamed>");
        } else {
            normalized.push_str(token);
        }
    }
    Ok(normalized)
}

impl NavigationIndex {
    fn source_symbol_fingerprint_with_budget(
        &self,
        uri: &Url,
        symbol: &Symbol,
        binding_names: &HashSet<String>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<String, String> {
        let header = symbol
            .routine_header_span
            .and_then(|span| self.documents.get(uri)?.source.get(span.start..span.end))
            .map(|source| {
                normalize_fingerprint_header_with_budget(source, binding_names, cancel, budget)
            })
            .transpose()?
            .unwrap_or_default();
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(
            std::mem::size_of::<String>()
                .saturating_add(uri.as_str().len())
                .saturating_add(header.len())
                .saturating_add(symbol.owner_type.as_ref().map_or(0, String::len))
                .saturating_add(128),
            cancel,
        )?;
        Ok(format!(
            "{uri}|owner={:?}|origin={:?}|kind={:?}|header={header}",
            symbol.owner_type, symbol.origin, symbol.kind
        ))
    }
}

#[derive(Debug, Clone)]
struct MemberLookup {
    candidates: Vec<Candidate>,
    ancestry_known: bool,
    implicit_member_known: bool,
    ambiguous_names: HashSet<String>,
}

impl MemberLookup {
    fn known(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            ancestry_known: true,
            implicit_member_known: false,
            ambiguous_names: HashSet::new(),
        }
    }

    fn unknown(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            ancestry_known: false,
            implicit_member_known: false,
            ambiguous_names: HashSet::new(),
        }
    }

    fn ambiguous(candidates: Vec<Candidate>, ambiguous_names: HashSet<String>) -> Self {
        Self {
            candidates,
            ancestry_known: true,
            implicit_member_known: false,
            ambiguous_names,
        }
    }

    fn with_ancestry_known(mut self, ancestry_known: bool) -> Self {
        self.ancestry_known &= ancestry_known;
        self
    }

    fn with_implicit_member_known(mut self, implicit_member_known: bool) -> Self {
        self.implicit_member_known |= implicit_member_known;
        self
    }
}

struct AncestryResolutionState {
    remaining_work: usize,
    active_types: HashSet<(Url, String)>,
    resolved_types: HashMap<(Url, String), TypeAncestryResolution>,
    active_members: HashSet<(Url, String, usize, Option<String>, bool)>,
    resolved_members: HashMap<(Url, String, usize, Option<String>, bool), MemberLookup>,
    active_helpers: HashSet<(Url, usize, Option<String>, bool)>,
}

impl AncestryResolutionState {
    fn new() -> Self {
        Self {
            remaining_work: MAX_ANCESTRY_WORK,
            active_types: HashSet::new(),
            resolved_types: HashMap::new(),
            active_members: HashSet::new(),
            resolved_members: HashMap::new(),
            active_helpers: HashSet::new(),
        }
    }

    fn take_work(&mut self) -> bool {
        let available = self.remaining_work > 0;
        if available {
            self.remaining_work -= 1;
        }
        available
    }
}

#[derive(Debug, Clone)]
enum Receiver {
    Unit(Url),
    Type(TypeInstance),
    Builtin(BuiltinType),
    IntegerLiteral(i128),
}

fn type_instance_from_symbol(candidate: &Candidate, symbol: &Symbol) -> TypeInstance {
    TypeInstance {
        uri: candidate.uri.clone(),
        key: symbol.key.clone(),
        kind: symbol.type_kind,
        scope: symbol.scope,
        parameter_names: symbol
            .generic_parameters
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect(),
        substitution: GenericSubstitution::empty(),
        helper_owner: None,
    }
}

fn symbolic_generic_substitution(
    uri: &Url,
    parameters: &[GenericParameter],
    scope: usize,
) -> GenericSubstitution {
    let mut substitution = GenericSubstitution::empty();
    for parameter in parameters {
        substitution.insert(
            &parameter.name,
            ResolvedType::Named(TypeInstance {
                uri: uri.clone(),
                key: parameter.name.clone(),
                kind: TypeKind::Other,
                scope,
                parameter_names: Vec::new(),
                substitution: GenericSubstitution::empty(),
                helper_owner: None,
            }),
        );
    }
    substitution
}

fn type_identity_from_resolved_type(resolved: &ResolvedType) -> Option<TypeIdentity> {
    match resolved {
        ResolvedType::Builtin(builtin) => Some(TypeIdentity::Builtin(*builtin)),
        ResolvedType::IntegerLiteral(value) => Some(TypeIdentity::IntegerLiteral(*value)),
        ResolvedType::Named(instance) => Some(TypeIdentity::Named {
            uri: instance.uri.clone(),
            key: instance.key.clone(),
            kind: instance.kind,
            args: instance
                .parameter_names
                .iter()
                .map(|name| instance.substitution.get(name))
                .map(|value| value.and_then(type_identity_from_resolved_type))
                .collect::<Option<Vec<_>>>()?,
        }),
    }
}

fn resolved_type_from_receivers(receivers: Vec<Receiver>) -> Option<ResolvedType> {
    let mut result = None;
    for receiver in receivers {
        let resolved = match receiver {
            Receiver::Builtin(builtin) => ResolvedType::Builtin(builtin),
            Receiver::IntegerLiteral(value) => ResolvedType::IntegerLiteral(value),
            Receiver::Type(instance) => {
                let resolved = ResolvedType::Named(instance);
                type_identity_from_resolved_type(&resolved)?;
                resolved
            }
            Receiver::Unit(_) => return None,
        };
        if result.as_ref().is_some_and(|current| current != &resolved) {
            return None;
        }
        result = Some(resolved);
    }
    result
}

fn substitution_from_receivers(receivers: Vec<Receiver>) -> Option<GenericSubstitution> {
    let mut result = None;
    for receiver in receivers {
        let Receiver::Type(instance) = receiver else {
            return None;
        };
        if result
            .as_ref()
            .is_some_and(|current: &GenericSubstitution| current != &instance.substitution)
        {
            return None;
        }
        result = Some(instance.substitution);
    }
    result
}

fn unique_type_instance(receivers: Vec<Receiver>) -> Option<TypeInstance> {
    let mut result = None;
    for receiver in receivers {
        let Receiver::Type(instance) = receiver else {
            return None;
        };
        if result
            .as_ref()
            .is_some_and(|current: &TypeInstance| current != &instance)
        {
            return None;
        }
        result = Some(instance);
    }
    result
}

const MAX_RECEIVER_WORK: usize = 256;
const MAX_TYPE_RESOLUTION_WORK: usize = 256;
const MAX_RECEIVER_RECURSION_DEPTH: usize = 64;
const MAX_WITH_CONTEXT_RECURSION_DEPTH: usize = 64;
const MAX_TYPE_REF_RECURSION_DEPTH: usize = 64;
const MAX_ANCESTRY_WORK: usize = 256;
const MAX_NAVIGATION_OVERLOAD_WORK: usize = 100_000;
const MAX_NAVIGATION_OVERLOAD_BYTES: usize = 8 * 1024 * 1024;

struct ResolutionState {
    receiver_work: usize,
    type_work: usize,
    active_members: HashSet<(Url, String, String, usize, GenericSubstitution)>,
    active_generic_constraints: HashSet<(Url, String)>,
    receiver_uncertain: bool,
    member_lookup_incomplete: bool,
    implicit_system_namespace_incomplete: bool,
    inaccessible_candidate: bool,
    ambiguous: bool,
    with_receivers: Vec<WithReceiverSlot>,
    with_context_depth: usize,
    with_context_resolutions: HashMap<(Url, Span), Option<Vec<WithReceiverSlot>>>,
    with_member_substitutions: HashMap<(Url, usize, Candidate), Vec<GenericSubstitution>>,
    suppress_with_lookup: bool,
}

#[derive(Clone, Copy)]
struct ReceiverLookupScope {
    implicit_system_namespace_incomplete: bool,
}

#[derive(Clone, Copy)]
enum UnitExportDomain {
    Closed,
    OpenImplicitSystem,
    Unknown,
}

impl ResolutionState {
    fn new() -> Self {
        Self {
            receiver_work: MAX_RECEIVER_WORK,
            type_work: MAX_TYPE_RESOLUTION_WORK,
            active_members: HashSet::new(),
            active_generic_constraints: HashSet::new(),
            receiver_uncertain: false,
            member_lookup_incomplete: false,
            implicit_system_namespace_incomplete: false,
            inaccessible_candidate: false,
            ambiguous: false,
            with_receivers: Vec::new(),
            with_context_depth: 0,
            with_context_resolutions: HashMap::new(),
            with_member_substitutions: HashMap::new(),
            suppress_with_lookup: false,
        }
    }

    fn mark_receiver_uncertain(&mut self) {
        self.receiver_uncertain = true;
    }

    fn mark_member_lookup_incomplete(&mut self) {
        self.member_lookup_incomplete = true;
    }

    fn mark_implicit_system_namespace_incomplete(&mut self) {
        self.implicit_system_namespace_incomplete = true;
    }

    fn begin_receiver_lookup_scope(&self) -> ReceiverLookupScope {
        ReceiverLookupScope {
            implicit_system_namespace_incomplete: self.implicit_system_namespace_incomplete,
        }
    }

    fn complete_receiver_lookup_as_closed_unit(&mut self, scope: ReceiverLookupScope) {
        // A failed unqualified probe may inspect the open implicit System
        // namespace.  That uncertainty belongs to the probe, not to a
        // subsequently proven ordinary unit receiver with a closed export
        // domain.  A source-backed System is deliberately not closed by its
        // unit identity alone.  Other uncertainty flags are deliberately
        // retained, so unknown with/receiver/helper state cannot be cleared
        // by a unit-shaped fallback.
        self.implicit_system_namespace_incomplete = scope.implicit_system_namespace_incomplete;
    }

    fn mark_inaccessible_candidate(&mut self) {
        self.inaccessible_candidate = true;
    }

    fn mark_ambiguous(&mut self) {
        self.ambiguous = true;
    }

    fn receiver_resolution_uncertain(&self) -> bool {
        self.receiver_uncertain
    }

    fn take_receiver_work(&mut self) -> bool {
        let available = self.receiver_work > 0;
        if available {
            self.receiver_work -= 1;
        }
        available
    }

    fn take_type_work(&mut self, amount: usize) -> bool {
        if amount > self.type_work {
            return false;
        }
        self.type_work -= amount;
        true
    }
}

#[derive(Debug)]
pub(crate) struct ParsedDocument {
    source: Arc<str>,
    parser_source: Arc<[u8]>,
    conditional_context: ConditionalContext,
    tree: Tree,
    parser_recovery_spans: Vec<Span>,
    unit_name: String,
    unit_display_name: String,
    interface_range: Option<Span>,
    implementation_range: Option<Span>,
    uses_clauses: Vec<UsesClauseMetadata>,
    interface_uses: Vec<String>,
    implementation_uses: Vec<String>,
    imports: Vec<ImportMetadata>,
    unknown_imports: HashSet<String>,
    interface_routine_keys: HashSet<String>,
    scopes: Vec<Scope>,
    scope_intervals: SourceIntervalIndex,
    with_contexts: Vec<WithContext>,
    cache_unsafe_scopes: HashSet<usize>,
    owner_type_contexts: Vec<OwnerTypeContext>,
    owner_type_intervals: SourceIntervalIndex,
    symbols: Vec<Symbol>,
    opaque_ranges: Vec<Span>,
    conditionals: ConditionalAnalysis,
    conditional_unknown_symbols: Vec<bool>,
    has_initialization: bool,
    has_finalization: bool,
    unknown_class_owners: HashSet<String>,
    known_non_class_owners: HashSet<String>,
    symbol_indices_by_scope_key: HashMap<(usize, String), Vec<usize>>,
    scope_symbol_indices: HashMap<usize, Vec<usize>>,
    member_symbol_indices: HashMap<(String, String), Vec<usize>>,
    member_binding_keys_by_owner: HashMap<String, HashSet<String>>,
    scope_binding_keys: Vec<HashSet<String>>,
    member_symbol_indices_by_owner: HashMap<String, Vec<usize>>,
    type_ancestry: HashMap<String, Vec<TypeAncestry>>,
    type_symbol_indices: HashMap<String, Vec<usize>>,
    direct_symbol_indices: HashMap<Span, Vec<usize>>,
    routine_symbol_indices: HashMap<String, Vec<usize>>,
    routine_symbol_indices_by_body_scope: HashMap<usize, Vec<usize>>,
    exported_symbol_indices: Vec<usize>,
    interface_member_routine_keys: HashMap<String, HashSet<String>>,
    method_resolutions: Vec<MethodResolution>,
    interface_delegations: Vec<InterfaceDelegation>,
    generic_parameter_contexts: Vec<GenericParameterContext>,
    generic_parameter_intervals: SourceIntervalIndex,
    helpers: Vec<HelperDefinition>,
    documentation: Vec<Option<Arc<documentation::Documentation>>>,
}

impl ParsedDocument {
    pub(crate) fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        visit(self.source.len())?;
        visit(self.parser_source.len())?;
        visit(self.unit_name.len())?;
        visit(self.unit_display_name.len())?;

        // Tree-sitter owns a native syntax tree in addition to the Rust-side
        // projections below. Walk it without allocating a traversal stack.
        let mut cursor = self.tree.walk();
        'tree: loop {
            visit(0)?;
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'tree;
                }
            }
        }

        for _ in &self.parser_recovery_spans {
            visit(0)?;
        }
        for clause in &self.uses_clauses {
            let _ = clause;
            visit(0)?;
        }
        for name in self.interface_uses.iter().chain(&self.implementation_uses) {
            visit(name.len())?;
        }
        for import in &self.imports {
            visit(import.name.len())?;
        }
        for name in self
            .unknown_imports
            .iter()
            .chain(&self.interface_routine_keys)
            .chain(&self.unknown_class_owners)
            .chain(&self.known_non_class_owners)
        {
            visit(name.len())?;
        }
        visit(self.conditionals.projected_source.len())?;
        for _ in &self.conditionals.inactive_spans {
            visit(0)?;
        }
        for _ in &self.conditionals.unknown_spans {
            visit(0)?;
        }
        for directive in &self.conditionals.directives {
            visit(directive.body.len())?;
        }
        self.conditional_context.visit_recovery_payload(visit)?;
        for scope in &self.scopes {
            visit(scope.owner_type.as_ref().map_or(0, String::len))?;
        }
        for interval in &self.scope_intervals.ranges {
            let _ = interval;
            visit(0)?;
        }
        for context in &self.with_contexts {
            visit(0)?;
            for _ in &context.receiver_spans {
                visit(0)?;
            }
        }
        for _ in &self.cache_unsafe_scopes {
            visit(0)?;
        }
        for context in &self.owner_type_contexts {
            visit(context.owner_type.len())?;
        }
        for interval in &self.owner_type_intervals.ranges {
            let _ = interval;
            visit(0)?;
        }
        for symbol in &self.symbols {
            visit(symbol.name.len())?;
            visit(symbol.key.len())?;
            for value in [
                symbol.owner_type.as_deref(),
                symbol.owner_type_name.as_deref(),
                symbol.generic_parameter.as_deref(),
                symbol.type_name.as_deref(),
                symbol.result_type_name.as_deref(),
                symbol.routine_key.as_deref(),
                symbol.routine_signature.as_deref(),
                symbol.accessor.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                visit(value.len())?;
            }
            for parameter in &symbol.generic_parameters {
                visit(parameter.name.len())?;
                if let Some(constraint) = &parameter.constraint {
                    visit_type_ref_recovery(constraint, visit)?;
                }
            }
            for parameter in &symbol.routine_parameters {
                visit(parameter.type_name.as_ref().map_or(0, String::len))?;
                if let Some(type_ref) = &parameter.type_ref {
                    visit_type_ref_recovery(type_ref, visit)?;
                }
                if let Some(type_shape) = &parameter.type_shape {
                    visit_type_shape_recovery(type_shape, visit)?;
                }
            }
            if let Some(type_ref) = &symbol.type_ref {
                visit_type_ref_recovery(type_ref, visit)?;
            }
            if let Some(type_shape) = &symbol.type_shape {
                visit_type_shape_recovery(type_shape, visit)?;
            }
            if let Some(type_ref) = &symbol.result_type_ref {
                visit_type_ref_recovery(type_ref, visit)?;
            }
        }
        for _ in &self.opaque_ranges {
            visit(0)?;
        }
        for _ in &self.conditional_unknown_symbols {
            visit(0)?;
        }
        for (key, indices) in &self.symbol_indices_by_scope_key {
            visit(key.1.len())?;
            for _ in indices {
                visit(0)?;
            }
        }
        for indices in self.scope_symbol_indices.values() {
            for _ in indices {
                visit(0)?;
            }
        }
        for ((owner, member), indices) in &self.member_symbol_indices {
            visit(owner.len())?;
            visit(member.len())?;
            for _ in indices {
                visit(0)?;
            }
        }
        for (owner, keys) in &self.member_binding_keys_by_owner {
            visit(owner.len())?;
            for key in keys {
                visit(key.len())?;
            }
        }
        for keys in &self.scope_binding_keys {
            for key in keys {
                visit(key.len())?;
            }
        }
        for (owner, indices) in &self.member_symbol_indices_by_owner {
            visit(owner.len())?;
            for _ in indices {
                visit(0)?;
            }
        }
        for (name, ancestries) in &self.type_ancestry {
            visit(name.len())?;
            for ancestry in ancestries {
                visit(0)?;
                for parent in &ancestry.parents {
                    for component in &parent.path {
                        visit(component.len())?;
                    }
                    if let Some(type_ref) = &parent.type_ref {
                        visit_type_ref_recovery(type_ref, visit)?;
                    }
                }
            }
        }
        for (key, indices) in &self.type_symbol_indices {
            visit(key.len())?;
            for _ in indices {
                visit(0)?;
            }
        }
        for _ in self.direct_symbol_indices.values().flatten() {
            visit(0)?;
        }
        for (key, indices) in &self.routine_symbol_indices {
            visit(key.len())?;
            for _ in indices {
                visit(0)?;
            }
        }
        for indices in self.routine_symbol_indices_by_body_scope.values() {
            visit(0)?;
            for _ in indices {
                visit(0)?;
            }
        }
        for _ in &self.exported_symbol_indices {
            visit(0)?;
        }
        for (owner, methods) in &self.interface_member_routine_keys {
            visit(owner.len())?;
            for method in methods {
                visit(method.len())?;
            }
        }
        for resolution in &self.method_resolutions {
            visit(resolution.class_owner.len())?;
            visit(resolution.interface_owner.len())?;
            visit(resolution.interface_method.len())?;
            visit(resolution.implementation_method.len())?;
        }
        for delegation in &self.interface_delegations {
            visit_type_ref_recovery(&delegation.interface, visit)?;
            if let Some(uri) = &delegation.declaring_uri {
                visit(uri.as_str().len())?;
            }
            if let Some(substitution) = &delegation.declaring_substitution {
                visit_generic_substitution_recovery(substitution, visit, 0)?;
            }
        }
        for context in &self.generic_parameter_contexts {
            visit(0)?;
            for name in &context.names {
                visit(name.len())?;
            }
        }
        for _ in &self.generic_parameter_intervals.ranges {
            visit(0)?;
        }
        for helper in &self.helpers {
            visit(0)?;
            visit(helper.key.len())?;
            visit_type_ref_recovery(&helper.target, visit)?;
            if let Some(parent) = &helper.parent {
                visit_type_ref_recovery(parent, visit)?;
            }
            for parameter in &helper.generic_parameters {
                visit(parameter.name.len())?;
                if let Some(constraint) = &parameter.constraint {
                    visit_type_ref_recovery(constraint, visit)?;
                }
            }
        }
        for documentation in &self.documentation {
            visit(0)?;
            if let Some(documentation) = documentation {
                documentation.visit_recovery_payload(visit)?;
            }
        }
        Ok(())
    }
}

fn visit_type_ref_recovery(
    reference: &TypeRef,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    visit_type_ref_recovery_depth(reference, visit, 0)
}

fn visit_type_ref_recovery_depth(
    reference: &TypeRef,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
    depth: usize,
) -> Result<(), String> {
    if depth > 256 {
        return Err("nested parsed type exceeds the recovery inspection depth".into());
    }
    for component in &reference.path {
        visit(component.len())?;
    }
    for argument in &reference.args {
        visit(0)?;
        visit_type_ref_recovery_depth(argument, visit, depth + 1)?;
    }
    Ok(())
}

fn visit_type_shape_recovery(
    shape: &TypeShape,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    visit_type_shape_recovery_depth(shape, visit, 0)
}

fn visit_type_shape_recovery_depth(
    shape: &TypeShape,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
    depth: usize,
) -> Result<(), String> {
    if depth > 256 {
        return Err("nested parsed type-shape exceeds the recovery inspection depth".into());
    }
    visit(0)?;
    match shape {
        TypeShape::Named(reference) => visit_type_ref_recovery(reference, visit),
        TypeShape::Pointer(inner) | TypeShape::Array { element: inner, .. } => {
            visit_type_shape_recovery_depth(inner, visit, depth + 1)
        }
        TypeShape::Callable | TypeShape::Unknown => Ok(()),
    }
}

fn visit_generic_substitution_recovery(
    substitution: &GenericSubstitution,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
    depth: usize,
) -> Result<(), String> {
    if depth > 256 {
        return Err("nested generic substitution exceeds the recovery inspection depth".into());
    }
    for (name, value) in &substitution.0 {
        visit(name.len())?;
        match value {
            ResolvedType::Builtin(_) | ResolvedType::IntegerLiteral(_) => visit(0)?,
            ResolvedType::Named(instance) => {
                visit(instance.uri.as_str().len())?;
                visit(instance.key.len())?;
                for name in &instance.parameter_names {
                    visit(name.len())?;
                }
                if let Some(owner) = &instance.helper_owner {
                    visit(owner.uri.as_str().len())?;
                    visit(0)?;
                }
                visit_generic_substitution_recovery(&instance.substitution, visit, depth + 1)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Document {
    parsed: Arc<ParsedDocument>,
    import_bindings: Option<HashMap<String, Url>>,
    import_binding_fingerprint: Option<u64>,
}

impl Deref for Document {
    type Target = ParsedDocument;

    fn deref(&self) -> &Self::Target {
        &self.parsed
    }
}

impl Document {
    fn parse_with_cancel(
        uri: Url,
        source: String,
        context: &ConditionalContext,
        cancel: &AtomicBool,
        previous: Option<Arc<ParsedDocument>>,
    ) -> Result<Self, String> {
        if let Some(previous) = previous.as_ref() {
            if previous.source.as_ref() == source.as_str()
                && previous.conditional_context == *context
            {
                if cancel.load(Ordering::Relaxed) {
                    return Err("request cancelled".to_string());
                }
                return Ok(Self {
                    parsed: previous.clone(),
                    import_bindings: None,
                    import_binding_fingerprint: None,
                });
            }
        }
        #[cfg(test)]
        TEST_DOCUMENT_PARSE_CALLS.with(|value| value.set(value.get().saturating_add(1)));
        let path = uri
            .to_file_path()
            .unwrap_or_else(|_| PathBuf::from(uri.path()));
        let info = FileInfo::new(path);
        let conditionals = conditional::analyze_with_context_and_cancel(&source, context, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        let (tree, _diagnostics, patches, parser_source) = if let Some(previous) = previous.as_ref()
        {
            let parsed = parser::parse_file_incremental(
                &info,
                conditionals.projected_source.as_bytes(),
                previous.parser_source.as_ref(),
                &previous.tree,
            )?;
            (
                parsed.tree,
                parsed.diagnostics,
                parsed.patches,
                parsed.parser_source,
            )
        } else {
            let parsed = parser::parse_file_with_parser_source(
                &info,
                conditionals.projected_source.as_bytes(),
            )?;
            (
                parsed.tree,
                parsed.diagnostics,
                parsed.patches,
                parsed.parser_source,
            )
        };
        let root = tree.root_node();
        let generic_parameter_contexts = semantic_tokens::generic_parameter_contexts(root, &source);
        let generic_parameter_intervals = SourceIntervalIndex::from_intervals(
            generic_parameter_contexts
                .iter()
                .enumerate()
                .map(|(index, context)| (context.span, index)),
        );
        let owner_type_contexts = semantic_tokens::owner_type_contexts(root, &source);
        let owner_type_intervals = SourceIntervalIndex::from_intervals(
            owner_type_contexts
                .iter()
                .enumerate()
                .map(|(index, context)| (context.span, index)),
        );
        let parser_recovery_spans = collect_parser_recovery_spans(root);
        let opaque_ranges = patches
            .into_iter()
            .filter_map(|patch| match patch {
                DirectivePatch::OpaqueBlock(block) => Some(Span {
                    start: block.start,
                    end: block.end,
                }),
                DirectivePatch::Markers(_) => None,
            })
            .collect();

        let mut module_names = Vec::new();
        let mut sections = Vec::new();
        let mut uses_clauses = Vec::new();
        let mut has_initialization = false;
        let mut has_finalization = false;
        collect_nodes(root, &mut |node| {
            if node.kind() == "moduleName" {
                module_names.push(node);
            } else if node.kind() == "interface" {
                sections.push((Region::Interface, Span::from_node(node)));
            } else if node.kind() == "implementation" {
                sections.push((Region::Implementation, Span::from_node(node)));
            } else if node.kind() == "declUses" {
                uses_clauses.push(UsesClauseMetadata {
                    span: SourceSpan {
                        start: node.start_byte(),
                        end: node.end_byte(),
                    },
                    interface: region_for_node(node) == Region::Interface,
                });
            } else if node.kind() == "initialization" {
                has_initialization = true;
            } else if node.kind() == "finalization" {
                has_finalization = true;
            }
        });

        let unit_module = module_names
            .iter()
            .copied()
            .find(|node| !has_ancestor_kind(*node, "declUses"));
        let unit_name = unit_module
            .map(|node| canonical_path(&identifier_texts(node, &source)))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| fallback_unit_name(&uri));
        let unit_display_name = unit_module
            .map(|node| identifier_texts(node, &source).join("."))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| unit_name.clone());

        let interface_range = sections
            .iter()
            .find_map(|(region, span)| (*region == Region::Interface).then_some(*span));
        let implementation_range = sections
            .iter()
            .find_map(|(region, span)| (*region == Region::Implementation).then_some(*span));

        let mut interface_uses = Vec::new();
        let mut implementation_uses = Vec::new();
        let mut imports = Vec::new();
        for module_name in &module_names {
            if !has_ancestor_kind(*module_name, "declUses") {
                continue;
            }
            let name = canonical_path(&identifier_texts(*module_name, &source));
            if name.is_empty() || name.eq_ignore_ascii_case("in") {
                continue;
            }
            imports.push(ImportMetadata {
                name: name.clone(),
                span: SourceSpan {
                    start: module_name.start_byte(),
                    end: module_name.end_byte(),
                },
            });
            match region_for_node(*module_name) {
                Region::Interface => interface_uses.push(name),
                Region::Implementation => implementation_uses.push(name),
                Region::Other => implementation_uses.push(name),
            }
        }
        let mut seen_interface_uses = HashSet::new();
        interface_uses.retain(|name| seen_interface_uses.insert(name.clone()));
        let mut seen_implementation_uses = HashSet::new();
        implementation_uses.retain(|name| seen_implementation_uses.insert(name.clone()));
        let unknown_imports = imports
            .iter()
            .filter(|import| {
                conditionals
                    .unknown_spans
                    .iter()
                    .any(|span| span.start <= import.span.start && import.span.end <= span.end)
            })
            .map(|import| canonical_name(&import.name))
            .collect();

        let definitions = collect_nodes_matching(root, "defProc");
        let (scopes, scope_by_span) = build_scopes(source.len(), root, &definitions, &source);
        let scope_intervals =
            SourceIntervalIndex::from_intervals(scopes.iter().enumerate().map(|(index, scope)| {
                (
                    Span {
                        start: scope.start,
                        end: scope.end,
                    },
                    index,
                )
            }));
        let with_contexts = collect_with_contexts(root);
        let cache_unsafe_scopes = cache_unsafe_scopes(root, &scope_by_span);
        let mut symbols = Vec::new();

        if let Some(module_name) = unit_module {
            let span = Span::from_node(module_name);
            let selection_span = identifier_nodes(module_name)
                .last()
                .map(|node| Span::from_node(*node))
                .unwrap_or(span);
            symbols.push(Symbol {
                span,
                declaration_span: Span::from_node(root),
                selection_span,
                name: node_text(module_name, &source),
                key: unit_name.clone(),
                kind: SymbolKind::Unit,
                type_kind: TypeKind::Other,
                routine_kind: RoutineKind::Procedure,
                routine_directives: RoutineDirectives::default(),
                parameter_mode: None,
                scope: ROOT_SCOPE,
                owner_type: None,
                owner_type_name: None,
                visibility: Visibility::Public,
                declaration_ordered: false,
                generic_parameters: Vec::new(),
                generic_parameter: None,
                type_name: None,
                type_ref: None,
                type_shape: None,
                result_type_name: None,
                result_type_ref: None,
                result_type_span: None,
                region: Region::Other,
                origin: Origin::Declaration,
                is_static: false,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                routine_header_span: None,
                routine_parameter_spans: Vec::new(),
                routine_parameters: Vec::new(),
                type_excerpt_end: None,
                body_scope: None,
                unresolved_abbreviated: false,
                accessor: None,
            });
        }

        collect_symbols(root, &source, &scopes, &scope_by_span, &mut symbols);
        pair_omitted_generic_constraints(&mut symbols);
        pair_abbreviated_definitions(root, &source, &scope_by_span, &mut symbols);
        let documentation = documentation::collect(&source, &symbols, cancel)?;
        inherit_member_routine_visibility(&mut symbols);
        let conditional_unknown_symbols =
            conditional_unknown_symbols(root, &conditionals, &symbols);
        let helpers = collect_helpers(root, &source);
        let type_ancestry = collect_type_ancestry(root, &source);
        let method_resolutions = collect_method_resolutions(root, &source);
        let interface_delegations = collect_interface_delegations(root, &source);
        let unknown_class_owners = symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Type
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                    && symbol.type_kind == TypeKind::Class
            })
            .map(|symbol| symbol.key.clone())
            .collect();
        let known_non_class_owners = symbols
            .iter()
            .enumerate()
            .filter(|(index, symbol)| {
                symbol.kind == SymbolKind::Type
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                    && symbol.type_kind != TypeKind::Class
                    && !conditional_unknown_symbols[*index]
            })
            .map(|(_, symbol)| symbol.key.clone())
            .collect();
        let interface_routine_keys: HashSet<String> = symbols
            .iter()
            .filter_map(|symbol| {
                (symbol.kind == SymbolKind::Routine
                    && symbol.owner_type.is_none()
                    && !symbol.local_only
                    && symbol.origin == Origin::Declaration
                    && symbol.region == Region::Interface)
                    .then(|| symbol.routine_key.clone())
                    .flatten()
            })
            .collect();
        let exported_symbol_indices = symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                if symbol.owner_type.is_some()
                    || symbol.kind == SymbolKind::Unit
                    || symbol.local_only
                    || symbol.generic_parameter.is_some()
                {
                    return false;
                }
                symbol.region == Region::Interface
                    || (symbol.kind == SymbolKind::Routine
                        && symbol.origin == Origin::Definition
                        && symbol
                            .routine_key
                            .as_ref()
                            .is_some_and(|key| interface_routine_keys.contains(key)))
            })
            .map(|(index, _)| index)
            .collect();
        let mut symbol_indices_by_scope_key = HashMap::new();
        let mut scope_symbol_indices = HashMap::new();
        let mut member_symbol_indices = HashMap::new();
        let mut member_symbol_indices_by_owner = HashMap::new();
        let mut type_symbol_indices = HashMap::new();
        let mut direct_symbol_indices = HashMap::new();
        let mut routine_symbol_indices = HashMap::new();
        let mut routine_symbol_indices_by_body_scope = HashMap::new();
        let mut interface_member_routine_keys = HashMap::<String, HashSet<String>>::new();
        for (index, symbol) in symbols.iter().enumerate() {
            symbol_indices_by_scope_key
                .entry((symbol.scope, symbol.key.clone()))
                .or_insert_with(Vec::new)
                .push(index);
            scope_symbol_indices
                .entry(symbol.scope)
                .or_insert_with(Vec::new)
                .push(index);
            direct_symbol_indices
                .entry(symbol.span)
                .or_insert_with(Vec::new)
                .push(index);
            if let Some(owner_type) = &symbol.owner_type {
                member_symbol_indices
                    .entry((owner_type.clone(), symbol.key.clone()))
                    .or_insert_with(Vec::new)
                    .push(index);
                member_symbol_indices_by_owner
                    .entry(owner_type.clone())
                    .or_insert_with(Vec::new)
                    .push(index);
                if symbol.kind == SymbolKind::Routine
                    && symbol.origin == Origin::Declaration
                    && symbol.region == Region::Interface
                {
                    if let Some(routine_key) = &symbol.routine_key {
                        interface_member_routine_keys
                            .entry(owner_type.clone())
                            .or_default()
                            .insert(routine_key.clone());
                    }
                }
            }
            if symbol.kind == SymbolKind::Type
                && symbol.scope == ROOT_SCOPE
                && symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
            {
                type_symbol_indices
                    .entry(symbol.key.clone())
                    .or_insert_with(Vec::new)
                    .push(index);
            }
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    routine_symbol_indices
                        .entry(routine_key.clone())
                        .or_insert_with(Vec::new)
                        .push(index);
                }
                if let Some(body_scope) = symbol.body_scope {
                    routine_symbol_indices_by_body_scope
                        .entry(body_scope)
                        .or_insert_with(Vec::new)
                        .push(index);
                }
            }
        }
        let scope_binding_keys = build_scope_binding_keys(&scopes, &symbol_indices_by_scope_key);
        let member_binding_keys_by_owner = build_member_binding_keys(&member_symbol_indices);

        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }

        let parsed = Arc::new(ParsedDocument {
            source: Arc::from(source),
            parser_source: Arc::from(parser_source.into_boxed_slice()),
            conditional_context: context.clone(),
            tree,
            parser_recovery_spans,
            unit_name,
            unit_display_name,
            interface_range,
            implementation_range,
            uses_clauses,
            interface_uses,
            implementation_uses,
            imports,
            unknown_imports,
            interface_routine_keys,
            scopes,
            scope_intervals,
            with_contexts,
            cache_unsafe_scopes,
            owner_type_contexts,
            owner_type_intervals,
            symbols,
            opaque_ranges,
            conditionals,
            conditional_unknown_symbols,
            has_initialization,
            has_finalization,
            unknown_class_owners,
            known_non_class_owners,
            symbol_indices_by_scope_key,
            scope_symbol_indices,
            member_symbol_indices,
            member_binding_keys_by_owner,
            scope_binding_keys,
            member_symbol_indices_by_owner,
            type_ancestry,
            type_symbol_indices,
            direct_symbol_indices,
            routine_symbol_indices,
            routine_symbol_indices_by_body_scope,
            exported_symbol_indices,
            interface_member_routine_keys,
            method_resolutions,
            interface_delegations,
            generic_parameter_contexts,
            generic_parameter_intervals,
            helpers,
            documentation,
        });
        Ok(Self {
            parsed,
            import_bindings: None,
            import_binding_fingerprint: None,
        })
    }

    fn region_at(&self, offset: usize) -> Region {
        if self
            .interface_range
            .is_some_and(|range| range.contains_offset(offset))
        {
            Region::Interface
        } else if self
            .implementation_range
            .is_some_and(|range| range.contains_offset(offset))
        {
            Region::Implementation
        } else {
            Region::Other
        }
    }

    fn offset_is_in_helper_declaration(&self, offset: usize) -> bool {
        self.helpers
            .iter()
            .any(|helper| helper.declaration_span.contains_offset(offset))
    }

    fn active_uses(&self, region: Region) -> Vec<&String> {
        match region {
            Region::Interface => self.interface_uses.iter().collect(),
            Region::Implementation | Region::Other => self
                .interface_uses
                .iter()
                .chain(self.implementation_uses.iter())
                .collect(),
        }
    }

    fn active_uses_with_budget<'a>(
        &'a self,
        region: Region,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<&'a String>, String> {
        let count = match region {
            Region::Interface => self.interface_uses.len(),
            Region::Implementation | Region::Other => self
                .interface_uses
                .len()
                .saturating_add(self.implementation_uses.len()),
        };
        budget.require_work(count, cancel)?;
        budget.require_bytes(count.saturating_mul(std::mem::size_of::<&String>()), cancel)?;
        Ok(match region {
            Region::Interface => self.interface_uses.iter().collect(),
            Region::Implementation | Region::Other => self
                .interface_uses
                .iter()
                .chain(self.implementation_uses.iter())
                .collect(),
        })
    }

    fn scope_at(&self, offset: usize) -> usize {
        self.scope_intervals.at(offset).unwrap_or(ROOT_SCOPE)
    }

    fn scope_chain_from(&self, mut current: usize) -> Vec<usize> {
        let mut result = Vec::new();
        loop {
            result.push(current);
            let Some(parent) = self.scopes[current].parent else {
                break;
            };
            current = parent;
        }
        result
    }

    fn with_contexts_at(&self, offset: usize) -> Vec<&WithContext> {
        let mut contexts = self
            .with_contexts
            .iter()
            .filter(|context| context.body.contains_offset(offset))
            .collect::<Vec<_>>();
        contexts.sort_by_key(|context| {
            (
                context.body.end.saturating_sub(context.body.start),
                context.body.start,
            )
        });
        contexts
    }

    fn with_receiver_contexts_at(&self, offset: usize) -> Vec<(&WithContext, usize)> {
        let mut contexts = self
            .with_contexts
            .iter()
            .filter_map(|context| {
                context
                    .receiver_spans
                    .iter()
                    .position(|span| span.contains_offset(offset))
                    .map(|receiver_index| (context, receiver_index))
            })
            .collect::<Vec<_>>();
        contexts.sort_by_key(|(context, _)| {
            (
                context.body.end.saturating_sub(context.body.start),
                context.body.start,
            )
        });
        contexts
    }

    fn has_with_context_at(&self, offset: usize) -> bool {
        self.with_contexts.iter().any(|context| {
            context.body.contains_offset(offset)
                || context
                    .receiver_spans
                    .iter()
                    .any(|span| span.contains_offset(offset))
        })
    }

    fn result_type_annotation_for_body_scope(&self, scope: usize) -> Option<ResultTypeAnnotation> {
        let mut current = Some(scope);
        while let Some(scope) = current {
            if let Some(annotation) = self
                .routine_symbol_indices_by_body_scope
                .get(&scope)
                .into_iter()
                .flatten()
                .find_map(|index| self.symbols.get(*index)?.result_type_annotation())
            {
                return Some(annotation);
            }
            current = self.scopes[scope].parent;
        }
        None
    }

    fn owner_type_at(&self, offset: usize) -> Option<String> {
        let scope = self.scope_at(offset);
        self.owner_type_for_scope(scope)
            .or_else(|| self.owner_type_at_offset(offset))
    }

    fn owner_type_at_identifier(&self, identifier: Node<'_>, scope: usize) -> Option<String> {
        self.owner_type_for_scope(scope)
            .or_else(|| self.owner_type_at_offset(identifier.start_byte()))
    }

    fn owner_type_for_scope(&self, scope: usize) -> Option<String> {
        self.scopes.get(scope)?.owner_type.clone()
    }

    fn owner_type_for_scope_ref(&self, scope: usize) -> Option<&str> {
        self.scopes
            .get(scope)
            .and_then(|scope| scope.owner_type.as_deref())
    }

    fn owner_type_at_offset(&self, offset: usize) -> Option<String> {
        let context = self.owner_type_intervals.at(offset)?;
        self.owner_type_contexts
            .get(context)
            .map(|context| context.owner_type.clone())
    }

    fn scope_at_with_budget_or_unknown(
        &self,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<BudgetedLookup<usize>, String> {
        match self
            .scope_intervals
            .at_with_budget_or_unknown(offset, cancel, budget)?
        {
            BudgetedLookup::Value(Some(scope)) => Ok(BudgetedLookup::Value(scope)),
            BudgetedLookup::Value(None) => Ok(BudgetedLookup::Value(ROOT_SCOPE)),
            BudgetedLookup::Exhausted => Ok(BudgetedLookup::Exhausted),
        }
    }

    fn owner_type_at_identifier_with_budget(
        &self,
        identifier: Node<'_>,
        scope: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<String>, String> {
        if let Some(owner_type) = self.owner_type_for_scope_ref(scope) {
            budget.require_bytes(owner_type.len(), cancel)?;
            return Ok(Some(owner_type.to_owned()));
        }
        let Some(context_index) =
            self.owner_type_intervals
                .at_with_budget(identifier.start_byte(), cancel, budget)?
        else {
            return Ok(None);
        };
        let Some(owner_type) = self.owner_type_contexts.get(context_index) else {
            return Ok(None);
        };
        budget.require_bytes(owner_type.owner_type.len(), cancel)?;
        Ok(Some(owner_type.owner_type.clone()))
    }

    fn owner_type_at_identifier_with_budget_or_unknown(
        &self,
        identifier: Node<'_>,
        scope: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<BudgetedLookup<Option<&str>>, String> {
        if let Some(owner_type) = self.owner_type_for_scope_ref(scope) {
            if !budget.take_bytes(owner_type.len(), cancel)? {
                return Ok(BudgetedLookup::Exhausted);
            }
            return Ok(BudgetedLookup::Value(Some(owner_type)));
        }
        #[cfg(test)]
        TEST_SEMANTIC_OWNER_FALLBACKS.with(|value| {
            value.set(value.get().saturating_add(1));
        });
        let context = match self.owner_type_intervals.at_with_budget_or_unknown(
            identifier.start_byte(),
            cancel,
            budget,
        )? {
            BudgetedLookup::Value(context) => context,
            BudgetedLookup::Exhausted => return Ok(BudgetedLookup::Exhausted),
        };
        let Some(context) = context else {
            return Ok(BudgetedLookup::Value(None));
        };
        let Some(owner_type) = self.owner_type_contexts.get(context) else {
            return Ok(BudgetedLookup::Value(None));
        };
        if !budget.take_bytes(owner_type.owner_type.len(), cancel)? {
            return Ok(BudgetedLookup::Exhausted);
        }
        Ok(BudgetedLookup::Value(Some(owner_type.owner_type.as_str())))
    }

    fn has_parser_recovery_near(&self, span: Span) -> bool {
        self.parser_recovery_spans
            .iter()
            .any(|recovery| recovery.start <= span.end && recovery.end >= span.start)
    }
}

fn symbol_visible_in_region(symbol: &Symbol, region: Region) -> bool {
    match region {
        Region::Interface => symbol.region == Region::Interface,
        Region::Implementation => {
            symbol.region == Region::Interface || symbol.region == Region::Implementation
        }
        Region::Other => true,
    }
}

fn symbol_is_available_at(
    document: &Document,
    symbol: &Symbol,
    candidate_uri: &Url,
    current_uri: &Url,
    offset: usize,
) -> bool {
    if candidate_uri != current_uri || symbol.owner_type.is_some() || !symbol.declaration_ordered {
        return true;
    }
    if symbol.span.start <= offset {
        return true;
    }
    symbol.kind == SymbolKind::Routine
        && symbol.origin == Origin::Definition
        && symbol.routine_key.as_ref().is_some_and(|routine_key| {
            document.symbols.iter().any(|declaration| {
                declaration.kind == SymbolKind::Routine
                    && declaration.origin == Origin::Declaration
                    && declaration.routine_key.as_ref() == Some(routine_key)
                    && (declaration.region == Region::Interface
                        || declaration.routine_directives.forward)
                    && declaration.span.start <= offset
            })
        })
}

fn visibility_for_declaration(node: Node<'_>, source: &str) -> Visibility {
    if enclosing_type(node, source).is_none() {
        return Visibility::Public;
    }

    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "declSection" | "ppDeclSection" => {
                let strict = has_direct_child_kind(parent, "kStrict");
                let visibility = [
                    ("kPublished", Visibility::Published),
                    ("kPublic", Visibility::Public),
                    ("kProtected", Visibility::Protected),
                    ("kPrivate", Visibility::Private),
                ]
                .into_iter()
                .find_map(|(kind, visibility)| {
                    has_direct_child_kind(parent, kind).then_some(visibility)
                })
                .unwrap_or(Visibility::Public);
                return match (strict, visibility) {
                    (true, Visibility::Private) => Visibility::StrictPrivate,
                    (true, Visibility::Protected) => Visibility::StrictProtected,
                    _ => visibility,
                };
            }
            "declType" => break,
            _ => current = parent.parent(),
        }
    }
    Visibility::Public
}

fn has_direct_child_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

fn inherit_member_routine_visibility(symbols: &mut [Symbol]) {
    let declarations = symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == SymbolKind::Routine
                && symbol.origin == Origin::Declaration
                && symbol.owner_type.is_some()
        })
        .filter_map(|symbol| Some((symbol.routine_key.as_ref()?.clone(), symbol.visibility)))
        .collect::<HashMap<_, _>>();

    for symbol in symbols.iter_mut().filter(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.origin == Origin::Definition
            && symbol.owner_type.is_some()
    }) {
        if let Some(routine_key) = &symbol.routine_key {
            if let Some(visibility) = declarations.get(routine_key) {
                symbol.visibility = *visibility;
            }
        }
    }
}

fn member_symbol_is_visible(
    document: &Document,
    symbol: &Symbol,
    type_key: &str,
    allow_implementation: bool,
) -> bool {
    if symbol.local_only || symbol.owner_type.as_deref() != Some(type_key) {
        return false;
    }
    match symbol.kind {
        SymbolKind::Routine => {
            (symbol.origin == Origin::Declaration
                && (symbol.region == Region::Interface
                    || (allow_implementation && symbol.region == Region::Implementation)))
                || (symbol.origin == Origin::Definition
                    && symbol.routine_key.as_ref().is_some_and(|key| {
                        document
                            .interface_member_routine_keys
                            .get(type_key)
                            .is_some_and(|keys| keys.contains(key))
                    }))
        }
        _ => {
            symbol.region == Region::Interface
                || (allow_implementation && symbol.region == Region::Implementation)
        }
    }
}

fn complete_ancestry(parents: Vec<(Url, String)>) -> TypeAncestryResolution {
    TypeAncestryResolution {
        status: AncestryStatus::Complete,
        parents,
    }
}

fn incomplete_ancestry(parents: Vec<(Url, String)>) -> TypeAncestryResolution {
    TypeAncestryResolution {
        status: AncestryStatus::Unknown,
        parents,
    }
}

fn unknown_ancestry() -> TypeAncestryResolution {
    incomplete_ancestry(Vec::new())
}

fn push_unique_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) {
    if !candidates
        .iter()
        .any(|current| current.uri == candidate.uri && current.index == candidate.index)
    {
        candidates.push(candidate);
    }
}

fn dedup_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut result = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        push_unique_candidate(&mut result, candidate);
    }
    result
}

fn candidate_sets_equal(left: &[Candidate], right: &[Candidate]) -> bool {
    left.len() == right.len()
        && left.iter().all(|candidate| {
            right
                .iter()
                .any(|other| candidate.uri == other.uri && candidate.index == other.index)
        })
}

fn collect_symbols(
    root: Node<'_>,
    source: &str,
    scopes: &[Scope],
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    collect_nodes(root, &mut |node| match node.kind() {
        "defProc" => add_definition_symbol(node, source, scopes, scope_by_span, symbols),
        "declProc" if !is_definition_header(node) => {
            add_routine_symbol(node, source, scope_by_span, symbols);
        }
        "declType" => {
            add_named_symbol(node, source, scope_by_span, symbols, SymbolKind::Type, None)
        }
        "declVar" | "varDef" | "varAssignDef" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Variable,
                type_name,
            );
        }
        "declConst" => add_named_symbol(
            node,
            source,
            scope_by_span,
            symbols,
            SymbolKind::Constant,
            None,
        ),
        "declField" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Field,
                type_name,
            );
        }
        "declProp" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Property,
                type_name,
            );
        }
        "declArg" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Parameter,
                type_name,
            );
        }
        "declEnumValue" => add_named_symbol(
            node,
            source,
            scope_by_span,
            symbols,
            SymbolKind::EnumValue,
            None,
        ),
        "declLabel" => add_named_symbol(
            node,
            source,
            scope_by_span,
            symbols,
            SymbolKind::Label,
            None,
        ),
        _ => {}
    });
}

/// Pair a definition that omits generic constraints with one unique declaration.
///
/// The exact routine key keeps constraint shape so distinct constrained overloads
/// remain separate.  This pass only supplies the declaration's exact key when the
/// rest of the identity and generic arity select one declaration unambiguously.
fn pair_omitted_generic_constraints(symbols: &mut [Symbol]) {
    let declaration_indices: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            (symbol.kind == SymbolKind::Routine && symbol.origin == Origin::Declaration)
                .then_some(index)
        })
        .collect();
    let definition_indices: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            (symbol.kind == SymbolKind::Routine && symbol.origin == Origin::Definition)
                .then_some(index)
        })
        .collect();

    let mut generic_parameter_key_replacements = HashMap::new();
    for definition_index in definition_indices {
        let definition = &symbols[definition_index];
        if definition.generic_parameters.is_empty()
            || definition
                .generic_parameters
                .iter()
                .all(|parameter| parameter.constraint.is_some() || parameter.constraint_unsupported)
        {
            continue;
        }
        let matching_declarations: Vec<usize> = declaration_indices
            .iter()
            .copied()
            .filter(|&declaration_index| {
                let declaration = &symbols[declaration_index];
                declaration.scope == definition.scope
                    && declaration.owner_type == definition.owner_type
                    && declaration.key == definition.key
                    && declaration.routine_signature == definition.routine_signature
                    && generic_parameters_match_with_omissions(
                        &definition.generic_parameters,
                        &declaration.generic_parameters,
                    )
            })
            .collect();
        let Some(&declaration_index) = matching_declarations
            .first()
            .filter(|_| matching_declarations.len() == 1)
        else {
            continue;
        };

        let Some(routine_key) = symbols[declaration_index].routine_key.clone() else {
            continue;
        };
        let old_routine_key = definition.routine_key.clone();
        symbols[definition_index].routine_key = Some(routine_key.clone());
        symbols[definition_index].generic_parameters =
            symbols[declaration_index].generic_parameters.clone();
        symbols[definition_index].routine_directives =
            symbols[declaration_index].routine_directives;
        symbols[definition_index].result_type_name =
            symbols[declaration_index].result_type_name.clone();
        symbols[definition_index].result_type_ref =
            symbols[declaration_index].result_type_ref.clone();
        symbols[definition_index].result_type_span = symbols[declaration_index].result_type_span;

        if let Some(old_routine_key) = old_routine_key {
            match generic_parameter_key_replacements.entry(old_routine_key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Some(routine_key));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().as_ref() != Some(&routine_key) {
                        entry.insert(None);
                    }
                }
            }
        }
    }
    for symbol in symbols.iter_mut() {
        if symbol.kind != SymbolKind::Type || symbol.generic_parameter.is_none() {
            continue;
        }
        let Some(routine_key) = symbol
            .routine_key
            .as_ref()
            .and_then(|key| generic_parameter_key_replacements.get(key))
            .and_then(Option::as_ref)
        else {
            continue;
        };
        symbol.routine_key = Some(routine_key.clone());
    }
}

fn generic_parameters_match_with_omissions(
    definition: &[GenericParameter],
    declaration: &[GenericParameter],
) -> bool {
    definition.len() == declaration.len()
        && definition
            .iter()
            .zip(declaration)
            .all(|(definition, declaration)| {
                if definition.constraint.is_none() && !definition.constraint_unsupported {
                    true
                } else {
                    generic_parameter_shape(definition) == generic_parameter_shape(declaration)
                }
            })
}

fn pair_abbreviated_definitions(
    root: Node<'_>,
    source: &str,
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    #[derive(Default)]
    struct DeclarationGroup {
        has_exact: bool,
        matching_count: usize,
        unique_matching: Option<usize>,
    }

    let mut declaration_nodes_by_key: HashMap<String, Vec<Node<'_>>> = HashMap::new();
    for node in collect_nodes_matching(root, "declProc") {
        if is_definition_header(node) || !is_routine_declaration_region(region_for_node(node)) {
            continue;
        }
        let Some((name, _, owner_type)) = routine_name(node, source) else {
            continue;
        };
        let signature = routine_signature(node, source);
        let generic_parameters = generic_parameters_for_node(node, source);
        let routine_key = routine_key_with_owner(
            owner_type.as_deref(),
            &name,
            &signature,
            &generic_parameters,
            scope_for_declaration(node, scope_by_span),
        );
        declaration_nodes_by_key
            .entry(routine_key)
            .or_default()
            .push(node);
    }

    let mut declaration_groups: HashMap<(usize, String, Option<String>), DeclarationGroup> =
        HashMap::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if symbol.kind == SymbolKind::Routine
            && symbol.origin == Origin::Declaration
            && is_routine_declaration_region(symbol.region)
        {
            let group = declaration_groups
                .entry((symbol.scope, symbol.key.clone(), symbol.owner_type.clone()))
                .or_default();
            if symbol.routine_signature.as_deref() == Some("") {
                group.has_exact = true;
            } else if symbol.routine_signature.is_some() {
                group.matching_count += 1;
                if group.matching_count == 1 {
                    group.unique_matching = Some(index);
                }
            }
        }
    }

    let abbreviated_definitions: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            (symbol.kind == SymbolKind::Routine
                && symbol.origin == Origin::Definition
                && symbol.routine_signature.as_deref() == Some(""))
            .then_some(index)
        })
        .collect();

    for definition_index in abbreviated_definitions {
        let definition_scope = symbols[definition_index].scope;
        let definition_key = symbols[definition_index].key.clone();
        let definition_owner = symbols[definition_index].owner_type.clone();
        let group_key = (definition_scope, definition_key, definition_owner);
        let Some(group) = declaration_groups.get(&group_key) else {
            continue;
        };
        if group.has_exact {
            continue;
        }
        if group.matching_count != 1 {
            if group.matching_count > 1 {
                symbols[definition_index].unresolved_abbreviated = true;
            }
            continue;
        }
        let Some(declaration_index) = group.unique_matching else {
            continue;
        };

        let Some(routine_key) = symbols[declaration_index].routine_key.clone() else {
            continue;
        };
        let body_scope = symbols[definition_index].body_scope;
        symbols[definition_index].routine_key = Some(routine_key.clone());
        symbols[definition_index].routine_directives =
            symbols[declaration_index].routine_directives;
        symbols[definition_index].result_type_name =
            symbols[declaration_index].result_type_name.clone();
        symbols[definition_index].result_type_ref =
            symbols[declaration_index].result_type_ref.clone();
        symbols[definition_index].result_type_span = symbols[declaration_index].result_type_span;
        symbols[definition_index].generic_parameters =
            symbols[declaration_index].generic_parameters.clone();
        let declaration_node = declaration_nodes_by_key
            .get(&routine_key)
            .and_then(|nodes| nodes.first().copied());
        let Some(body_scope) = body_scope else {
            continue;
        };
        if let Some(declaration_node) = declaration_node {
            inject_abbreviated_parameters(declaration_node, body_scope, source, symbols);
        }
    }

    let mut declarations_by_routine_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if symbol.kind == SymbolKind::Routine && symbol.origin == Origin::Declaration {
            if let Some(routine_key) = symbol.routine_key.as_ref() {
                declarations_by_routine_key
                    .entry(routine_key.clone())
                    .or_default()
                    .push(index);
            }
        }
    }
    let definition_indices: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            (symbol.kind == SymbolKind::Routine && symbol.origin == Origin::Definition)
                .then_some(index)
        })
        .collect();
    for definition_index in definition_indices {
        let Some(routine_key) = symbols[definition_index].routine_key.as_ref() else {
            continue;
        };
        let Some(declarations) = declarations_by_routine_key.get(routine_key) else {
            continue;
        };
        let Some(&declaration_index) = declarations.first().filter(|_| declarations.len() == 1)
        else {
            continue;
        };
        symbols[definition_index].result_type_name =
            symbols[declaration_index].result_type_name.clone();
        symbols[definition_index].result_type_ref =
            symbols[declaration_index].result_type_ref.clone();
        symbols[definition_index].result_type_span = symbols[declaration_index].result_type_span;
        symbols[definition_index].generic_parameters =
            symbols[declaration_index].generic_parameters.clone();
        symbols[definition_index].routine_directives =
            symbols[declaration_index].routine_directives;
    }
}

fn inject_abbreviated_parameters(
    declaration: Node<'_>,
    body_scope: usize,
    source: &str,
    symbols: &mut Vec<Symbol>,
) {
    let Some(arguments) = declaration.child_by_field_name("args") else {
        return;
    };
    collect_nodes(arguments, &mut |node| {
        if node.kind() != "declArg" {
            return;
        }
        let type_name = node
            .child_by_field_name("type")
            .and_then(|type_node| simple_type_path(type_node, source));
        let type_ref = node
            .child_by_field_name("type")
            .and_then(|type_node| type_ref_from_node(type_node, source));
        let type_shape = node
            .child_by_field_name("type")
            .and_then(|type_node| type_shape_from_node(type_node, source));
        for identifier in field_identifier_nodes(node, "name") {
            let name = node_text(identifier, source);
            if name.is_empty() {
                continue;
            }
            symbols.push(Symbol {
                span: Span::from_node(identifier),
                declaration_span: Span::from_node(node),
                selection_span: Span::from_node(identifier),
                name: name.clone(),
                key: canonical_name(&name),
                kind: SymbolKind::Parameter,
                type_kind: TypeKind::Other,
                routine_kind: RoutineKind::Procedure,
                routine_directives: RoutineDirectives::default(),
                parameter_mode: Some(parameter_mode(node)),
                scope: body_scope,
                owner_type: None,
                owner_type_name: None,
                visibility: Visibility::Public,
                declaration_ordered: false,
                generic_parameters: Vec::new(),
                generic_parameter: None,
                type_name: type_name.clone(),
                type_ref: type_ref.clone(),
                type_shape: type_shape.clone(),
                result_type_name: None,
                result_type_ref: None,
                result_type_span: None,
                region: Region::Implementation,
                origin: Origin::Declaration,
                is_static: false,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                routine_header_span: None,
                routine_parameter_spans: Vec::new(),
                routine_parameters: Vec::new(),
                type_excerpt_end: None,
                body_scope: None,
                unresolved_abbreviated: false,
                accessor: None,
            });
        }
    });
}

fn add_definition_symbol(
    node: Node<'_>,
    source: &str,
    scopes: &[Scope],
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    let Some(header) = node.child_by_field_name("header") else {
        return;
    };
    let Some((name, span, owner_type)) = routine_name(header, source) else {
        return;
    };
    let owner_type_name = routine_owner_name(header, source);
    let own_scope = scope_by_span
        .get(&Span::from_node(node))
        .copied()
        .unwrap_or(ROOT_SCOPE);
    let scope = scopes[own_scope].parent.unwrap_or(ROOT_SCOPE);
    let signature = routine_signature(header, source);
    let result_type = routine_result_type(header, source);
    let result_type_ref = header
        .child_by_field_name("type")
        .and_then(|type_node| type_ref_from_node(type_node, source));
    let generic_parameters = generic_parameters_for_node(header, source);
    let routine_parameters = direct_routine_parameters(header, source);
    let routine_key = routine_key_with_owner(
        owner_type.as_deref(),
        &name,
        &signature,
        &generic_parameters,
        scope,
    );
    symbols.push(Symbol {
        span,
        declaration_span: Span::from_node(node),
        selection_span: span,
        name: name.clone(),
        key: canonical_name(&name),
        kind: SymbolKind::Routine,
        type_kind: TypeKind::Other,
        routine_kind: routine_kind(header),
        routine_directives: routine_directives(header),
        parameter_mode: None,
        scope,
        owner_type: owner_type.clone(),
        owner_type_name,
        visibility: Visibility::Public,
        declaration_ordered: scope != ROOT_SCOPE || region_for_node(node) != Region::Interface,
        generic_parameters: generic_parameters.clone(),
        generic_parameter: None,
        type_name: None,
        type_ref: None,
        type_shape: None,
        result_type_name: result_type.as_ref().map(|(name, _)| name.clone()),
        result_type_ref,
        result_type_span: result_type.map(|(_, span)| span),
        region: region_for_node(node),
        origin: Origin::Definition,
        is_static: has_class_modifier(header),
        local_only: false,
        routine_key: Some(routine_key.clone()),
        routine_signature: Some(signature),
        routine_header_span: Some(Span::from_node(header)),
        routine_parameter_spans: routine_parameters
            .iter()
            .map(|parameter| parameter.span)
            .collect(),
        routine_parameters,
        type_excerpt_end: None,
        body_scope: Some(own_scope),
        unresolved_abbreviated: false,
        accessor: None,
    });
    push_routine_generic_parameter_symbols(
        symbols,
        &generic_parameters,
        &routine_key,
        scope,
        region_for_node(node),
    );
}

fn add_routine_symbol(
    node: Node<'_>,
    source: &str,
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    let Some((name, span, owner_type)) = routine_name(node, source) else {
        return;
    };
    let owner_type_name = routine_owner_name(node, source);
    let scope = scope_for_declaration(node, scope_by_span);
    let signature = routine_signature(node, source);
    let result_type = routine_result_type(node, source);
    let result_type_ref = node
        .child_by_field_name("type")
        .and_then(|type_node| type_ref_from_node(type_node, source));
    let generic_parameters = generic_parameters_for_node(node, source);
    let routine_parameters = direct_routine_parameters(node, source);
    let routine_key = routine_key_with_owner(
        owner_type.as_deref(),
        &name,
        &signature,
        &generic_parameters,
        scope,
    );
    symbols.push(Symbol {
        span,
        declaration_span: Span::from_node(node),
        selection_span: span,
        name: name.clone(),
        key: canonical_name(&name),
        kind: SymbolKind::Routine,
        type_kind: TypeKind::Other,
        routine_kind: routine_kind(node),
        routine_directives: routine_directives(node),
        parameter_mode: None,
        scope,
        owner_type: owner_type.clone(),
        owner_type_name,
        visibility: visibility_for_declaration(node, source),
        declaration_ordered: scope != ROOT_SCOPE || region_for_node(node) != Region::Interface,
        generic_parameters: generic_parameters.clone(),
        generic_parameter: None,
        type_name: None,
        type_ref: None,
        type_shape: None,
        result_type_name: result_type.as_ref().map(|(name, _)| name.clone()),
        result_type_ref,
        result_type_span: result_type.map(|(_, span)| span),
        region: region_for_node(node),
        origin: Origin::Declaration,
        is_static: has_class_modifier(node),
        local_only: false,
        routine_key: Some(routine_key.clone()),
        routine_signature: Some(signature),
        routine_header_span: Some(Span::from_node(node)),
        routine_parameter_spans: routine_parameters
            .iter()
            .map(|parameter| parameter.span)
            .collect(),
        routine_parameters,
        type_excerpt_end: None,
        body_scope: None,
        unresolved_abbreviated: false,
        accessor: None,
    });
    push_routine_generic_parameter_symbols(
        symbols,
        &generic_parameters,
        &routine_key,
        scope,
        region_for_node(node),
    );
}

fn push_routine_generic_parameter_symbols(
    symbols: &mut Vec<Symbol>,
    parameters: &[GenericParameter],
    routine_key: &str,
    scope: usize,
    region: Region,
) {
    for parameter in parameters {
        symbols.push(Symbol {
            span: parameter.span,
            declaration_span: parameter.span,
            selection_span: parameter.span,
            name: parameter.name.clone(),
            key: parameter.name.clone(),
            kind: SymbolKind::Type,
            type_kind: TypeKind::Other,
            routine_kind: RoutineKind::Procedure,
            routine_directives: RoutineDirectives::default(),
            parameter_mode: None,
            scope,
            owner_type: None,
            owner_type_name: None,
            visibility: Visibility::Public,
            declaration_ordered: false,
            generic_parameters: Vec::new(),
            generic_parameter: Some(parameter.name.clone()),
            type_name: parameter.constraint.as_ref().map(TypeRef::display),
            type_ref: parameter.constraint.clone(),
            type_shape: None,
            result_type_name: None,
            result_type_ref: None,
            result_type_span: None,
            region,
            origin: Origin::Declaration,
            is_static: false,
            local_only: false,
            routine_key: Some(routine_key.to_owned()),
            routine_signature: None,
            routine_header_span: None,
            routine_parameter_spans: Vec::new(),
            routine_parameters: Vec::new(),
            type_excerpt_end: None,
            body_scope: None,
            unresolved_abbreviated: false,
            accessor: None,
        });
    }
}

fn add_named_symbol(
    node: Node<'_>,
    source: &str,
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
    kind: SymbolKind,
    type_name: Option<String>,
) {
    let owner_type = enclosing_type(node, source);
    let owner_type_name = enclosing_type_name(node, source);
    let scope = scope_for_declaration(node, scope_by_span);
    let local_only = kind == SymbolKind::Parameter && scope == ROOT_SCOPE;
    let type_kind = if kind == SymbolKind::Type {
        type_kind_for_declaration(node)
    } else {
        TypeKind::Other
    };
    let is_static = declaration_is_static(node);
    let type_excerpt_end = if kind == SymbolKind::Type {
        type_declaration_excerpt_end(node, type_kind)
    } else {
        None
    };
    let accessor = if kind == SymbolKind::Property {
        property_accessor(node, source)
    } else {
        None
    };
    let generic_parameters = if kind == SymbolKind::Type {
        generic_parameters_for_node(node, source)
    } else {
        Vec::new()
    };
    let type_ref = node
        .child_by_field_name("type")
        .and_then(|type_node| type_ref_from_node(type_node, source));
    let type_shape = node
        .child_by_field_name("type")
        .and_then(|type_node| type_shape_from_node(type_node, source));
    let identifiers = if kind == SymbolKind::Type {
        declaration_name_identifiers(node)
    } else if matches!(node.kind(), "varDef" | "varAssignDef") {
        identifier_nodes(node).into_iter().take(1).collect()
    } else {
        field_identifier_nodes(node, "name")
    };
    let declared_type_key = (kind == SymbolKind::Type)
        .then(|| identifiers.last())
        .flatten()
        .map(|identifier| canonical_name(&node_text(*identifier, source)));
    for identifier in identifiers {
        let name = node_text(identifier, source);
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            span: Span::from_node(identifier),
            declaration_span: Span::from_node(node),
            selection_span: Span::from_node(identifier),
            name: name.clone(),
            key: canonical_name(&name),
            kind,
            type_kind,
            routine_kind: RoutineKind::Procedure,
            routine_directives: RoutineDirectives::default(),
            parameter_mode: (kind == SymbolKind::Parameter).then(|| parameter_mode(node)),
            scope,
            owner_type: owner_type.clone(),
            owner_type_name: owner_type_name.clone(),
            visibility: visibility_for_declaration(node, source),
            declaration_ordered: scope != ROOT_SCOPE && owner_type.is_none(),
            generic_parameters: generic_parameters.clone(),
            generic_parameter: None,
            type_name: type_name.clone(),
            type_ref: type_ref.clone(),
            type_shape: type_shape.clone(),
            result_type_name: None,
            result_type_ref: None,
            result_type_span: None,
            region: region_for_node(node),
            origin: Origin::Declaration,
            is_static,
            local_only,
            routine_key: None,
            routine_signature: None,
            routine_header_span: None,
            routine_parameter_spans: Vec::new(),
            routine_parameters: Vec::new(),
            type_excerpt_end,
            body_scope: None,
            unresolved_abbreviated: false,
            accessor: accessor.clone(),
        });
    }

    if kind == SymbolKind::Type {
        for parameter in generic_parameters {
            symbols.push(Symbol {
                span: parameter.span,
                declaration_span: parameter.span,
                selection_span: parameter.span,
                name: parameter.name.clone(),
                key: parameter.name.clone(),
                kind: SymbolKind::Type,
                type_kind: TypeKind::Other,
                routine_kind: RoutineKind::Procedure,
                routine_directives: RoutineDirectives::default(),
                parameter_mode: None,
                scope,
                owner_type: declared_type_key.clone(),
                owner_type_name: declared_type_key.clone(),
                visibility: Visibility::Public,
                declaration_ordered: false,
                generic_parameters: Vec::new(),
                generic_parameter: Some(parameter.name),
                type_name: parameter.constraint.as_ref().map(TypeRef::display),
                type_ref: parameter.constraint,
                type_shape: None,
                result_type_name: None,
                result_type_ref: None,
                result_type_span: None,
                region: region_for_node(node),
                origin: Origin::Declaration,
                is_static: false,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                routine_header_span: None,
                routine_parameter_spans: Vec::new(),
                routine_parameters: Vec::new(),
                type_excerpt_end: None,
                body_scope: None,
                unresolved_abbreviated: false,
                accessor: None,
            });
        }
    }
}

fn declaration_is_static(node: Node<'_>) -> bool {
    match node.kind() {
        "declProc" | "declProp" => has_class_modifier(node),
        "declVar" => node
            .parent()
            .is_some_and(|parent| parent.kind() == "declVars" && has_class_modifier(parent)),
        "declConst" => node
            .parent()
            .is_some_and(|parent| parent.kind() == "declConsts" && has_class_modifier(parent)),
        _ => false,
    }
}

fn has_class_modifier(node: Node<'_>) -> bool {
    if matches!(node.kind(), "declVars" | "declConsts") {
        return node.child(0).is_some_and(|child| child.kind() == "kClass");
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .take(4)
        .any(|child| child.kind() == "kClass")
}

fn type_declaration_excerpt_end(node: Node<'_>, type_kind: TypeKind) -> Option<usize> {
    if !matches!(
        type_kind,
        TypeKind::Class | TypeKind::Record | TypeKind::Interface
    ) {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    let mut body_start: Option<usize> = None;
    collect_nodes(type_node, &mut |child| {
        if matches!(
            child.kind(),
            "declSection"
                | "declField"
                | "declProc"
                | "declProp"
                | "declTypes"
                | "declVariant"
                | "declEnum"
        ) {
            body_start =
                Some(body_start.map_or(child.start_byte(), |start| start.min(child.start_byte())));
        }
    });
    Some(body_start.unwrap_or_else(|| type_node.end_byte()))
}

fn property_accessor(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("getter")
        .or_else(|| node.child_by_field_name("setter"))
        .map(|accessor| node_text(accessor, source))
        .filter(|accessor| !accessor.is_empty())
}

fn type_kind_for_declaration(node: Node<'_>) -> TypeKind {
    let Some(type_node) = node.child_by_field_name("type") else {
        return TypeKind::Other;
    };
    let mut pending = vec![type_node];
    while let Some(current) = pending.pop() {
        match current.kind() {
            "declIntf" => return TypeKind::Interface,
            "declEnum" => return TypeKind::Enum,
            "declRecord" => return TypeKind::Record,
            "declHelper" => return helper_kind(current),
            "declArray" => {
                let has_range = (0..current.named_child_count())
                    .filter_map(|index| current.named_child(index))
                    .any(|child| child.kind() == "range");
                return if has_range {
                    TypeKind::Array
                } else {
                    TypeKind::DynamicArray
                };
            }
            "declSet" | "kArray" => return TypeKind::Array,
            "typerefPtr" => return TypeKind::Pointer,
            "declProcRef" | "kProcedure" | "kFunction" => return TypeKind::Callable,
            "declString" | "kString" => return TypeKind::String,
            "declFile" | "kFile" => return TypeKind::File,
            "declMetaClass" => return TypeKind::Class,
            "declClass" => {
                let mut cursor = current.walk();
                return current
                    .children(&mut cursor)
                    .find_map(|child| match child.kind() {
                        "kRecord" => Some(TypeKind::Record),
                        "kClass" | "kObject" => Some(TypeKind::Class),
                        _ => None,
                    })
                    .unwrap_or(TypeKind::Class);
            }
            _ => {
                let mut cursor = current.walk();
                let children: Vec<_> = current.children(&mut cursor).collect();
                pending.extend(children.into_iter().rev());
            }
        }
    }
    TypeKind::Other
}

fn helper_kind(node: Node<'_>) -> TypeKind {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| match child.kind() {
            "kClass" => Some(TypeKind::Class),
            "kRecord" => Some(TypeKind::Record),
            "kType" => Some(TypeKind::Other),
            _ => None,
        })
        .unwrap_or(TypeKind::Other)
}

fn is_type_reference_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "type" | "typeref" | "typerefDot" | "typerefTpl" | "typerefPtr" | "typerefArgs"
    )
}

fn helper_target_node<'a>(node: Node<'a>, source: &str) -> Option<Node<'a>> {
    let for_end = {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "kFor")
            .map(|child| child.end_byte())?
    };
    let mut candidates = Vec::new();
    collect_nodes(node, &mut |child| {
        if child.start_byte() >= for_end
            && is_type_reference_node(child)
            && type_ref_from_node(child, source).is_some()
        {
            candidates.push(child);
        }
    });
    candidates.sort_by_key(|candidate| {
        (
            candidate.start_byte(),
            std::cmp::Reverse(candidate.end_byte().saturating_sub(candidate.start_byte())),
        )
    });
    candidates.into_iter().next()
}

fn helper_parent_node<'a>(node: Node<'a>, for_start: usize, source: &str) -> Option<Node<'a>> {
    let mut candidates = Vec::new();
    collect_nodes(node, &mut |child| {
        if child.end_byte() <= for_start
            && is_type_reference_node(child)
            && type_ref_from_node(child, source).is_some()
        {
            candidates.push(child);
        }
    });
    candidates.sort_by_key(|candidate| {
        (
            candidate.start_byte(),
            std::cmp::Reverse(candidate.end_byte().saturating_sub(candidate.start_byte())),
        )
    });
    candidates.into_iter().next()
}

fn collect_helpers(root: Node<'_>, source: &str) -> Vec<HelperDefinition> {
    let mut helpers = Vec::new();
    for declaration in collect_nodes_matching(root, "declType") {
        if enclosing_type(declaration, source).is_some() {
            continue;
        }
        let Some(type_node) = declaration.child_by_field_name("type") else {
            continue;
        };
        let Some(helper_node) = ({
            let mut found = None;
            collect_nodes(type_node, &mut |child| {
                if found.is_none() && child.kind() == "declHelper" {
                    found = Some(child);
                }
            });
            found
        }) else {
            continue;
        };
        let kind = helper_kind(helper_node);
        if !matches!(kind, TypeKind::Class | TypeKind::Record) {
            continue;
        }
        let Some(name) = declaration_name_identifiers(declaration).last().copied() else {
            continue;
        };
        let Some(target_node) = helper_target_node(helper_node, source) else {
            continue;
        };
        let Some(target) = type_ref_from_node(target_node, source) else {
            continue;
        };
        let generic_parameters = generic_parameters_for_node(declaration, source);
        let parent = {
            let for_start = {
                let mut cursor = helper_node.walk();
                helper_node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "kFor")
                    .map(|child| child.start_byte())
            };
            for_start
                .and_then(|for_start| helper_parent_node(helper_node, for_start, source))
                .and_then(|parent| type_ref_from_node(parent, source))
        };
        helpers.push(HelperDefinition {
            key: canonical_name(&node_text(name, source)),
            name_span: Span::from_node(name),
            declaration_span: Span::from_node(declaration),
            kind,
            generic_parameters,
            target,
            parent,
            region: region_for_node(declaration),
        });
    }
    helpers.sort_by_key(|helper| helper.declaration_span.start);
    helpers
}

fn helper_visible_at(
    helper: &HelperDefinition,
    local: bool,
    region: Region,
    offset: usize,
) -> bool {
    if local && helper.declaration_span.start > offset {
        return false;
    }
    if local {
        helper.region == Region::Interface
            || (helper.region == Region::Implementation
                && matches!(region, Region::Implementation | Region::Other))
    } else {
        helper.region == Region::Interface
    }
}

fn helper_target_may_match(helper: &HelperDefinition, target: &TypeInstance) -> bool {
    helper
        .target
        .path
        .last()
        .is_some_and(|name| canonical_name(name) == target.key)
        || matches!(
            (helper.kind, target.kind),
            (TypeKind::Class, TypeKind::Class)
        )
}

fn target_instances_match_for_helper(
    resolved: &TypeInstance,
    target: &TypeInstance,
    unspecialized_target: bool,
    helper_uri: &Url,
    generic_parameters: &[GenericParameter],
) -> bool {
    if resolved.uri != target.uri || resolved.key != target.key || resolved.kind != target.kind {
        return false;
    }
    if generic_parameters.is_empty() {
        return target_instances_match(resolved, target, unspecialized_target);
    }
    if unspecialized_target {
        return true;
    }
    if resolved.parameter_names != target.parameter_names {
        return false;
    }

    let mut bindings = GenericSubstitution::empty();
    resolved.parameter_names.iter().all(|name| {
        match (
            resolved.substitution.get(name),
            target.substitution.get(name),
        ) {
            (Some(pattern), Some(actual)) => helper_type_pattern_matches(
                pattern,
                actual,
                helper_uri,
                generic_parameters,
                &mut bindings,
            ),
            (None, None) => true,
            _ => false,
        }
    })
}

fn helper_type_pattern_matches(
    pattern: &ResolvedType,
    actual: &ResolvedType,
    helper_uri: &Url,
    generic_parameters: &[GenericParameter],
    bindings: &mut GenericSubstitution,
) -> bool {
    if let ResolvedType::Named(instance) = pattern {
        if instance.uri == *helper_uri
            && instance.kind == TypeKind::Other
            && instance.parameter_names.is_empty()
            && instance.substitution.0.is_empty()
            && generic_parameters
                .iter()
                .any(|parameter| parameter.name == instance.key)
        {
            return match bindings.get(&instance.key) {
                Some(bound) => bound == actual,
                None => {
                    bindings.insert(&instance.key, actual.clone());
                    true
                }
            };
        }
    }

    match (pattern, actual) {
        (ResolvedType::Builtin(pattern), ResolvedType::Builtin(actual)) => pattern == actual,
        (ResolvedType::IntegerLiteral(pattern), ResolvedType::IntegerLiteral(actual)) => {
            pattern == actual
        }
        (ResolvedType::Named(pattern), ResolvedType::Named(actual)) => {
            pattern.uri == actual.uri
                && pattern.key == actual.key
                && pattern.kind == actual.kind
                && pattern.parameter_names == actual.parameter_names
                && pattern.parameter_names.iter().all(|name| {
                    match (
                        pattern.substitution.get(name),
                        actual.substitution.get(name),
                    ) {
                        (Some(pattern), Some(actual)) => helper_type_pattern_matches(
                            pattern,
                            actual,
                            helper_uri,
                            generic_parameters,
                            bindings,
                        ),
                        (None, None) => true,
                        _ => false,
                    }
                })
        }
        _ => false,
    }
}

fn target_instances_match(
    resolved: &TypeInstance,
    target: &TypeInstance,
    unspecialized_target: bool,
) -> bool {
    resolved.uri == target.uri
        && resolved.key == target.key
        && resolved.kind == target.kind
        && (unspecialized_target || resolved.substitution == target.substitution)
}

fn collect_type_ancestry(root: Node<'_>, source: &str) -> HashMap<String, Vec<TypeAncestry>> {
    let mut ancestry = HashMap::new();
    for declaration in collect_nodes_matching(root, "declType") {
        if enclosing_type(declaration, source).is_some() {
            continue;
        }
        let Some(name) = declaration_name_identifiers(declaration).last().copied() else {
            continue;
        };
        let type_kind = type_kind_for_declaration(declaration);
        let Some(type_node) = declaration.child_by_field_name("type") else {
            continue;
        };
        let Some(shape) = type_shape_node(type_node, type_kind) else {
            ancestry
                .entry(canonical_name(&node_text(name, source)))
                .or_insert_with(Vec::new)
                .push(TypeAncestry {
                    kind: type_kind,
                    name_span: Span::from_node(name),
                    parent_declared: false,
                    abstract_: type_declaration_is_abstract(type_node),
                    parents: Vec::new(),
                });
            continue;
        };

        let parent_fields = {
            let mut cursor = shape.walk();
            shape
                .children_by_field_name("parent", &mut cursor)
                .collect::<Vec<_>>()
        };
        let parent_declared = !parent_fields.is_empty();
        let mut parent_spans = HashSet::new();
        let mut parents = Vec::new();
        let mut parent_position = 0usize;
        for field in parent_fields {
            let fields = if field.kind() == "typeref" {
                vec![field]
            } else {
                let mut cursor = field.walk();
                field
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "typeref")
                    .collect()
            };
            for parent in fields {
                let relation = match type_kind {
                    TypeKind::Class => {
                        if parent_position == 0 {
                            ParentRelation::Superclass
                        } else {
                            ParentRelation::ImplementedInterface
                        }
                    }
                    TypeKind::Interface => ParentRelation::InterfaceParent,
                    _ => ParentRelation::ImplementedInterface,
                };
                parent_position = parent_position.saturating_add(1);
                let span = Span::from_node(parent);
                if !parent_spans.insert(span) {
                    continue;
                }
                let type_ref = type_ref_from_node(parent, source);
                let path = type_ref
                    .as_ref()
                    .map(|type_ref| type_ref.path.clone())
                    .unwrap_or_default();
                parents.push(ParentType {
                    path,
                    type_ref,
                    relation,
                    span,
                });
            }
        }

        ancestry
            .entry(canonical_name(&node_text(name, source)))
            .or_insert_with(Vec::new)
            .push(TypeAncestry {
                kind: type_kind,
                name_span: Span::from_node(name),
                parent_declared,
                abstract_: type_declaration_is_abstract(type_node),
                parents,
            });
    }
    ancestry
}

fn collect_method_resolutions(root: Node<'_>, source: &str) -> Vec<MethodResolution> {
    let mut resolutions = Vec::new();
    for declaration in collect_nodes_matching(root, "declProc") {
        let Some(class_owner) = enclosing_type(declaration, source) else {
            continue;
        };
        let names = declaration_name_identifiers(declaration);
        if names.len() < 2 || declaration.child_by_field_name("assign").is_none() {
            continue;
        }
        let Some(implementation) = declaration
            .child_by_field_name("assign")
            .and_then(|assign| identifier_nodes(assign).last().copied())
        else {
            continue;
        };
        let Some(interface_owner) = names
            .get(names.len().saturating_sub(2))
            .map(|identifier| canonical_name(&node_text(*identifier, source)))
        else {
            continue;
        };
        let Some(interface_method) = names
            .last()
            .map(|identifier| canonical_name(&node_text(*identifier, source)))
        else {
            continue;
        };
        resolutions.push(MethodResolution {
            class_owner,
            interface_owner,
            interface_method,
            implementation_method: canonical_name(&node_text(implementation, source)),
        });
    }
    resolutions
}

fn collect_interface_delegations(root: Node<'_>, source: &str) -> Vec<InterfaceDelegation> {
    let mut delegations = Vec::new();
    for declaration in collect_nodes_matching(root, "declProp") {
        let Some(class_owner) = enclosing_type(declaration, source) else {
            continue;
        };
        let mut cursor = declaration.walk();
        for interface in declaration.children_by_field_name("implements", &mut cursor) {
            let Some(type_ref) = type_ref_from_node(interface, source) else {
                continue;
            };
            delegations.push(InterfaceDelegation {
                class_owner: class_owner.clone(),
                interface: type_ref,
                declaring_uri: None,
                declaring_substitution: None,
            });
        }
    }
    delegations
}

fn type_shape_node(node: Node<'_>, type_kind: TypeKind) -> Option<Node<'_>> {
    let wanted = match type_kind {
        TypeKind::Class | TypeKind::Record => "declClass",
        TypeKind::Interface => "declIntf",
        _ => return None,
    };
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.kind() == wanted {
            return Some(current);
        }
        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    None
}

fn type_declaration_is_abstract(type_node: Node<'_>) -> bool {
    has_direct_child_kind(type_node, "kAbstract")
}

fn routine_kind(node: Node<'_>) -> RoutineKind {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        match current.kind() {
            "kFunction" => return RoutineKind::Function,
            "kConstructor" => return RoutineKind::Constructor,
            "kDestructor" => return RoutineKind::Destructor,
            "kOperator" => return RoutineKind::Operator,
            "kProcedure" => return RoutineKind::Procedure,
            _ => {
                let mut cursor = current.walk();
                let children: Vec<_> = current.children(&mut cursor).collect();
                pending.extend(children.into_iter().rev());
            }
        }
    }
    RoutineKind::Procedure
}

fn routine_directives(node: Node<'_>) -> RoutineDirectives {
    let mut directives = RoutineDirectives::default();
    collect_nodes(node, &mut |child| match child.kind() {
        "kOverload" => directives.overload = true,
        "kVirtual" => directives.virtual_ = true,
        "kDynamic" => directives.dynamic = true,
        "kOverride" => directives.override_ = true,
        "kReintroduce" => directives.reintroduce = true,
        "kForward" => directives.forward = true,
        "kAbstract" => directives.abstract_ = true,
        "kCdecl" => directives.calling_convention = Some(CallingConvention::Cdecl),
        "kStdcall" => directives.calling_convention = Some(CallingConvention::Stdcall),
        "kPascal" => directives.calling_convention = Some(CallingConvention::Pascal),
        "kRegister" => directives.calling_convention = Some(CallingConvention::Register),
        "kSafecall" => directives.calling_convention = Some(CallingConvention::Safecall),
        "kCppdecl" | "kCvar" | "kMwpascal" | "kMs_abi_default" | "kMs_abi_cdecl"
        | "kSaveregisters" | "kSysv_abi_default" | "kSysv_abi_cdecl" | "kVectorcall"
        | "kVarargs" | "kWinapi" => directives.calling_convention_unknown = true,
        _ => {}
    });
    directives
}

fn routine_result_type(node: Node<'_>, source: &str) -> Option<(String, Span)> {
    node.child_by_field_name("type").and_then(|type_node| {
        simple_type_path(type_node, source).map(|name| (name, Span::from_node(type_node)))
    })
}

fn routine_name(node: Node<'_>, source: &str) -> Option<(String, Span, Option<String>)> {
    let identifiers = declaration_name_identifiers(node);
    let last = identifiers.last().copied()?;
    let name = node_text(last, source);
    let owner_type = if identifiers.len() > 1 {
        identifiers
            .get(identifiers.len().saturating_sub(2))
            .map(|identifier| canonical_name(&node_text(*identifier, source)))
    } else {
        enclosing_type(node, source)
    };
    Some((name, Span::from_node(last), owner_type))
}

fn routine_owner_name(node: Node<'_>, source: &str) -> Option<String> {
    let identifiers = declaration_name_identifiers(node);
    if identifiers.len() > 1 {
        return identifiers
            .get(identifiers.len().saturating_sub(2))
            .map(|identifier| node_text(*identifier, source));
    }
    enclosing_type_name(node, source)
}

fn routine_signature(node: Node<'_>, source: &str) -> String {
    let Some(arguments) = node.child_by_field_name("args") else {
        return String::new();
    };
    let mut types = Vec::new();
    for child in direct_routine_argument_groups(arguments) {
        let count = field_identifier_nodes(child, "name").len().max(1);
        let mode = match parameter_mode(child) {
            ParameterMode::Value => "",
            ParameterMode::Var => "var ",
            ParameterMode::Out => "out ",
            ParameterMode::Const => "const ",
            ParameterMode::ConstRef => "constref ",
        };
        let type_name = child
            .child_by_field_name("type")
            .and_then(|type_node| type_shape_from_node(type_node, source))
            .and_then(|shape| missing_unit_type_shape_signature(&shape))
            .unwrap_or_else(|| "?".to_string());
        for _ in 0..count {
            types.push(format!("{mode}{type_name}"));
        }
    }
    types.join(",")
}

fn direct_routine_argument_groups(arguments: Node<'_>) -> Vec<Node<'_>> {
    (0..arguments.named_child_count())
        .filter_map(|index| arguments.named_child(index))
        .filter(|child| child.kind() == "declArg")
        .collect()
}

fn direct_routine_parameters(node: Node<'_>, source: &str) -> Vec<RoutineParameter> {
    let Some(arguments) = node.child_by_field_name("args") else {
        return Vec::new();
    };
    direct_routine_argument_groups(arguments)
        .into_iter()
        .flat_map(|group| {
            let type_node = group.child_by_field_name("type");
            let type_span = type_node.map(Span::from_node);
            let type_name = type_node.and_then(|node| simple_type_path(node, source));
            let type_ref = type_node.and_then(|node| type_ref_from_node(node, source));
            let type_shape = type_node.and_then(|node| type_shape_from_node(node, source));
            let mode = parameter_mode(group);
            let has_default = group.child_by_field_name("defaultValue").is_some();
            field_identifier_nodes(group, "name")
                .into_iter()
                .map(move |identifier| RoutineParameter {
                    span: Span::from_node(identifier),
                    type_span,
                    type_name: type_name.clone(),
                    type_ref: type_ref.clone(),
                    type_shape: type_shape.clone(),
                    mode,
                    has_default,
                })
        })
        .collect()
}

fn parameter_mode(node: Node<'_>) -> ParameterMode {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "kVar" => return ParameterMode::Var,
            "kOut" => return ParameterMode::Out,
            "kConstref" => return ParameterMode::ConstRef,
            "kConst" => return ParameterMode::Const,
            _ => {}
        }
    }
    ParameterMode::Value
}

fn routine_key_with_owner(
    owner_type: Option<&str>,
    name: &str,
    signature: &str,
    generic_parameters: &[GenericParameter],
    scope: usize,
) -> String {
    format!(
        "{}::{}<{}>({})@{}",
        owner_type.unwrap_or_default(),
        canonical_name(name),
        generic_shape(generic_parameters),
        signature,
        scope,
    )
}

fn missing_unit_type_shape_signature(shape: &TypeShape) -> Option<String> {
    match shape {
        TypeShape::Named(type_ref) => Some(type_ref.display()),
        TypeShape::Pointer(element) => {
            Some(format!("^{}", missing_unit_type_shape_signature(element)?))
        }
        TypeShape::Array {
            element,
            dynamic: true,
        } => Some(format!(
            "array of {}",
            missing_unit_type_shape_signature(element)?
        )),
        // Static bounds are not retained by TypeShape, so do not manufacture
        // a routine identity for them from an incomplete representation.
        TypeShape::Array { dynamic: false, .. } | TypeShape::Callable | TypeShape::Unknown => None,
    }
}

fn generic_shape(parameters: &[GenericParameter]) -> String {
    if parameters.is_empty() {
        return "0".to_owned();
    }
    parameters
        .iter()
        .map(generic_parameter_shape)
        .collect::<Vec<_>>()
        .join(",")
}

fn generic_parameter_shape(parameter: &GenericParameter) -> String {
    if parameter.constraint_unsupported {
        "!".to_owned()
    } else {
        parameter
            .constraint
            .as_ref()
            .map_or_else(|| "?".to_owned(), TypeRef::display)
    }
}

fn enclosing_type(node: Node<'_>, source: &str) -> Option<String> {
    let node_span = Span::from_node(node);
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "declType" {
            let Some(type_node) = parent.child_by_field_name("type") else {
                current = parent.parent();
                continue;
            };
            if Span::from_node(type_node).contains(node_span) {
                let identifiers = declaration_name_identifiers(parent);
                return identifiers
                    .last()
                    .map(|identifier| canonical_name(&node_text(*identifier, source)));
            }
        }
        current = parent.parent();
    }
    None
}

fn enclosing_type_name(node: Node<'_>, source: &str) -> Option<String> {
    let node_span = Span::from_node(node);
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "declType"
            && parent
                .child_by_field_name("type")
                .is_some_and(|type_node| Span::from_node(type_node).contains(node_span))
        {
            return declaration_name_identifiers(parent)
                .last()
                .map(|identifier| node_text(*identifier, source));
        }
        current = parent.parent();
    }
    None
}

fn simple_type_path(node: Node<'_>, source: &str) -> Option<String> {
    type_ref_from_node(node, source).map(|type_ref| type_ref.display())
}

fn type_ref_from_node(node: Node<'_>, source: &str) -> Option<TypeRef> {
    type_ref_from_node_at_depth(node, source, 0)
}

fn type_shape_from_node(node: Node<'_>, source: &str) -> Option<TypeShape> {
    let mut type_node = node;
    while matches!(type_node.kind(), "type" | "typeref") {
        type_node = first_named_child(type_node)?;
    }
    match type_node.kind() {
        "identifier" | "typerefDot" | "typerefTpl" | "exprTpl" => {
            type_ref_from_node(type_node, source).map(TypeShape::Named)
        }
        "typerefPtr" => type_node
            .child_by_field_name("operand")
            .and_then(|operand| type_shape_from_node(operand, source))
            .map(|element| TypeShape::Pointer(Box::new(element))),
        "declArray" => {
            let has_range = (0..type_node.named_child_count())
                .filter_map(|index| type_node.named_child(index))
                .any(|child| child.kind() == "range");
            let element = (0..type_node.named_child_count())
                .filter_map(|index| type_node.named_child(index))
                .filter(|child| child.kind() == "type")
                .next_back()
                .and_then(|element| type_shape_from_node(element, source))?;
            Some(TypeShape::Array {
                element: Box::new(element),
                dynamic: !has_range,
            })
        }
        "declProcRef" => Some(TypeShape::Callable),
        "declString" => Some(TypeShape::Named(TypeRef {
            path: vec!["string".to_owned()],
            args: Vec::new(),
            span: Span::from_node(node),
        })),
        _ => Some(TypeShape::Unknown),
    }
}

fn normalized_contract_type_definition(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\'' {
            // Keep string/character literals byte-for-byte meaningful while
            // normalizing identifiers around them.  Comment markers inside a
            // literal are data, not source comments.
            result.push(character);
            while let Some(next) = chars.next() {
                result.push(next);
                if next == '\'' {
                    if chars.peek() == Some(&'\'') {
                        if let Some(escaped_quote) = chars.next() {
                            result.push(escaped_quote);
                        }
                    } else {
                        break;
                    }
                }
            }
            continue;
        }
        if character == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' || next == '\r' {
                    break;
                }
            }
            continue;
        }
        if character == '{' {
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
            }
            continue;
        }
        if character == '(' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if previous == '*' && next == ')' {
                    break;
                }
                previous = next;
            }
            continue;
        }
        if !character.is_whitespace() {
            result.push(character.to_ascii_lowercase());
        }
    }
    result
}

fn type_ref_from_node_at_depth(node: Node<'_>, source: &str, depth: usize) -> Option<TypeRef> {
    if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
        return None;
    }
    let mut type_node = node;
    while matches!(type_node.kind(), "type" | "typeref") {
        type_node = first_named_child(type_node)?;
    }
    match type_node.kind() {
        "identifier" => Some(TypeRef {
            path: vec![canonical_name(&node_text(type_node, source))],
            args: Vec::new(),
            span: Span::from_node(node),
        }),
        "typerefDot" => {
            let lhs = type_node.child_by_field_name("lhs")?;
            let rhs = type_node.child_by_field_name("rhs")?;
            let next_depth = depth.saturating_add(1);
            let mut left = type_ref_from_node_at_depth(lhs, source, next_depth)?;
            let right = type_ref_from_node_at_depth(rhs, source, next_depth)?;
            if !right.args.is_empty() {
                left.args = right.args;
            }
            left.path.extend(right.path);
            left.span = Span::from_node(node);
            Some(left)
        }
        "typerefTpl" | "exprTpl" => {
            let entity = type_node.child_by_field_name("entity")?;
            let next_depth = depth.saturating_add(1);
            let mut result = type_ref_from_node_at_depth(entity, source, next_depth)?;
            let args_node = type_node.child_by_field_name("args")?;
            let args = if matches!(args_node.kind(), "genericArgs" | "typerefArgs" | "exprArgs") {
                (0..args_node.named_child_count())
                    .filter_map(|index| args_node.named_child(index))
                    .filter(|argument| {
                        !argument.is_extra()
                            && !matches!(argument.kind(), "comment" | "kLt" | "kGt")
                    })
                    .map(|arg| type_ref_from_node_at_depth(arg, source, next_depth))
                    .collect::<Option<Vec<_>>>()?
            } else {
                (0..type_node.named_child_count())
                    .filter_map(|index| type_node.named_child(index))
                    .filter(|argument| argument.start_byte() >= args_node.start_byte())
                    .filter(|argument| {
                        !argument.is_extra()
                            && !matches!(argument.kind(), "comment" | "kLt" | "kGt")
                    })
                    .map(|argument| type_ref_from_node_at_depth(argument, source, next_depth))
                    .collect::<Option<Vec<_>>>()?
            };
            if args.is_empty() {
                return None;
            }
            result.args = args;
            result.span = Span::from_node(node);
            Some(result)
        }
        "declString" => Some(TypeRef {
            path: vec!["string".to_owned()],
            args: Vec::new(),
            span: Span::from_node(node),
        }),
        _ => None,
    }
}

fn declaration_name_identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    let Some(name) = node.child_by_field_name("name") else {
        if matches!(node.kind(), "varDef" | "varAssignDef") {
            return (0..node.named_child_count())
                .filter_map(|index| node.named_child(index))
                .find(|child| child.kind() == "identifier")
                .into_iter()
                .collect();
        }
        return Vec::new();
    };
    let mut result = Vec::new();
    collect_name_identifiers(name, &mut result);
    result
}

pub(super) fn is_declaration_identifier(identifier: Node<'_>) -> bool {
    let identifier_span = Span::from_node(identifier);
    let mut current = Some(identifier);
    while let Some(node) = current {
        let named = declaration_name_identifiers(node);
        let field_named = field_identifier_nodes(node, "name");
        if named
            .into_iter()
            .chain(field_named)
            .any(|name| Span::from_node(name) == identifier_span)
        {
            return true;
        }
        current = node.parent();
    }
    false
}

fn enclosing_module_name(identifier: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        if node.kind() == "moduleName" {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

fn is_unit_declaration_module(module_name: Node<'_>) -> bool {
    module_name.kind() == "moduleName"
        && !has_ancestor_kind(module_name, "declUses")
        && has_ancestor_kind(module_name, "unit")
}

fn is_unit_declaration_identifier(identifier: Node<'_>) -> bool {
    enclosing_module_name(identifier).is_some_and(is_unit_declaration_module)
}

fn collect_name_identifiers<'a>(node: Node<'a>, result: &mut Vec<Node<'a>>) {
    match node.kind() {
        "identifier" => result.push(node),
        "genericTpl" => {
            if let Some(entity) = node.child_by_field_name("entity") {
                collect_name_identifiers(entity, result);
            }
        }
        "genericDot" => {
            if let Some(lhs) = node.child_by_field_name("lhs") {
                collect_name_identifiers(lhs, result);
            }
            if let Some(rhs) = node.child_by_field_name("rhs") {
                collect_name_identifiers(rhs, result);
            }
        }
        _ => {}
    }
}

fn generic_template_for_name(node: Node<'_>) -> Option<Node<'_>> {
    let name = node.child_by_field_name("name")?;
    generic_template_in_name(name)
}

fn generic_template_in_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "genericTpl" => Some(node),
        "genericDot" => node
            .child_by_field_name("rhs")
            .and_then(generic_template_in_name),
        _ => None,
    }
}

fn generic_parameters_for_node(node: Node<'_>, source: &str) -> Vec<GenericParameter> {
    let Some(template) = generic_template_for_name(node) else {
        return Vec::new();
    };
    let Some(arguments) = template.child_by_field_name("args") else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for index in 0..arguments.named_child_count() {
        let Some(argument) = arguments.named_child(index) else {
            continue;
        };
        if argument.kind() != "genericArgs" && argument.kind() != "genericArg" {
            continue;
        }
        let groups = if argument.kind() == "genericArgs" {
            (0..argument.named_child_count())
                .filter_map(|child_index| argument.named_child(child_index))
                .collect::<Vec<_>>()
        } else {
            vec![argument]
        };
        for group in groups {
            if group.kind() != "genericArg" {
                continue;
            }
            let constraint_node = generic_constraint_type_node(group);
            let constraint =
                constraint_node.and_then(|type_node| type_ref_from_node(type_node, source));
            let has_constraint_syntax = contains_uncommented_byte(
                source,
                Span {
                    start: group.start_byte(),
                    end: group.end_byte(),
                },
                b':',
            );
            let constraint_unsupported = has_constraint_syntax
                && constraint_node.is_none_or(|type_node| {
                    constraint.is_none()
                        || generic_constraint_has_trailing_tokens(group, type_node, source)
                });
            for identifier in field_identifier_nodes(group, "name") {
                result.push(GenericParameter {
                    name: canonical_name(&node_text(identifier, source)),
                    span: Span::from_node(identifier),
                    constraint: constraint.clone(),
                    constraint_unsupported,
                });
            }
        }
    }
    result
}

fn generic_constraint_has_trailing_tokens(
    group: Node<'_>,
    constraint: Node<'_>,
    source: &str,
) -> bool {
    contains_uncommented_byte(
        source,
        Span {
            start: constraint.end_byte(),
            end: group.end_byte(),
        },
        b',',
    )
}

fn generic_constraint_type_node(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(type_node) = node
        .child_by_field_name("type")
        .filter(|type_node| type_node.kind() != ":")
    {
        return Some(type_node);
    }
    (0..node.named_child_count())
        .filter_map(|index| node.named_child(index))
        .find(|child| {
            matches!(
                child.kind(),
                "typeref" | "typerefDot" | "typerefTpl" | "typerefPtr" | "ppFragmentExpr"
            )
        })
}

fn build_scopes(
    source_len: usize,
    root: Node<'_>,
    definitions: &[Node<'_>],
    source: &str,
) -> (Vec<Scope>, HashMap<Span, usize>) {
    let mut scope_nodes = definitions.to_vec();
    scope_nodes.extend(collect_nodes_matching(root, "lambda"));
    scope_nodes.extend(collect_nodes_matching(root, "block"));
    let mut seeds: Vec<(Span, Option<String>)> = scope_nodes
        .iter()
        .map(|node| {
            let owner_type = node
                .child_by_field_name("header")
                .and_then(|header| routine_name(header, source))
                .and_then(|(_, _, owner)| owner);
            (Span::from_node(*node), owner_type)
        })
        .collect();
    seeds.sort_by_key(|(span, _)| (span.start, std::cmp::Reverse(span.end)));

    let mut scopes = vec![Scope {
        start: 0,
        end: source_len,
        parent: None,
        owner_type: None,
    }];
    let mut open_scopes = vec![ROOT_SCOPE];
    let mut scope_by_span = HashMap::new();
    for (span, owner_type) in seeds {
        while let Some(&open_scope) = open_scopes.last() {
            let open_span = Span {
                start: scopes[open_scope].start,
                end: scopes[open_scope].end,
            };
            if open_span.contains(span) && open_span != span {
                break;
            }
            open_scopes.pop();
        }
        let parent = open_scopes.last().copied().unwrap_or(ROOT_SCOPE);
        let scope = scopes.len();
        let owner_type = owner_type.or_else(|| scopes[parent].owner_type.clone());
        scopes.push(Scope {
            start: span.start,
            end: span.end,
            parent: Some(parent),
            owner_type,
        });
        scope_by_span.insert(span, scope);
        open_scopes.push(scope);
    }
    (scopes, scope_by_span)
}

fn build_scope_binding_keys(
    scopes: &[Scope],
    symbol_indices_by_scope_key: &HashMap<(usize, String), Vec<usize>>,
) -> Vec<HashSet<String>> {
    let mut keys = vec![HashSet::new(); scopes.len()];
    for ((scope, key), indices) in symbol_indices_by_scope_key {
        if *scope != ROOT_SCOPE && !indices.is_empty() {
            keys[*scope].insert(key.clone());
        }
    }
    for scope in 1..scopes.len() {
        let inherited = scopes[scope]
            .parent
            .and_then(|parent| keys.get(parent))
            .cloned()
            .unwrap_or_default();
        keys[scope].extend(inherited);
    }
    keys
}

fn build_member_binding_keys(
    member_symbol_indices: &HashMap<(String, String), Vec<usize>>,
) -> HashMap<String, HashSet<String>> {
    let mut keys_by_owner = HashMap::new();
    for ((owner_type, key), indices) in member_symbol_indices {
        if !indices.is_empty() {
            keys_by_owner
                .entry(owner_type.clone())
                .or_insert_with(HashSet::new)
                .insert(key.clone());
        }
    }
    keys_by_owner
}

fn collect_with_contexts(root: Node<'_>) -> Vec<WithContext> {
    collect_nodes_matching(root, "with")
        .into_iter()
        .filter_map(|node| {
            let body = node.child_by_field_name("body")?;
            let mut cursor = node.walk();
            let receiver_spans = node
                .children_by_field_name("entity", &mut cursor)
                .map(Span::from_node)
                .collect::<Vec<_>>();
            (!receiver_spans.is_empty()).then_some(WithContext {
                body: Span::from_node(body),
                receiver_spans,
            })
        })
        .collect()
}

fn cache_unsafe_scopes(root: Node<'_>, scope_by_span: &HashMap<Span, usize>) -> HashSet<usize> {
    let declarations = ["declConst", "declType", "declVar", "varDef", "varAssignDef"]
        .into_iter()
        .flat_map(|kind| collect_nodes_matching(root, kind))
        .collect::<Vec<_>>();

    let mut unsafe_scopes = HashSet::new();
    for declaration in declarations {
        let mut current = Some(declaration);
        let mut marked = false;
        while let Some(node) = current {
            if let Some(scope) = scope_by_span.get(&Span::from_node(node)).copied() {
                unsafe_scopes.insert(scope);
                marked = true;
                break;
            }
            match node.kind() {
                "lambda" => {
                    // Lambda occurrences are rejected by the per-use check;
                    // they must not disable caching for unrelated root uses.
                    marked = true;
                    break;
                }
                _ => current = node.parent(),
            }
        }
        if !marked {
            // Inline declarations in a program/unit-level block have no
            // distinct scope in the current model, so disable root caching.
            unsafe_scopes.insert(ROOT_SCOPE);
        }
    }
    unsafe_scopes
}

fn scope_for_declaration(node: Node<'_>, scope_by_span: &HashMap<Span, usize>) -> usize {
    let mut current = Some(node);
    while let Some(item) = current {
        if let Some(scope) = scope_by_span.get(&Span::from_node(item)).copied() {
            return scope;
        }
        current = item.parent();
    }
    ROOT_SCOPE
}

fn region_for_node(node: Node<'_>) -> Region {
    let mut current = Some(node);
    while let Some(item) = current {
        match item.kind() {
            "interface" => return Region::Interface,
            "implementation" => return Region::Implementation,
            _ => current = item.parent(),
        }
    }
    Region::Other
}

fn is_routine_declaration_region(region: Region) -> bool {
    matches!(region, Region::Interface | Region::Implementation)
}

fn is_definition_header(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "defProc"
            && parent
                .child_by_field_name("header")
                .is_some_and(|header| Span::from_node(header).contains(Span::from_node(node)))
    })
}

fn conditional_unknown_symbols(
    root: Node<'_>,
    conditionals: &ConditionalAnalysis,
    symbols: &[Symbol],
) -> Vec<bool> {
    let mut declaration_type_spans = HashMap::new();
    collect_nodes(root, &mut |node| {
        if matches!(
            node.kind(),
            "declVar" | "varDef" | "varAssignDef" | "declArg" | "declField" | "declProp"
        ) {
            if let Some(type_node) = node.child_by_field_name("type") {
                declaration_type_spans.insert(Span::from_node(node), Span::from_node(type_node));
            }
        }
    });

    symbols
        .iter()
        .map(|symbol| {
            let symbol_unknown = conditionals.unknown_spans.iter().any(|unknown| {
                unknown.start <= symbol.span.start && symbol.span.end <= unknown.end
            });
            let type_unknown = declaration_type_spans
                .get(&symbol.declaration_span)
                .is_some_and(|type_span| {
                    conditionals.unknown_spans.iter().any(|unknown| {
                        unknown.start < type_span.end && type_span.start < unknown.end
                    })
                });
            let header_unknown = symbol.routine_header_span.is_some_and(|header| {
                conditionals
                    .unknown_spans
                    .iter()
                    .any(|unknown| unknown.start < header.end && header.start < unknown.end)
            });
            symbol_unknown || type_unknown || header_unknown
        })
        .collect()
}

fn collect_nodes_matching<'a>(root: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    collect_nodes(root, &mut |node| {
        if node.kind() == kind {
            result.push(node);
        }
    });
    result
}

fn collect_nodes<'a>(root: Node<'a>, callback: &mut impl FnMut(Node<'a>)) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        callback(node);
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
}

fn collect_parser_recovery_spans(root: Node<'_>) -> Vec<Span> {
    let mut spans = Vec::new();
    collect_nodes(root, &mut |node| {
        if node.is_error() || node.is_missing() {
            spans.push(Span::from_node(node));
        }
    });
    spans
}

fn identifier_nodes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    collect_nodes(node, &mut |child| {
        #[cfg(test)]
        TEST_SEMANTIC_OWNER_HEADER_NODE_VISITS.with(|value| {
            value.set(value.get().saturating_add(1));
        });
        if child.kind() == "identifier" {
            result.push(child);
        }
    });
    result
}

pub(super) fn identifier_nodes_with_budget<'a>(
    root: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    Ok(identifier_nodes_with_budget_and_count(root, cancel, budget)?.0)
}

fn identifier_nodes_with_budget_and_count<'a>(
    root: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(Vec<Node<'a>>, usize), String> {
    let mut cursor = root.walk();
    let mut result = Vec::new();
    let mut visited = 0usize;
    loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        visited = visited.saturating_add(1);
        let node = cursor.node();
        #[cfg(test)]
        TEST_SEMANTIC_OWNER_HEADER_NODE_VISITS.with(|value| {
            value.set(value.get().saturating_add(1));
        });
        if node.kind() == "identifier" {
            let span = Span::from_node(node);
            budget.require_bytes(
                std::mem::size_of::<Node<'_>>().saturating_add(span.end.saturating_sub(span.start)),
                cancel,
            )?;
            result.push(node);
        }

        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok((result, visited));
            }
        }
    }
}

fn field_identifier_nodes<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for field_node in node.children_by_field_name(field, &mut cursor) {
        result.extend(identifier_nodes(field_node));
    }
    result
}

fn identifier_texts(node: Node<'_>, source: &str) -> Vec<String> {
    identifier_nodes(node)
        .into_iter()
        .map(|identifier| node_text(identifier, source))
        .collect()
}

fn qualified_name_parts(node: &Node<'_>, source: &str) -> Option<Vec<String>> {
    let mut pending = vec![*node];
    let mut parts = Vec::new();
    while let Some(current) = pending.pop() {
        match current.kind() {
            "identifier" => parts.push(node_text(current, source)),
            "exprDot" | "genericDot" | "typerefDot" => {
                let lhs = current.child_by_field_name("lhs")?;
                let rhs = current.child_by_field_name("rhs")?;
                pending.push(rhs);
                pending.push(lhs);
            }
            _ => return None,
        }
    }
    Some(parts)
}

fn qualified_path_at<'a>(
    identifier: Node<'a>,
    source: &str,
) -> Option<(Node<'a>, Vec<String>, usize)> {
    let target_span = Span::from_node(identifier);
    let mut current = identifier.parent();
    let mut qualified_node = None;
    while let Some(node) = current {
        if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot")
            && qualified_name_parts(&node, source).is_some_and(|parts| parts.len() > 1)
        {
            qualified_node = Some(node);
        }
        current = node.parent();
    }
    let node = qualified_node?;
    let parts = qualified_name_parts(&node, source)?;
    let cursor_index = identifier_nodes(node)
        .into_iter()
        .position(|candidate| Span::from_node(candidate) == target_span)?;
    Some((node, parts, cursor_index))
}

fn qualified_path_at_with_budget<'a>(
    identifier: Node<'a>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<(Node<'a>, Vec<String>, usize)>, String> {
    let Some(node) = qualified_node_at_with_budget(identifier, cancel, budget)? else {
        return Ok(None);
    };
    let Some(parts) = qualified_name_parts_with_budget(node, source, cancel, budget)?
        .filter(|parts| parts.len() > 1)
    else {
        return Ok(None);
    };
    let Some(cursor_index) =
        qualified_identifier_index_with_budget(node, identifier, cancel, budget)?
    else {
        return Ok(None);
    };
    Ok(Some((node, parts, cursor_index)))
}

fn is_qualified_identifier_node_kind(kind: &str) -> bool {
    matches!(kind, "exprDot" | "genericDot" | "typerefDot")
}

fn is_nested_qualified_identifier_node(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    is_qualified_identifier_node_kind(parent.kind())
        && (parent
            .child_by_field_name("lhs")
            .is_some_and(|lhs| Span::from_node(lhs) == Span::from_node(node))
            || parent
                .child_by_field_name("rhs")
                .is_some_and(|rhs| Span::from_node(rhs) == Span::from_node(node)))
}

/// Compare one top-level, left-associated dotted path with an import spelling.
///
/// The old implementation flattened every nested dot node, causing a path of
/// `n` identifiers to be walked once for each of its `n - 1` dot nodes.  This
/// helper is deliberately linear: it follows the left spine once, then walks
/// back through the same parent chain and examines each right-hand identifier
/// once.  Unsupported shapes and paths beyond the conservative cap are
/// represented by `None`, which callers must treat as unsafe to rewrite.
fn qualified_identifier_matches_spelling_with_budget(
    root: Node<'_>,
    source: &str,
    spelling: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<bool>, String> {
    if !is_qualified_identifier_node_kind(root.kind()) {
        return Ok(None);
    }

    let root_span = Span::from_node(root);
    let mut leftmost = root;
    let mut dot_nodes = 0usize;
    loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        check_navigation_cancel(cancel)?;
        #[cfg(test)]
        test_record_qualified_identifier_node_visit();
        check_navigation_cancel(cancel)?;
        match leftmost.kind() {
            "identifier" => break,
            kind if is_qualified_identifier_node_kind(kind) => {
                if dot_nodes >= MAX_QUALIFIED_IMPORT_ALIAS_PATH_NODES {
                    return Ok(None);
                }
                let Some(lhs) = leftmost.child_by_field_name("lhs") else {
                    return Ok(None);
                };
                if !matches!(
                    lhs.kind(),
                    "identifier" | "exprDot" | "genericDot" | "typerefDot"
                ) {
                    return Ok(None);
                }
                leftmost = lhs;
                dot_nodes = dot_nodes.saturating_add(1);
            }
            _ => return Ok(None),
        }
    }

    let mut expected = spelling.split('.');
    let actual = source
        .get(leftmost.start_byte()..leftmost.end_byte())
        .ok_or_else(|| "qualified import path has an invalid source span".to_string())?;
    budget.require_bytes(actual.len(), cancel)?;
    let Some(expected_part) = expected.next() else {
        return Ok(None);
    };
    if expected_part.is_empty() {
        return Ok(None);
    }
    if !canonical_identifier_eq(actual, expected_part) {
        return Ok(Some(false));
    }

    let mut current = leftmost;
    loop {
        if Span::from_node(current) == root_span {
            break;
        }
        check_navigation_cancel(cancel)?;
        let Some(parent) = current.parent() else {
            return Ok(None);
        };
        if !is_qualified_identifier_node_kind(parent.kind())
            || Span::from_node(parent).start < root_span.start
            || Span::from_node(parent).end > root_span.end
        {
            return Ok(None);
        }
        let Some(lhs) = parent.child_by_field_name("lhs") else {
            return Ok(None);
        };
        if Span::from_node(lhs) != Span::from_node(current) {
            return Ok(None);
        }
        let Some(rhs) = parent.child_by_field_name("rhs") else {
            return Ok(None);
        };
        if rhs.kind() != "identifier" {
            return Ok(None);
        }

        budget.require_work(1, cancel)?;
        check_navigation_cancel(cancel)?;
        #[cfg(test)]
        test_record_qualified_identifier_node_visit();
        check_navigation_cancel(cancel)?;
        let actual = source
            .get(rhs.start_byte()..rhs.end_byte())
            .ok_or_else(|| "qualified import path has an invalid source span".to_string())?;
        budget.require_bytes(actual.len(), cancel)?;
        let Some(expected_part) = expected.next() else {
            return Ok(Some(true));
        };
        if expected_part.is_empty() {
            return Ok(None);
        }
        if !canonical_identifier_eq(actual, expected_part) {
            return Ok(Some(false));
        }
        current = parent;
    }

    Ok(Some(false))
}

fn is_identifier_in_qualified_path(identifier: Node<'_>, _source: &str) -> bool {
    let span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot")
            && Span::from_node(node).contains(span)
        {
            return node
                .child_by_field_name("rhs")
                .is_some_and(|rhs| Span::from_node(rhs).contains(span));
        }
        current = node.parent();
    }
    false
}

fn is_identifier_in_qualified_path_with_budget(
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot")
            && Span::from_node(node).contains(span)
        {
            let is_rhs = node
                .child_by_field_name("rhs")
                .is_some_and(|rhs| Span::from_node(rhs).contains(span));
            return Ok(is_rhs);
        }
        current = node.parent();
    }
    Ok(false)
}

fn contains_uncommented_byte(source: &str, span: Span, needle: u8) -> bool {
    let bytes = source.as_bytes();
    let mut index = span.start.min(bytes.len());
    let end = span.end.min(bytes.len());
    while index < end {
        index = match bytes[index] {
            b'\'' => skip_source_string(bytes, index, end),
            b'{' => skip_source_brace_comment(bytes, index, end),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                skip_source_line_comment(bytes, index, end)
            }
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                skip_source_paren_star_comment(bytes, index, end)
            }
            byte if byte == needle => return true,
            _ => index.saturating_add(1),
        };
    }
    false
}

fn skip_source_string(bytes: &[u8], mut index: usize, end: usize) -> usize {
    index = index.saturating_add(1);
    while index < end {
        if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') && index + 1 < end {
                index = index.saturating_add(2);
            } else {
                return index.saturating_add(1);
            }
        } else {
            index = index.saturating_add(1);
        }
    }
    end
}

fn skip_source_brace_comment(bytes: &[u8], mut index: usize, end: usize) -> usize {
    index = index.saturating_add(1);
    while index < end {
        if bytes[index] == b'}' {
            return index.saturating_add(1);
        }
        index = index.saturating_add(1);
    }
    end
}

fn skip_source_line_comment(bytes: &[u8], mut index: usize, end: usize) -> usize {
    index = index.saturating_add(2);
    while index < end && !matches!(bytes[index], b'\n' | b'\r') {
        index = index.saturating_add(1);
    }
    index
}

fn skip_source_paren_star_comment(bytes: &[u8], mut index: usize, end: usize) -> usize {
    index = index.saturating_add(2);
    while index < end {
        if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b')') && index + 1 < end {
            return index.saturating_add(2);
        }
        index = index.saturating_add(1);
    }
    end
}

fn callable_lookup_identifier(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" => return current,
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return node;
                };
                current = rhs;
            }
            "exprTpl" => {
                let Some(entity) = current.child_by_field_name("entity") else {
                    return node;
                };
                current = entity;
            }
            _ => return node,
        }
    }
}

fn callable_owner_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    loop {
        match current.kind() {
            "exprTpl" => current = current.child_by_field_name("entity")?,
            "exprDot" | "genericDot" | "typerefDot" => {
                return current.child_by_field_name("lhs");
            }
            _ => return None,
        }
    }
}

fn qualified_name_parts_with_budget(
    node: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Vec<String>>, String> {
    budget.require_work(1, cancel)?;
    let mut pending = vec![node];
    let mut parts = Vec::new();
    while let Some(current) = pending.pop() {
        #[cfg(test)]
        test_record_unit_path_nodes(1, cancel);
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match current.kind() {
            "identifier" => {
                let text = node_text_with_budget(current, source, cancel, budget)?;
                budget.require_owned_bytes(
                    text.len().saturating_add(std::mem::size_of::<String>()),
                    cancel,
                )?;
                parts.push(text.to_owned());
            }
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(lhs) = current.child_by_field_name("lhs") else {
                    return Ok(None);
                };
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return Ok(None);
                };
                pending.push(rhs);
                pending.push(lhs);
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(parts))
}

fn matching_unit_prefix_len_with_budget(
    unit_name: &str,
    parts: &[String],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
    if parts.is_empty() {
        return Ok(None);
    }

    // The spelling is already owned by the parsed document. Charge its full
    // source representation before walking it, rather than allocating a
    // normalized component vector and charging after the fact. This makes
    // repeated prefix probes consume the shared request byte budget.
    budget.require_bytes(unit_name.len(), cancel)?;

    let mut unit_part_count = 0;
    for unit_part in unit_name.split('.').filter(|part| !part.is_empty()) {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        check_navigation_cancel(cancel)?;
        if unit_part_count >= parts.len() {
            return Ok(None);
        }
        if !canonical_name_eq_with_cancel(unit_part, &parts[unit_part_count], cancel)? {
            return Ok(None);
        }
        unit_part_count = unit_part_count.saturating_add(1);
        check_navigation_cancel(cancel)?;
    }

    Ok((unit_part_count < parts.len()).then_some(unit_part_count))
}

fn canonical_name_eq_with_cancel(
    left: &str,
    right: &str,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let mut left = left.trim_start_matches('&').chars();
    let mut right = right.trim_start_matches('&').chars();
    loop {
        check_navigation_cancel(cancel)?;
        match (left.next(), right.next()) {
            (Some(left), Some(right)) if left == right => {}
            (Some(left), Some(right))
                if left.is_ascii() && right.is_ascii() && left.eq_ignore_ascii_case(&right) => {}
            (None, None) => return Ok(true),
            _ => return Ok(false),
        }
    }
}

pub(super) fn node_text_with_budget<'a>(
    node: Node<'_>,
    source: &'a str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<&'a str, String> {
    let text = source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default();
    budget.require_bytes(text.len(), cancel)?;
    Ok(text)
}

fn use_name_at_with_budget(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() == "moduleName"
            && has_ancestor_kind_with_budget(node, "declUses", cancel, budget)?
        {
            let span = Span::from_node(node);
            let (identifiers, node_count) =
                identifier_nodes_with_budget_and_count(node, cancel, budget)?;
            budget.require_work(node_count.saturating_mul(3).saturating_add(1), cancel)?;
            budget.require_bytes(
                identifiers
                    .len()
                    .saturating_mul(std::mem::size_of::<String>()),
                cancel,
            )?;
            let names = identifiers
                .into_iter()
                .map(|identifier| {
                    let text = node_text_with_budget(identifier, source, cancel, budget)?;
                    budget.require_owned_bytes(text.len(), cancel)?;
                    Ok(text.to_owned())
                })
                .collect::<Result<Vec<_>, String>>()?;
            let path_bytes = span
                .end
                .saturating_sub(span.start)
                .saturating_add(names.len().saturating_sub(1));
            budget.require_owned_bytes(path_bytes, cancel)?;
            return Ok(Some(canonical_path(&names)));
        }
        current = node.parent();
    }
    Ok(None)
}

fn has_ancestor_kind_with_budget(
    node: Node<'_>,
    kind: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if ancestor.kind() == kind {
            return Ok(true);
        }
        current = ancestor.parent();
    }
    Ok(false)
}

#[cfg(test)]
fn count_nodes_with_budget(
    node: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<usize, String> {
    let mut cursor = node.walk();
    let mut count = 0usize;
    loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        count = count.saturating_add(1);

        if cursor.goto_first_child() {
            #[cfg(test)]
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| {
                entries.set(entries.get().saturating_add(1));
            });
            continue;
        }

        loop {
            if cursor.goto_next_sibling() {
                #[cfg(test)]
                TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| {
                    entries.set(entries.get().saturating_add(1));
                });
                break;
            }
            if !cursor.goto_parent() {
                return Ok(count);
            }
        }
    }
}

fn qualified_type_path_at_with_budget(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<(Vec<String>, usize)>, String> {
    let Some(node) = qualified_node_at_with_budget(identifier, cancel, budget)? else {
        return Ok(None);
    };
    if !matches!(node.kind(), "typerefDot" | "genericDot") {
        return Ok(None);
    }
    let Some(parts) = qualified_name_parts_with_budget(node, source, cancel, budget)?
        .filter(|parts| parts.len() > 1)
    else {
        return Ok(None);
    };
    let Some(cursor_index) =
        qualified_identifier_index_with_budget(node, identifier, cancel, budget)?
    else {
        return Ok(None);
    };
    Ok(Some((parts, cursor_index)))
}

fn qualified_node_at_with_budget<'a>(
    identifier: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let mut current = identifier;
    let mut qualified_node = None;
    while let Some(parent) = current.parent() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !matches!(parent.kind(), "exprDot" | "genericDot" | "typerefDot") {
            break;
        }
        let is_child = parent
            .child_by_field_name("lhs")
            .is_some_and(|lhs| Span::from_node(lhs) == Span::from_node(current))
            || parent
                .child_by_field_name("rhs")
                .is_some_and(|rhs| Span::from_node(rhs) == Span::from_node(current));
        if !is_child {
            break;
        }
        qualified_node = Some(parent);
        current = parent;
    }
    Ok(qualified_node)
}

fn qualified_identifier_index_with_budget(
    node: Node<'_>,
    target: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
    let target_span = Span::from_node(target);
    let mut pending = vec![node];
    let mut index = 0usize;
    while let Some(current) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match current.kind() {
            "identifier" => {
                if Span::from_node(current) == target_span {
                    return Ok(Some(index));
                }
                index = index.saturating_add(1);
            }
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(lhs) = current.child_by_field_name("lhs") else {
                    return Ok(None);
                };
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return Ok(None);
                };
                pending.push(rhs);
                pending.push(lhs);
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn qualified_type_path_at(identifier: Node<'_>, source: &str) -> Option<(Vec<String>, usize)> {
    let mut current = identifier.parent();
    let mut qualified_node = None;
    while let Some(node) = current {
        if matches!(node.kind(), "typerefDot" | "genericDot") {
            let parts = qualified_name_parts(&node, source)?;
            if parts.len() > 1 {
                qualified_node = Some(node);
            }
        }
        current = node.parent();
    }
    let node = qualified_node?;
    let parts = qualified_name_parts(&node, source)?;
    let cursor_index = identifier_nodes(node)
        .into_iter()
        .position(|candidate| Span::from_node(candidate) == Span::from_node(identifier))?;
    Some((parts, cursor_index))
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    (0..node.named_child_count()).find_map(|index| node.named_child(index))
}

fn canonical_name(name: &str) -> String {
    name.trim_start_matches('&').to_ascii_lowercase()
}

fn canonical_identifier_eq(left: &str, right: &str) -> bool {
    left.trim_start_matches('&')
        .eq_ignore_ascii_case(right.trim_start_matches('&'))
}

fn import_binding_fingerprint(
    conditional_context: &ConditionalContext,
    bindings: &HashMap<String, Url>,
) -> u64 {
    let mut entries = bindings.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.0
            .cmp(right.0)
            .then_with(|| left.1.as_str().cmp(right.1.as_str()))
    });
    let mut hasher = DefaultHasher::new();
    conditional_context.fingerprint().hash(&mut hasher);
    for (name, uri) in entries {
        name.hash(&mut hasher);
        uri.hash(&mut hasher);
    }
    hasher.finish()
}

pub(super) fn source_content_hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

fn missing_unit_provider_declaration_fingerprint(symbol: &Symbol, source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    symbol.kind.hash(&mut hasher);
    symbol.origin.hash(&mut hasher);
    symbol.span.start.hash(&mut hasher);
    symbol.span.end.hash(&mut hasher);
    symbol.declaration_span.start.hash(&mut hasher);
    symbol.declaration_span.end.hash(&mut hasher);
    symbol.selection_span.start.hash(&mut hasher);
    symbol.selection_span.end.hash(&mut hasher);
    symbol.name.hash(&mut hasher);
    symbol.type_kind.hash(&mut hasher);
    symbol.type_name.hash(&mut hasher);
    symbol.type_ref.hash(&mut hasher);
    symbol.result_type_name.hash(&mut hasher);
    symbol.result_type_ref.hash(&mut hasher);
    symbol.routine_kind.hash(&mut hasher);
    symbol.routine_signature.hash(&mut hasher);
    source
        .get(symbol.declaration_span.start..symbol.declaration_span.end)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn ancestor_routine_declaration<'a>(
    identifier: Node<'a>,
    declaration_span: Span,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        if node.kind() == "declProc" && Span::from_node(node) == declaration_span {
            if !is_definition_header(node) {
                return Ok(Some(node));
            }
            return Ok(None);
        }
        current = node.parent();
    }
    Ok(None)
}

fn ancestor_declared_type<'a>(
    declaration: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let mut current = declaration.parent();
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        if node.kind() == "declType" {
            return Ok(Some(node));
        }
        current = node.parent();
    }
    Ok(None)
}

fn routine_declaration_node_for_symbol<'a>(
    document: &'a Document,
    symbol: &Symbol,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let Some(identifier) = identifier_at(document.tree.root_node(), symbol.span.start) else {
        return Ok(None);
    };
    ancestor_routine_declaration(identifier, symbol.declaration_span, cancel, budget)
}

fn range_for_span(source: &str, span: Span) -> Option<Range> {
    Some(Range::new(
        text::offset_to_position(source, span.start)?,
        text::offset_to_position(source, span.end)?,
    ))
}

fn replace_method_header_name(header: &str, owner: &str, method: &str) -> Option<String> {
    let prefix = format!("{owner}.");
    let start = header.find(&prefix)?.saturating_add(prefix.len());
    let end = header[start..]
        .find(['(', '<', ';'])
        .map_or(header.len(), |offset| start.saturating_add(offset));
    if start >= end {
        return None;
    }
    let mut result = String::with_capacity(header.len());
    result.push_str(&header[..start]);
    result.push_str(method);
    result.push_str(&header[end..]);
    Some(result)
}

#[allow(clippy::too_many_arguments)]
fn method_declaration_header(
    index: &NavigationIndex,
    symbol: &Symbol,
    node: Node<'_>,
    source: &str,
    source_uri: &Url,
    destination_uri: &Url,
    method_name: &str,
    substitution: &GenericSubstitution,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(None);
    };
    let Some(header_end) = first_uncommented_semicolon(source, node.start_byte(), node.end_byte())
    else {
        return Ok(None);
    };
    if header_end < name.end_byte() {
        return Ok(None);
    }
    check_navigation_cancel(cancel)?;
    if !budget.take_work(1, cancel)? {
        return Ok(None);
    }
    let mut replacements = vec![(name.start_byte(), name.end_byte(), method_name.to_owned())];
    if !append_specialized_routine_replacements(
        index,
        symbol,
        source,
        source_uri,
        destination_uri,
        substitution,
        node.start_byte(),
        header_end.saturating_add(1),
        cancel,
        budget,
        &mut replacements,
    )? {
        return Ok(None);
    }
    if !append_qualified_default_replacements(
        index,
        node,
        source,
        source_uri,
        destination_uri,
        node.start_byte(),
        header_end.saturating_add(1),
        cancel,
        budget,
        &mut replacements,
    )? {
        return Ok(None);
    }
    let Some(mut rendered) = apply_source_replacements(
        source,
        node.start_byte(),
        header_end.saturating_add(1),
        replacements,
    ) else {
        return Ok(None);
    };
    if routine_directives(node).overload {
        rendered.push_str(" overload;");
    }
    if let Some(convention) = routine_calling_convention_keyword(node) {
        rendered.push(' ');
        rendered.push_str(convention);
        rendered.push(';');
    }
    Ok(Some(rendered))
}

#[allow(clippy::too_many_arguments)]
fn append_qualified_default_replacements(
    index: &NavigationIndex,
    node: Node<'_>,
    source: &str,
    source_uri: &Url,
    destination_uri: &Url,
    header_start: usize,
    header_end: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    replacements: &mut Vec<(usize, usize, String)>,
) -> Result<bool, String> {
    let Some(document) = index.documents.get(source_uri) else {
        return Ok(false);
    };
    let Some(arguments) = node.child_by_field_name("args") else {
        return Ok(true);
    };
    for group in direct_routine_argument_groups(arguments) {
        let Some(default) = group.child_by_field_name("defaultValue") else {
            continue;
        };
        let identifiers = collect_identifier_nodes_with_budget(default, cancel, budget)?;
        let mut roots = Vec::new();
        for identifier in identifiers {
            if !identifier_is_dot_rhs_with_budget(identifier, default, cancel, budget)? {
                roots.push(identifier);
            }
        }

        // A declaration and its implementing class can have different
        // lexical scopes even when they live in the same unit.  A named
        // default in the interface can therefore be shadowed by a class
        // member.  Unit qualification is not uniformly legal for every
        // Pascal expression, so refuse non-literal same-unit defaults unless
        // there is no identifier to rebind.
        if source_uri == destination_uri
            && roots.iter().any(|identifier| {
                source
                    .get(identifier.start_byte()..identifier.end_byte())
                    .is_none_or(|name| !builtin_default_value(name))
            })
        {
            return Ok(false);
        }

        if source_uri != destination_uri {
            // Every identifier-rooted reference is resolved independently.
            // Looking only at the first qualified term is unsound for
            // expressions such as `A.Value + B.Value`: the later reference
            // can still bind to a destination-unit declaration.  Rewrite the
            // exact root spans, and fail closed when any root is ambiguous,
            // destination-local, or unsupported.
            for root in &roots {
                check_navigation_cancel(cancel)?;
                let root_span = Span::from_node(*root);
                let Some(name) = source.get(root_span.start..root_span.end) else {
                    return Ok(false);
                };
                if builtin_default_value(name) {
                    continue;
                }
                let mut state = ResolutionState::new();
                let candidates = index.unqualified_references_with_budget_and_state(
                    source_uri,
                    document,
                    root_span.start,
                    name,
                    *root,
                    &mut state,
                    cancel,
                    budget,
                )?;
                if candidates.len() != 1 {
                    return Ok(false);
                }
                let candidate = &candidates[0];
                let Some(symbol) = index.symbol(candidate) else {
                    return Ok(false);
                };
                if candidate.uri == *destination_uri {
                    return Ok(false);
                }
                if symbol.kind == SymbolKind::Unit {
                    // `Provider.TDefaults.Value` already has a stable unit
                    // root.  The remaining member path is deliberately left
                    // intact; it cannot be rebound by a destination-unit
                    // declaration once the unit root is proven.
                    continue;
                }
                if !matches!(
                    symbol.kind,
                    SymbolKind::Constant
                        | SymbolKind::EnumValue
                        | SymbolKind::Routine
                        | SymbolKind::Type
                        | SymbolKind::Variable
                        | SymbolKind::Property
                ) {
                    return Ok(false);
                }
                let Some(unit) = index.unit_display_name(&candidate.uri) else {
                    return Ok(false);
                };
                budget.require_bytes(
                    unit.len().saturating_add(name.len()).saturating_add(1),
                    cancel,
                )?;
                replacements.push((root_span.start, root_span.end, format!("{unit}.{name}")));
            }
        }

        if source_uri == destination_uri {
            for identifier in roots {
                check_navigation_cancel(cancel)?;
                let span = Span::from_node(identifier);
                if span.start < header_start || span.end > header_end {
                    continue;
                }
                let Some(name) = source.get(span.start..span.end) else {
                    continue;
                };
                let mut state = ResolutionState::new();
                let candidates = index.unqualified_references_with_budget_and_state(
                    source_uri, document, span.start, name, identifier, &mut state, cancel, budget,
                )?;
                if candidates.len() != 1 {
                    if builtin_default_value(name) {
                        continue;
                    }
                    return Ok(false);
                }
                let candidate = &candidates[0];
                let Some(symbol) = index.symbol(candidate) else {
                    continue;
                };
                if symbol.kind == SymbolKind::Unit {
                    continue;
                }
                if candidate.uri == *destination_uri
                    || !matches!(
                        symbol.kind,
                        SymbolKind::Constant
                            | SymbolKind::EnumValue
                            | SymbolKind::Routine
                            | SymbolKind::Type
                            | SymbolKind::Variable
                            | SymbolKind::Property
                    )
                {
                    if candidate.uri == *destination_uri {
                        continue;
                    }
                    return Ok(false);
                }
                let Some(unit) = index.unit_display_name(&candidate.uri) else {
                    return Ok(false);
                };
                budget.require_bytes(
                    unit.len().saturating_add(name.len()).saturating_add(1),
                    cancel,
                )?;
                replacements.push((span.start, span.end, format!("{unit}.{name}")));
            }
        }
    }
    Ok(true)
}

fn collect_identifier_nodes_with_budget<'a>(
    root: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    let mut identifiers = Vec::new();
    let mut pending = vec![root];
    while let Some(current) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if current.kind() == "identifier" {
            identifiers.push(current);
        }
        let mut cursor = current.walk();
        pending.extend(current.children(&mut cursor));
    }
    Ok(identifiers)
}

fn identifier_is_dot_rhs_with_budget(
    identifier: Node<'_>,
    boundary: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let identifier_span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.id() == boundary.id() {
            return Ok(false);
        }
        if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot")
            && node
                .child_by_field_name("rhs")
                .is_some_and(|rhs| Span::from_node(rhs).contains(identifier_span))
        {
            return Ok(true);
        }
        current = node.parent();
    }
    Ok(false)
}

fn identifier_is_decl_arg_name_with_budget(
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let Some(parent) = identifier.parent() else {
        return Ok(false);
    };
    if parent.kind() != "declArg" {
        return Ok(false);
    }
    check_navigation_cancel(cancel)?;
    budget.require_work(1, cancel)?;
    Ok(parent
        .child_by_field_name("name")
        .is_some_and(|name| Span::from_node(name).contains(Span::from_node(identifier))))
}

fn builtin_default_value(name: &str) -> bool {
    matches!(canonical_name(name).as_str(), "false" | "nil" | "true")
}

fn class_member_insertion(
    class_node: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<InterfaceDeclarationInsertion>, String> {
    let mut cursor = class_node.walk();
    let children = class_node.children(&mut cursor).collect::<Vec<_>>();
    budget.require_work(children.len(), cancel)?;
    let Some(end_node) = children
        .iter()
        .copied()
        .find(|child| child.kind() == "kEnd")
    else {
        return Ok(None);
    };
    let public_section = children
        .iter()
        .copied()
        .find(|child| child.kind() == "declSection" && has_direct_child_kind(*child, "kPublic"));
    let target = if let Some(section) = public_section {
        let Some(section_index) = children
            .iter()
            .position(|child| Span::from_node(*child) == Span::from_node(section))
        else {
            return Ok(None);
        };
        children
            .iter()
            .skip(section_index.saturating_add(1))
            .copied()
            .find(|child| matches!(child.kind(), "declSection" | "kEnd"))
            .unwrap_or(end_node)
    } else {
        end_node
    };
    let line_start = source[..target.start_byte()]
        .rfind('\n')
        .map_or(0, |offset| offset.saturating_add(1));
    let Some(prefix) = source.get(line_start..target.start_byte()) else {
        return Ok(None);
    };
    if !prefix.trim().is_empty() {
        return Ok(None);
    }
    let owner_indent = prefix.to_owned();
    if let Some(section) = public_section {
        let section_line_start = source[..section.start_byte()]
            .rfind('\n')
            .map_or(0, |offset| offset.saturating_add(1));
        let Some(section_prefix) = source.get(section_line_start..section.start_byte()) else {
            return Ok(None);
        };
        let section_indent = section_prefix.to_owned();
        let member_indent = first_section_member_indent(section, source)
            .unwrap_or_else(|| format!("{section_indent}  "));
        return Ok(Some(InterfaceDeclarationInsertion {
            start: line_start,
            end: target.start_byte(),
            indent: member_indent,
            owner_indent,
            add_public: false,
        }));
    }
    Ok(Some(InterfaceDeclarationInsertion {
        start: line_start,
        end: target.start_byte(),
        indent: format!("{owner_indent}  "),
        owner_indent: owner_indent.clone(),
        add_public: true,
    }))
}

fn first_section_member_indent(section: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = section.walk();
    section.children(&mut cursor).find_map(|child| {
        if !matches!(
            child.kind(),
            "declProc" | "declField" | "declProp" | "declConst" | "declType"
        ) {
            return None;
        }
        let line_start = source[..child.start_byte()]
            .rfind('\n')
            .map_or(0, |offset| offset.saturating_add(1));
        let prefix = source.get(line_start..child.start_byte())?;
        prefix.trim().is_empty().then(|| prefix.to_owned())
    })
}

fn declared_type_qualification(
    node: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    let target = Span::from_node(node);
    let mut names = Vec::new();
    let mut current = Some(node);
    while let Some(candidate) = current {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        let is_owner = Span::from_node(candidate) == target;
        let encloses_owner = candidate
            .child_by_field_name("type")
            .is_some_and(|type_node| Span::from_node(type_node).contains(target));
        if candidate.kind() == "declType" && (is_owner || encloses_owner) {
            let Some(name_node) = candidate.child_by_field_name("name") else {
                return Ok(None);
            };
            let Some(name) = reference_name_with_budget(name_node, source, cancel, budget)? else {
                return Ok(None);
            };
            names.push(name);
        }
        current = candidate.parent();
    }
    if names.is_empty() {
        return Ok(None);
    }
    names.reverse();
    let qualification = names.join(".");
    if !budget.take_bytes(qualification.len(), cancel)? {
        return Ok(None);
    }
    Ok(Some(qualification))
}

fn type_node_contains_kind_with_budget(
    node: Node<'_>,
    kind: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<bool>, String> {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        if current.kind() == kind {
            return Ok(Some(true));
        }
        let mut cursor = current.walk();
        pending.extend(current.children(&mut cursor));
    }
    Ok(Some(false))
}

fn routine_declaration_is_external(node: Node<'_>) -> bool {
    let mut external = false;
    collect_nodes(node, &mut |child| {
        if matches!(child.kind(), "procExternal" | "kExternal") {
            external = true;
        }
    });
    external
}

fn method_signature_is_supported(symbol: &Symbol) -> bool {
    if symbol
        .generic_parameters
        .iter()
        .any(|parameter| parameter.constraint_unsupported)
    {
        return false;
    }
    if symbol.routine_parameters.iter().any(|parameter| {
        parameter
            .type_shape
            .as_ref()
            .is_none_or(|shape| !method_type_shape_is_supported(shape))
    }) {
        return false;
    }
    match symbol.routine_kind {
        RoutineKind::Function => symbol
            .result_type_ref
            .as_ref()
            .is_some_and(method_type_ref_is_supported),
        RoutineKind::Procedure | RoutineKind::Constructor | RoutineKind::Destructor => {
            symbol.result_type_ref.is_none()
        }
        RoutineKind::Operator => false,
    }
}

fn method_type_shape_is_supported(shape: &TypeShape) -> bool {
    match shape {
        TypeShape::Named(type_ref) => method_type_ref_is_supported(type_ref),
        TypeShape::Pointer(element) => method_type_shape_is_supported(element),
        TypeShape::Array { element, dynamic } => {
            *dynamic && method_type_shape_is_supported(element)
        }
        TypeShape::Callable => true,
        TypeShape::Unknown => false,
    }
}

fn method_type_ref_is_supported(type_ref: &TypeRef) -> bool {
    !type_ref.path.is_empty() && type_ref.args.iter().all(method_type_ref_is_supported)
}

fn method_definition_proof(
    document: &Document,
    declaration: &Symbol,
    declaration_index: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<ContractMatch, String> {
    let mut uncertain = false;
    for (index, definition) in document.symbols.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(ContractMatch::Unknown);
        }
        if index == declaration_index
            || definition.kind != SymbolKind::Routine
            || definition.origin != Origin::Definition
            || definition.owner_type != declaration.owner_type
            || definition.scope != declaration.scope
            || definition.key != declaration.key
            || definition.is_static != declaration.is_static
        {
            continue;
        }
        let definition_span = definition
            .routine_header_span
            .unwrap_or(definition.declaration_span);
        let definition_unknown = document
            .conditional_unknown_symbols
            .get(index)
            .copied()
            .unwrap_or(true)
            || document.has_parser_recovery_near(definition_span);
        if definition.routine_kind != declaration.routine_kind {
            uncertain = true;
            continue;
        }
        if definition.routine_key == declaration.routine_key {
            if definition_unknown {
                uncertain = true;
                continue;
            }
            match missing_unit_routine_implementation_status(declaration, definition) {
                ContractMatch::Yes => return Ok(ContractMatch::Yes),
                ContractMatch::Unknown => uncertain = true,
                ContractMatch::No => {}
            }
            continue;
        }

        // A deliberately abbreviated implementation is only safe after the
        // shared pairing pass has selected one declaration. If it remains
        // unresolved, or if its signature contains an unsupported shape, it
        // can represent the selected overload and must block generation.
        if definition.unresolved_abbreviated
            || definition.routine_signature.is_none()
            || definition
                .routine_signature
                .as_deref()
                .is_some_and(|signature| {
                    signature
                        .split(',')
                        .any(|part| part.trim_end().ends_with('?'))
                })
        {
            uncertain = true;
        } else if !declaration.routine_directives.overload
            || !definition.routine_directives.overload
        {
            // A known but differently-shaped implementation with the same
            // owner/name is still a collision for a non-overload declaration.
            // Do not append a second body unless the source explicitly proves
            // that both routines belong to one overload group.
            uncertain = true;
        }
    }
    Ok(if uncertain {
        ContractMatch::Unknown
    } else {
        ContractMatch::No
    })
}

fn method_implementation_insertion_offset(
    document: &Document,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
    let root = document.tree.root_node();
    let unit = if root.kind() == "unit" {
        root
    } else {
        let mut unit = None;
        for index in 0..root.named_child_count() {
            check_navigation_cancel(cancel)?;
            if !budget.take_work(1, cancel)? {
                return Ok(None);
            }
            if let Some(child) = root.named_child(index) {
                if child.kind() == "unit" {
                    unit = Some(child);
                    break;
                }
            }
        }
        let Some(unit) = unit else {
            return Ok(None);
        };
        unit
    };
    let mut implementation_seen = false;
    for index in 0..unit.named_child_count() {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        let Some(child) = unit.named_child(index) else {
            return Ok(None);
        };
        if child.kind() == "implementation" {
            implementation_seen = true;
            continue;
        }
        if implementation_seen && matches!(child.kind(), "initialization" | "finalization" | "kEnd")
        {
            return Ok(Some(child.start_byte()));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn method_implementation_header(
    index: &NavigationIndex,
    symbol: &Symbol,
    node: Node<'_>,
    source: &str,
    source_uri: &Url,
    destination_uri: &Url,
    owner_name: &str,
    substitution: &GenericSubstitution,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(None);
    };
    let Some(method_name) = reference_name_with_budget(name, source, cancel, budget)? else {
        return Ok(None);
    };
    let Some(header_end) = first_uncommented_semicolon(source, node.start_byte(), node.end_byte())
    else {
        return Ok(None);
    };
    if header_end < name.end_byte() {
        return Ok(None);
    }

    let mut replacements = vec![(
        name.start_byte(),
        name.end_byte(),
        format!("{owner_name}.{method_name}"),
    )];
    if let Some(arguments) = node.child_by_field_name("args") {
        for group in direct_routine_argument_groups(arguments) {
            let Some(default) = group.child_by_field_name("defaultValue") else {
                continue;
            };
            let mut start = default.start_byte();
            while start > group.start_byte()
                && source
                    .as_bytes()
                    .get(start.saturating_sub(1))
                    .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
            {
                start = start.saturating_sub(1);
            }
            replacements.push((start, default.end_byte(), String::new()));
        }
    }
    if !append_specialized_routine_replacements(
        index,
        symbol,
        source,
        source_uri,
        destination_uri,
        substitution,
        node.start_byte(),
        header_end.saturating_add(1),
        cancel,
        budget,
        &mut replacements,
    )? {
        return Ok(None);
    }
    let Some(mut rendered) = apply_source_replacements(
        source,
        node.start_byte(),
        header_end.saturating_add(1),
        replacements,
    ) else {
        return Ok(None);
    };

    if let Some(convention) = routine_calling_convention_keyword(node) {
        rendered.push(' ');
        rendered.push_str(convention);
        rendered.push(';');
    }
    Ok(Some(rendered))
}

#[allow(clippy::too_many_arguments)]
fn append_specialized_routine_replacements(
    index: &NavigationIndex,
    symbol: &Symbol,
    source: &str,
    source_uri: &Url,
    destination_uri: &Url,
    substitution: &GenericSubstitution,
    header_start: usize,
    header_end: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    replacements: &mut Vec<(usize, usize, String)>,
) -> Result<bool, String> {
    for parameter in &symbol.routine_parameters {
        let Some(span) = parameter.type_span else {
            continue;
        };
        if span.start < header_start || span.end > header_end {
            continue;
        }
        let rendered = assistance::specialized_type_text(
            index,
            symbol,
            source,
            Some(source_uri),
            Some(destination_uri),
            parameter.type_ref.as_ref(),
            substitution,
            cancel,
            budget,
        )?;
        match rendered {
            assistance::SpecializedTypeText::Unchanged => {}
            assistance::SpecializedTypeText::Replaced(text) => {
                replacements.push((span.start, span.end, text));
            }
            assistance::SpecializedTypeText::Unsupported => return Ok(false),
        }
    }
    if let (Some(span), Some(type_ref)) = (symbol.result_type_span, symbol.result_type_ref.as_ref())
    {
        if span.start >= header_start && span.end <= header_end {
            match assistance::specialized_type_text(
                index,
                symbol,
                source,
                Some(source_uri),
                Some(destination_uri),
                Some(type_ref),
                substitution,
                cancel,
                budget,
            )? {
                assistance::SpecializedTypeText::Unchanged => {}
                assistance::SpecializedTypeText::Replaced(text) => {
                    replacements.push((span.start, span.end, text));
                }
                assistance::SpecializedTypeText::Unsupported => return Ok(false),
            }
        }
    }
    let _ = source;
    Ok(true)
}

fn apply_source_replacements(
    source: &str,
    start: usize,
    end: usize,
    mut replacements: Vec<(usize, usize, String)>,
) -> Option<String> {
    replacements.sort_by_key(|replacement| (replacement.0, replacement.1));
    for pair in replacements.windows(2) {
        if pair[0].1 > pair[1].0 || pair[0].0 < start || pair[1].1 > end {
            return None;
        }
    }
    if replacements
        .first()
        .is_some_and(|replacement| replacement.0 < start || replacement.1 > end)
    {
        return None;
    }
    let mut rendered = String::new();
    let mut cursor = start;
    for (replacement_start, replacement_end, replacement) in replacements {
        rendered.push_str(source.get(cursor..replacement_start)?);
        rendered.push_str(&replacement);
        cursor = replacement_end;
    }
    rendered.push_str(source.get(cursor..end)?);
    Some(rendered)
}

fn reference_name_with_budget(
    node: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    check_navigation_cancel(cancel)?;
    if !budget.take_work(1, cancel)? {
        return Ok(None);
    }
    match node.kind() {
        "identifier" => {
            let Some(name) = source.get(node.start_byte()..node.end_byte()) else {
                return Ok(None);
            };
            if name.is_empty() || !budget.take_bytes(name.len(), cancel)? {
                return Ok(None);
            }
            Ok(Some(name.to_owned()))
        }
        "genericTpl" => {
            let Some(entity) = node.child_by_field_name("entity") else {
                return Ok(None);
            };
            let Some(arguments) = node.child_by_field_name("args") else {
                return Ok(None);
            };
            let Some(entity) = reference_name_with_budget(entity, source, cancel, budget)? else {
                return Ok(None);
            };
            let Some(parameters) =
                generic_reference_parameters_with_budget(arguments, source, cancel, budget)?
            else {
                return Ok(None);
            };
            let mut result = entity;
            result.push('<');
            result.push_str(&parameters.join(", "));
            result.push('>');
            if !budget.take_bytes(result.len(), cancel)? {
                return Ok(None);
            }
            Ok(Some(result))
        }
        "genericDot" => {
            let Some(lhs) = node.child_by_field_name("lhs") else {
                return Ok(None);
            };
            let Some(rhs) = node.child_by_field_name("rhs") else {
                return Ok(None);
            };
            let Some(lhs) = reference_name_with_budget(lhs, source, cancel, budget)? else {
                return Ok(None);
            };
            let Some(rhs) = reference_name_with_budget(rhs, source, cancel, budget)? else {
                return Ok(None);
            };
            let result = format!("{lhs}.{rhs}");
            if !budget.take_bytes(result.len(), cancel)? {
                return Ok(None);
            }
            Ok(Some(result))
        }
        _ => Ok(None),
    }
}

fn generic_reference_parameters_with_budget(
    arguments: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Vec<String>>, String> {
    let mut parameters = Vec::new();
    for index in 0..arguments.named_child_count() {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        let Some(argument) = arguments.named_child(index) else {
            return Ok(None);
        };
        match argument.kind() {
            "genericArg" => {
                if !append_generic_reference_group(
                    argument,
                    source,
                    cancel,
                    budget,
                    &mut parameters,
                )? {
                    return Ok(None);
                }
            }
            "genericArgs" => {
                for child_index in 0..argument.named_child_count() {
                    check_navigation_cancel(cancel)?;
                    if !budget.take_work(1, cancel)? {
                        return Ok(None);
                    }
                    let Some(group) = argument.named_child(child_index) else {
                        return Ok(None);
                    };
                    if group.kind() != "genericArg"
                        || !append_generic_reference_group(
                            group,
                            source,
                            cancel,
                            budget,
                            &mut parameters,
                        )?
                    {
                        return Ok(None);
                    }
                }
            }
            "comment" => {}
            _ => return Ok(None),
        }
    }
    if parameters.is_empty() {
        return Ok(None);
    }
    Ok(Some(parameters))
}

fn append_generic_reference_group(
    group: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    parameters: &mut Vec<String>,
) -> Result<bool, String> {
    if !generic_reference_group_is_supported(group, source) {
        return Ok(false);
    }
    let names = field_identifier_nodes(group, "name");
    if names.is_empty() {
        return Ok(false);
    }
    for name_node in names {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            return Ok(false);
        }
        let Some(name) = source.get(name_node.start_byte()..name_node.end_byte()) else {
            return Ok(false);
        };
        if name.is_empty() || !budget.take_bytes(name.len(), cancel)? {
            return Ok(false);
        }
        parameters.push(name.to_owned());
    }
    Ok(true)
}

fn generic_reference_group_is_supported(group: Node<'_>, source: &str) -> bool {
    let Some(constraint_node) = generic_constraint_type_node(group) else {
        return !contains_uncommented_byte(
            source,
            Span {
                start: group.start_byte(),
                end: group.end_byte(),
            },
            b':',
        );
    };
    let has_constraint_syntax = contains_uncommented_byte(
        source,
        Span {
            start: group.start_byte(),
            end: group.end_byte(),
        },
        b':',
    );
    !has_constraint_syntax
        || (type_ref_from_node(constraint_node, source).is_some()
            && !generic_constraint_has_trailing_tokens(group, constraint_node, source))
}

fn routine_calling_convention_keyword(node: Node<'_>) -> Option<&'static str> {
    let mut convention = None;
    collect_nodes(node, &mut |child| {
        convention = convention.or(match child.kind() {
            "kCdecl" => Some("cdecl"),
            "kStdcall" => Some("stdcall"),
            "kPascal" => Some("pascal"),
            "kRegister" => Some("register"),
            "kSafecall" => Some("safecall"),
            _ => None,
        });
    });
    convention
}

fn first_uncommented_semicolon(source: &str, start: usize, end: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start.min(bytes.len());
    let end = end.min(bytes.len());
    let mut parentheses = 0usize;
    while index < end {
        index = match bytes[index] {
            b'\'' => skip_source_string(bytes, index, end),
            b'{' => skip_source_brace_comment(bytes, index, end),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                skip_source_line_comment(bytes, index, end)
            }
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                skip_source_paren_star_comment(bytes, index, end)
            }
            b'(' => {
                parentheses = parentheses.saturating_add(1);
                index.saturating_add(1)
            }
            b')' => {
                parentheses = parentheses.saturating_sub(1);
                index.saturating_add(1)
            }
            b';' if parentheses == 0 => return Some(index),
            _ => index.saturating_add(1),
        };
    }
    None
}

/// Establish the relation that the resolver's routine key is intended to
/// represent.  A key is useful for finding declaration/definition pairs, but
/// it is not by itself a proof: unsupported parameter syntax can collapse
/// distinct overloads onto the same key.  Missing-unit actions need a proof
/// for every returned candidate, so unknown identity is deliberately not a
/// match.
fn missing_unit_routine_implementation_status(
    declaration: &Symbol,
    definition: &Symbol,
) -> ContractMatch {
    if declaration.kind != SymbolKind::Routine
        || definition.kind != SymbolKind::Routine
        || declaration.origin != Origin::Declaration
        || definition.origin != Origin::Definition
        || declaration.key != definition.key
        || declaration.scope != definition.scope
        || declaration.owner_type != definition.owner_type
        || declaration.routine_kind != definition.routine_kind
        || declaration.is_static != definition.is_static
        || declaration.routine_key != definition.routine_key
    {
        return ContractMatch::No;
    }
    if declaration.routine_directives.calling_convention_unknown
        || definition.routine_directives.calling_convention_unknown
    {
        return ContractMatch::Unknown;
    }
    match (
        declaration.routine_directives.calling_convention,
        definition.routine_directives.calling_convention,
    ) {
        (Some(left), Some(right)) if left != right => return ContractMatch::No,
        (None, Some(_)) | (Some(_), None) => return ContractMatch::Unknown,
        _ => {}
    }

    if declaration.generic_parameters.len() != definition.generic_parameters.len() {
        return ContractMatch::No;
    }
    for (left, right) in declaration
        .generic_parameters
        .iter()
        .zip(&definition.generic_parameters)
    {
        if left.constraint_unsupported || right.constraint_unsupported {
            return ContractMatch::Unknown;
        }
        if generic_parameter_shape(left) != generic_parameter_shape(right) {
            return ContractMatch::No;
        }
    }

    // An abbreviated implementation is paired by the shared resolver only
    // when exactly one declaration can own it.  Its synthetic parameters are
    // materialized separately, so use that established pair rather than
    // treating the deliberately abbreviated header as an empty signature.
    let abbreviated_definition = definition.routine_signature.as_deref() == Some("");
    if !abbreviated_definition {
        if declaration.routine_parameters.len() != definition.routine_parameters.len() {
            return ContractMatch::No;
        }
        for (left, right) in declaration
            .routine_parameters
            .iter()
            .zip(&definition.routine_parameters)
        {
            if left.mode != right.mode {
                return ContractMatch::No;
            }
            match missing_unit_type_shape_match(left.type_shape.as_ref(), right.type_shape.as_ref())
            {
                ContractMatch::Yes => {}
                other => return other,
            }
        }
    }

    match declaration.routine_kind {
        RoutineKind::Function | RoutineKind::Operator => {
            let (Some(left), Some(right)) = (
                declaration.result_type_ref.as_ref(),
                definition.result_type_ref.as_ref(),
            ) else {
                return ContractMatch::Unknown;
            };
            if !missing_unit_type_ref_equal(left, right) {
                return ContractMatch::No;
            }
        }
        RoutineKind::Procedure | RoutineKind::Constructor | RoutineKind::Destructor => {
            if declaration.result_type_ref.is_some() != definition.result_type_ref.is_some() {
                return ContractMatch::No;
            }
        }
    }
    ContractMatch::Yes
}

fn missing_unit_type_shape_match(
    left: Option<&TypeShape>,
    right: Option<&TypeShape>,
) -> ContractMatch {
    let (Some(left), Some(right)) = (left, right) else {
        return ContractMatch::Unknown;
    };
    match (left, right) {
        (TypeShape::Named(left), TypeShape::Named(right)) => {
            if missing_unit_type_ref_equal(left, right) {
                ContractMatch::Yes
            } else {
                ContractMatch::No
            }
        }
        (TypeShape::Pointer(left), TypeShape::Pointer(right)) => {
            missing_unit_type_shape_match(Some(left), Some(right))
        }
        (
            TypeShape::Array {
                element: left,
                dynamic: left_dynamic,
            },
            TypeShape::Array {
                element: right,
                dynamic: right_dynamic,
            },
        ) if left_dynamic == right_dynamic && *left_dynamic => {
            missing_unit_type_shape_match(Some(left), Some(right))
        }
        (TypeShape::Array { dynamic: false, .. }, TypeShape::Array { dynamic: false, .. }) => {
            // The indexed shape intentionally does not retain static bounds.
            ContractMatch::Unknown
        }
        (TypeShape::Callable, TypeShape::Callable)
        | (TypeShape::Unknown, _)
        | (_, TypeShape::Unknown) => ContractMatch::Unknown,
        _ => ContractMatch::No,
    }
}

fn missing_unit_type_ref_equal(left: &TypeRef, right: &TypeRef) -> bool {
    left.path == right.path
        && left.args.len() == right.args.len()
        && left
            .args
            .iter()
            .zip(&right.args)
            .all(|(left, right)| missing_unit_type_ref_equal(left, right))
}

fn canonical_path(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| canonical_name(part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn node_text(node: Node<'_>, source: &str) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .to_string()
}

fn fallback_unit_name(uri: &Url) -> String {
    uri.to_file_path()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            uri.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|name| name.trim_end_matches(".pas").to_owned())
        })
        .map_or_else(|| "<anonymous>".to_string(), |name| canonical_name(&name))
}

fn has_ancestor_kind(node: Node<'_>, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn identifier_at(root: Node<'_>, offset: usize) -> Option<Node<'_>> {
    if !Span::from_node(root).contains_offset(offset) {
        return None;
    }

    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if !Span::from_node(node).contains_offset(offset) {
            continue;
        }
        if node.kind() == "identifier" {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    None
}

fn declared_type_node_for_span_with_budget<'a>(
    root: Node<'a>,
    span: Span,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let Some(identifier) = identifier_at_with_budget(root, span.start, cancel, budget)? else {
        return Ok(None);
    };
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() == "declType"
            && node
                .child_by_field_name("name")
                .is_some_and(|name| Span::from_node(name).contains(span))
        {
            return Ok(Some(node));
        }
        current = node.parent();
    }
    Ok(None)
}

fn declaration_node_for_symbol_with_budget<'a>(
    document: &'a Document,
    symbol: &Symbol,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let Some(identifier) =
        identifier_at_with_budget(document.tree.root_node(), symbol.span.start, cancel, budget)?
    else {
        return Ok(None);
    };
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if Span::from_node(node) == symbol.declaration_span
            && matches!(node.kind(), "declConst" | "declEnumValue")
        {
            return Ok(Some(node));
        }
        current = node.parent();
    }
    Ok(None)
}

fn identifier_at_with_budget<'a>(
    root: Node<'a>,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    if !Span::from_node(root).contains_offset(offset) {
        return Ok(None);
    }

    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !Span::from_node(node).contains_offset(offset) {
            continue;
        }
        if node.kind() == "identifier" {
            return Ok(Some(node));
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    Ok(None)
}

pub(super) fn is_ignored_offset(root: Node<'_>, offset: usize) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if !Span::from_node(node).contains_offset(offset) {
            continue;
        }
        if matches!(node.kind(), "comment" | "literalString" | "literalChar") {
            return true;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    false
}

fn is_non_value_identifier_with_budget(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    is_non_value_identifier_with_budget_and_declaration(
        identifier,
        source,
        cancel,
        budget,
        is_declaration_identifier(identifier),
    )
}

pub(super) fn is_non_value_identifier_with_budget_and_declaration(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    declaration: bool,
) -> Result<bool, String> {
    let name = canonical_name(node_text_with_budget(identifier, source, cancel, budget)?);
    if declaration
        || is_implicit_or_intrinsic_name(&name)
        || is_type_valued_intrinsic_argument_with_budget(identifier, source, cancel, budget)?
    {
        return Ok(true);
    }
    let span = Span::from_node(identifier);
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(
            node.kind(),
            "asm"
                | "declExport"
                | "declExports"
                | "declHelper"
                | "declLabels"
                | "inherited"
                | "label"
                | "goto"
                | "procAttribute"
                | "rttiAttributes"
        ) {
            return Ok(true);
        }
        if node.kind().starts_with("typeref")
            || matches!(node.kind(), "type" | "genericArg" | "genericArgs")
        {
            return Ok(true);
        }
        if node.kind() == "declProp" {
            return Ok(true);
        }
        if node
            .child_by_field_name("type")
            .is_some_and(|type_node| Span::from_node(type_node).contains(span))
            || node
                .child_by_field_name("parent")
                .is_some_and(|parent| Span::from_node(parent).contains(span))
        {
            return Ok(true);
        }
        if node.kind() == "exprBinary"
            && node
                .child_by_field_name("lhs")
                .is_some_and(|lhs| Span::from_node(lhs).contains(span))
            && node
                .child_by_field_name("operator")
                .is_some_and(|operator| node_text(operator, source) == ":=")
            && has_ancestor_kind_with_budget(node, "exprCall", cancel, budget)?
        {
            return Ok(true);
        }
        current = node.parent();
    }
    Ok(false)
}

fn is_implicit_or_intrinsic_name(name: &str) -> bool {
    matches!(
        name,
        "self" | "result" | "inherited" | "exit" | "break" | "continue" | "raise"
    )
}

fn is_implicit_tobject_member(name: &str) -> bool {
    matches!(
        name,
        "afterconstruction"
            | "beforedestruction"
            | "cleanupinstance"
            | "classinfo"
            | "classname"
            | "classnameis"
            | "classparent"
            | "classtype"
            | "create"
            | "defaultdie"
            | "defaulthandler"
            | "destroy"
            | "dispatch"
            | "equals"
            | "fieldaddress"
            | "free"
            | "freeinstance"
            | "gethashcode"
            | "getinterface"
            | "getinterfaceentry"
            | "getinterfacetable"
            | "inheritsfrom"
            | "initinstance"
            | "instance_size"
            | "instancesize"
            | "methodaddress"
            | "methodname"
            | "newinstance"
            | "qualifiedclassname"
            | "safecallexception"
            | "tostring"
            | "unitname"
            | "unitscope"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImplicitSystemNamespaceStatus {
    Unavailable,
    Ambiguous,
    /// A source-backed System unit can provide positive symbol evidence, but
    /// it is not treated as a complete catalogue of compiler-provided names.
    SourceBacked {
        uri: Url,
    },
}

fn is_type_valued_intrinsic_argument_with_budget(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let Some(builtin) =
        overload::builtin_type(node_text_with_budget(identifier, source, cancel, budget)?)
    else {
        return Ok(false);
    };
    let _ = builtin;
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() == "exprCall" {
            let Some(entity) = node.child_by_field_name("entity") else {
                return Ok(false);
            };
            let Some(parts) = qualified_name_parts_with_budget(entity, source, cancel, budget)?
            else {
                return Ok(false);
            };
            let Some(name) = parts.last() else {
                return Ok(false);
            };
            let call_name = canonical_path(&parts);
            return Ok(overload::builtin_type(&call_name).is_some()
                || matches!(
                    canonical_name(name).as_str(),
                    "default" | "high" | "ismanagedtype" | "low" | "sizeof" | "typeinfo"
                ));
        }
        if matches!(node.kind(), "assignment" | "block" | "statements") {
            return Ok(false);
        }
        current = node.parent();
    }
    Ok(false)
}

pub(super) fn use_name_at(identifier: Node<'_>, source: &str) -> Option<String> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        if node.kind() == "moduleName" && has_ancestor_kind(node, "declUses") {
            return Some(canonical_path(&identifier_texts(node, source)));
        }
        current = node.parent();
    }
    None
}

pub(super) fn member_expression_at(identifier: Node<'_>) -> Option<Node<'_>> {
    let mut current = identifier.parent();
    while let Some(node) = current {
        if node.kind() == "exprDot" {
            return Some(node);
        }
        if matches!(node.kind(), "assignment" | "block" | "statements") {
            return None;
        }
        current = node.parent();
    }
    None
}

fn is_right_hand_member(dot: Node<'_>, identifier: Node<'_>) -> bool {
    dot.child_by_field_name("rhs")
        .is_some_and(|rhs| Span::from_node(rhs).contains(Span::from_node(identifier)))
}

fn location_for_span(uri: &Url, source: &str, span: Span) -> Option<Location> {
    let start = text::offset_to_position(source, span.start)?;
    let end = text::offset_to_position(source, span.end)?;
    Some(Location {
        uri: uri.clone(),
        range: Range { start, end },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn repeated_class_field_source(repetitions: usize) -> String {
        let mut source = String::from(
            "unit RepeatedClassField;\ninterface\ntype\n  TFirst = class\n    FValue: Integer;\n    procedure Touch;\n  end;\nimplementation\nprocedure TFirst.Touch;\nbegin\n",
        );
        for index in 0..repetitions {
            writeln!(&mut source, "  FValue := {index};").expect("write fixture source");
        }
        source.push_str("end;\nend.\n");
        source
    }

    fn query_repeated_class_field(repetitions: usize) -> (usize, usize) {
        let source = repeated_class_field_source(repetitions);
        let uri = Url::parse("file:///tmp/repeated-class-field.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("class-field fixture parses");

        OWNER_TYPE_ROOT_LOOKUPS.with(|lookups| lookups.set(0));
        let locations = index
            .binding_locations(&uri, Position::new(4, 4), true)
            .expect("class-field references resolve");
        let root_lookups = OWNER_TYPE_ROOT_LOOKUPS.with(Cell::get);
        (locations.len(), root_lookups)
    }

    fn duplicate_provider_rebind_fixture(
        bound_provider: Option<usize>,
    ) -> (NavigationIndex, Url, Url, Url, String) {
        let main = Url::parse("file:///tmp/rebind-imports/Main.pas").expect("main URI");
        let provider_a =
            Url::parse("file:///tmp/rebind-imports/A/Shared.pas").expect("provider A URI");
        let provider_b =
            Url::parse("file:///tmp/rebind-imports/B/Shared.pas").expect("provider B URI");
        let source = concat!(
            "unit Main;\n",
            "interface\n",
            "uses Shared;\n",
            "implementation\n",
            "procedure Work;\n",
            "var my_var: Integer;\n",
            "begin my_var := 1; end;\n",
            "procedure Other;\n",
            "begin\n",
            "  WriteLn(MyVar);\n",
            "end;\n",
            "end.\n",
        )
        .to_owned();
        let provider_source = "unit Shared;\ninterface\nconst MyVar = 1;\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(main.clone(), source.clone())
            .expect("target fixture parses");
        index
            .update(provider_a.clone(), provider_source.to_owned())
            .expect("provider A parses");
        index
            .update(provider_b.clone(), provider_source.to_owned())
            .expect("provider B parses");
        if let Some(provider) = match bound_provider {
            Some(0) => Some(provider_a.clone()),
            Some(1) => Some(provider_b.clone()),
            Some(other) => panic!("unsupported provider fixture index {other}"),
            None => None,
        } {
            index.bind_imports(&main, [("Shared".to_owned(), provider)]);
        }
        (index, main, provider_a, provider_b, source)
    }

    fn bounded_declaration_locations(
        index: &NavigationIndex,
        uri: &Url,
        position: Position,
    ) -> Result<Vec<Location>, String> {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            1_000_000,
            32 * 1024 * 1024,
            "rebind import preservation test",
        );
        index.navigate_with_cancel_and_budget(
            uri,
            position,
            NavigationTarget::Declaration,
            &cancel,
            &mut budget,
        )
    }

    #[test]
    fn rebind_preserves_explicit_target_import_binding_for_bounded_navigation() {
        let (index, main, provider_a, _provider_b, source) =
            duplicate_provider_rebind_fixture(Some(0));
        let before = bounded_declaration_locations(&index, &main, Position::new(9, 10))
            .expect("original imported reference resolves");
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].uri, provider_a);
        let before_provider = index.import_provider_uri(&main, "Shared").cloned();
        let before_fingerprint = index.import_binding_context_fingerprint(&main);

        let transformed_source = source.replace("my_var", "MyVar");
        let transformed = index
            .rebind_with_replaced_source_for_fix_all(
                &main,
                &transformed_source,
                &AtomicBool::new(false),
                &mut AssistanceBudget::new(
                    1_000_000,
                    32 * 1024 * 1024,
                    "rebind import preservation test",
                ),
            )
            .expect("transformed target rebinds");
        let after = bounded_declaration_locations(&transformed, &main, Position::new(9, 10))
            .expect("unchanged imported reference resolves after rebind");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].uri, provider_a);
        assert_eq!(
            transformed.import_provider_uri(&main, "Shared").cloned(),
            before_provider
        );
        assert_eq!(
            transformed.import_binding_context_fingerprint(&main),
            before_fingerprint
        );

        let local_rename = bounded_declaration_locations(&transformed, &main, Position::new(6, 6))
            .expect("combined local rename remains resolvable");
        assert_eq!(local_rename.len(), 1);
        assert_eq!(local_rename[0].uri, main);
        assert_eq!(local_rename[0].range.start.line, 5);
    }

    #[test]
    fn rebind_keeps_ambiguous_original_imports_conservative() {
        let (index, main, provider_a, provider_b, source) = duplicate_provider_rebind_fixture(None);
        let before = bounded_declaration_locations(&index, &main, Position::new(9, 10))
            .expect("ambiguous original import remains queryable");
        assert_eq!(before.len(), 2);
        assert_eq!(
            before
                .iter()
                .map(|location| location.uri.clone())
                .collect::<Vec<_>>(),
            vec![provider_a.clone(), provider_b.clone()]
        );
        assert_eq!(index.import_binding_context_fingerprint(&main), None);

        let transformed = index
            .rebind_with_replaced_source_for_fix_all(
                &main,
                &source.replace("my_var", "MyVar"),
                &AtomicBool::new(false),
                &mut AssistanceBudget::new(
                    1_000_000,
                    32 * 1024 * 1024,
                    "rebind import preservation test",
                ),
            )
            .expect("ambiguous target rebinds");
        let after = bounded_declaration_locations(&transformed, &main, Position::new(9, 10))
            .expect("ambiguous transformed import remains queryable");
        assert_eq!(after.len(), 2);
        assert_eq!(
            after
                .iter()
                .map(|location| location.uri.clone())
                .collect::<Vec<_>>(),
            vec![provider_a, provider_b]
        );
        assert_eq!(transformed.import_binding_context_fingerprint(&main), None);
    }

    #[test]
    fn rebind_keeps_changed_target_import_provider_conservative() {
        let (index, main, provider_a, provider_b, source) =
            duplicate_provider_rebind_fixture(Some(1));
        let before = bounded_declaration_locations(&index, &main, Position::new(9, 10))
            .expect("changed provider original import resolves");
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].uri, provider_b);
        let before_fingerprint = index.import_binding_context_fingerprint(&main);
        assert!(before_fingerprint.is_some());

        let transformed = index
            .rebind_with_replaced_source_for_fix_all(
                &main,
                &source.replace("my_var", "MyVar"),
                &AtomicBool::new(false),
                &mut AssistanceBudget::new(
                    1_000_000,
                    32 * 1024 * 1024,
                    "rebind import preservation test",
                ),
            )
            .expect("changed provider target rebinds");
        let after = bounded_declaration_locations(&transformed, &main, Position::new(9, 10))
            .expect("changed provider transformed import resolves");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].uri, provider_b);
        assert_ne!(after[0].uri, provider_a);
        assert_eq!(
            transformed.import_binding_context_fingerprint(&main),
            before_fingerprint
        );
    }

    fn deep_qualified_import_scan_source() -> String {
        let mut source = String::from(
            "unit Main;\ninterface\nuses Alpha, Legacy;\ntype TNode = class\n  Member: TNode;\nend;\nvar Receiver: TNode;\nimplementation\nprocedure Call; begin\n",
        );
        for _ in 0..40 {
            source.push_str("Receiver := Receiver.");
            for _ in 0..198 {
                source.push_str("Member.");
            }
            source.push_str("Member;\n");
        }
        source.push_str("end;\nend.\n");
        source
    }

    #[test]
    fn qualified_alias_scan_fails_closed_before_deep_paths_exceed_the_work_bound() {
        let source = deep_qualified_import_scan_source();
        assert_eq!(source.len(), 56_750);
        let uri = Url::parse("file:///tmp/qualified-alias-work-bound.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("deep qualified fixture parses");

        test_reset_qualified_identifier_work();
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(300_000, 8 * 1024 * 1024, "qualified-alias test");
        let result = index
            .import_spelling_has_qualified_use_with_budget(
                &uri,
                SourceSpan {
                    start: 0,
                    end: source
                        .find("implementation")
                        .expect("implementation boundary"),
                },
                "Legacy",
                &cancel,
                &mut budget,
            )
            .expect("deep path scan must stay within the assistance budget");
        let visits = test_qualified_identifier_node_visits();
        test_reset_qualified_identifier_work();

        assert!(
            result,
            "an unsupported deep path must withhold alias deduplication"
        );
        assert_eq!(
            visits,
            MAX_QUALIFIED_IMPORT_ALIAS_PATH_NODES + 1,
            "the deep-path fixture must stop at the documented cap"
        );
    }

    #[test]
    fn qualified_alias_scan_honors_cancellation_inside_deep_path_extraction() {
        let source = deep_qualified_import_scan_source();
        let uri = Url::parse("file:///tmp/qualified-alias-cancel.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("deep qualified fixture parses");

        test_reset_qualified_identifier_work();
        test_cancel_after_qualified_identifier_node_visits(10);
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(300_000, 8 * 1024 * 1024, "qualified-alias test");
        let error = index
            .import_spelling_has_qualified_use_with_budget(
                &uri,
                SourceSpan {
                    start: 0,
                    end: source
                        .find("implementation")
                        .expect("implementation boundary"),
                },
                "Legacy",
                &cancel,
                &mut budget,
            )
            .expect_err("cancellation must interrupt deep path extraction");
        let visits = test_qualified_identifier_node_visits();
        test_reset_qualified_identifier_work();

        assert_eq!(error, "request cancelled");
        assert!(
            visits <= 20,
            "cancellation must be observed near the configured visit bound, got {visits}"
        );
    }

    #[test]
    fn budgeted_navigation_honors_cancellation_inside_resolution() {
        let source = "unit Main;\ninterface\nimplementation\nprocedure Use;\nvar bad_var: Integer;\nbegin\n  bad_var := bad_var + 1;\nend;\nend.\n";
        let uri = Url::parse("file:///tmp/budgeted-navigation-cancel.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("navigation fixture parses");
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "budgeted navigation");
        budget.cancel_after_work(12);

        let result = index.navigate_with_cancel_and_budget(
            &uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
            &cancel,
            &mut budget,
        );

        assert_eq!(result, Err("request cancelled".to_string()));
        assert!(
            budget.remaining_work < 100_000 - 1,
            "cancellation must occur after entering semantic resolution"
        );
    }

    #[test]
    fn budgeted_navigation_fails_when_shared_work_exhausts_inside_resolution() {
        let source = "unit Main;\ninterface\nimplementation\nprocedure Use;\nvar bad_var: Integer;\nbegin\n  bad_var := bad_var + 1;\nend;\nend.\n";
        let uri =
            Url::parse("file:///tmp/budgeted-navigation-exhaustion.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("navigation fixture parses");
        let cancel = AtomicBool::new(false);
        let mut complete_budget =
            AssistanceBudget::new(100_000, 8 * 1024 * 1024, "budgeted navigation exhaustion");
        let locations = index
            .navigate_with_cancel_and_budget(
                &uri,
                Position::new(6, 2),
                NavigationTarget::Declaration,
                &cancel,
                &mut complete_budget,
            )
            .expect("complete semantic query");
        assert_eq!(locations.len(), 1);
        let consumed_work = 100_000 - complete_budget.remaining_work;
        assert!(consumed_work > 2, "fixture must enter semantic resolution");

        let mut limited_budget = AssistanceBudget::new(
            consumed_work - 1,
            8 * 1024 * 1024,
            "budgeted navigation exhaustion",
        );
        let error = index
            .navigate_with_cancel_and_budget(
                &uri,
                Position::new(6, 2),
                NavigationTarget::Declaration,
                &cancel,
                &mut limited_budget,
            )
            .expect_err("the shared budget must stop semantic resolution");
        assert!(error.contains("budgeted navigation exhaustion exceeds"));
        assert!(limited_budget.exhausted());
    }

    #[test]
    fn qualified_alias_path_extraction_charges_work_and_source_bytes() {
        let source = concat!(
            "unit QualifiedAliasBudget;\n",
            "interface\n",
            "implementation\n",
            "procedure Call; begin Legacy.Value := 1; end;\n",
            "end.\n",
        );
        let uri = Url::parse("file:///tmp/qualified-alias-budget.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("qualified budget fixture parses");
        let root = index
            .documents
            .get(&uri)
            .expect("qualified budget document")
            .tree
            .root_node();
        let path = collect_nodes_matching(root, "exprDot")
            .into_iter()
            .find(|node| !is_nested_qualified_identifier_node(*node))
            .expect("top-level qualified path");
        let cancel = AtomicBool::new(false);

        let mut work_budget = AssistanceBudget::new(1, 8 * 1024 * 1024, "qualified-alias test");
        let work_error = qualified_identifier_matches_spelling_with_budget(
            path,
            source,
            "Legacy",
            &cancel,
            &mut work_budget,
        )
        .expect_err("each extracted path node must consume work budget");
        assert!(
            work_error.contains("qualified-alias test exceeds the 1-node traversal limit"),
            "unexpected work-budget error: {work_error}"
        );

        let mut byte_budget = AssistanceBudget::new(16, 1, "qualified-alias test");
        let byte_error = qualified_identifier_matches_spelling_with_budget(
            path,
            source,
            "Legacy",
            &cancel,
            &mut byte_budget,
        )
        .expect_err("identifier source bytes must consume byte budget");
        assert!(
            byte_error.contains("qualified-alias test exceeds the 1-byte scan limit"),
            "unexpected byte-budget error: {byte_error}"
        );
    }

    #[test]
    fn binding_reference_unit_path_work_is_bounded_before_materialization() {
        let provider_uri =
            Url::parse("file:///tmp/bounded-unit-path/provider.pas").expect("provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/bounded-unit-path/consumer.pas").expect("consumer URI");
        let provider = "unit Ns.Provider;\ninterface\nconst Value = 1;\nimplementation\nend.\n";
        let mut consumer = String::from(
            "unit Consumer;\ninterface\nuses Ns.Provider;\nimplementation\nprocedure Run;\nbegin\n  Ns.Provider",
        );
        for index in 0..600 {
            write!(&mut consumer, ".Member{index}").expect("write qualified path");
        }
        consumer.push_str(";\nend;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer)
            .expect("consumer parses");
        index.bind_imports(
            &consumer_uri,
            [("Ns.Provider".to_owned(), provider_uri.clone())],
        );

        let mut work_budget = BindingWorkBudget::new(1_000);
        let mut semantic_budget = AssistanceBudget::new(1_000, 8 * 1024 * 1024, "unit-path test");
        let error = index
            .binding_locations_with_cancel_and_work_budget(
                &provider_uri,
                Position::new(0, 8),
                true,
                &AtomicBool::new(false),
                &mut work_budget,
                &mut semantic_budget,
            )
            .expect_err("qualified unit-path work must be charged before it completes");
        assert!(
            error.contains("unit-path test exceeds"),
            "unexpected bounded-path error: {error}"
        );
    }

    #[test]
    fn binding_reference_unit_path_honors_cancellation_during_materialization() {
        let provider_uri =
            Url::parse("file:///tmp/cancellable-unit-path/provider.pas").expect("provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/cancellable-unit-path/consumer.pas").expect("consumer URI");
        let provider = "unit Ns.Provider;\ninterface\nconst Value = 1;\nimplementation\nend.\n";
        let mut consumer = String::from(
            "unit Consumer;\ninterface\nuses Ns.Provider;\nimplementation\nprocedure Run;\nbegin\n  Ns.Provider",
        );
        for index in 0..180 {
            write!(&mut consumer, ".Member{index}").expect("write qualified path");
        }
        consumer.push_str(";\nend;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer)
            .expect("consumer parses");
        index.bind_imports(
            &consumer_uri,
            [("Ns.Provider".to_owned(), provider_uri.clone())],
        );

        test_reset_unit_path_work();
        test_cancel_after_unit_path_node_visits(8);
        let mut semantic_budget =
            AssistanceBudget::new(100_000, 8 * 1024 * 1024, "cancellable unit-path test");
        let error = index
            .binding_locations_with_cancel_and_work_budget(
                &provider_uri,
                Position::new(0, 8),
                true,
                &AtomicBool::new(false),
                &mut BindingWorkBudget::new(100_000),
                &mut semantic_budget,
            )
            .expect_err("unit-path cancellation must interrupt the inner traversal");
        let visits = test_unit_path_node_visits();
        test_reset_unit_path_work();

        assert_eq!(error, "request cancelled");
        assert!(
            visits <= 16,
            "inner unit-path traversal ignored cancellation until {visits} nodes"
        );
    }

    #[test]
    fn unit_prefix_matching_charges_long_import_spellings_before_repeated_scans() {
        let uri = Url::parse("file:///tmp/long-unit-prefix-spellings.pas").expect("fixture URI");
        let import_names = (0..4)
            .map(|index| {
                format!(
                    "LongNamespaceComponentThatIsNotTheQuery{index}.LongProviderComponentThatIsAlsoNotTheQuery{index}"
                )
            })
            .collect::<Vec<_>>();
        let mut source = String::from("unit PrefixBudgetConsumer;\ninterface\nuses ");
        source.push_str(&import_names.join(", "));
        source.push_str(";\nimplementation\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("long import spelling fixture parses");
        let document = index.documents.get(&uri).expect("consumer document");
        let parts = vec!["MissingQueryPrefix".to_owned(), "Member".to_owned()];
        let existing_bytes = document.unit_name.len().saturating_add(
            document
                .interface_uses
                .len()
                .saturating_mul(std::mem::size_of::<&String>()),
        );
        let spelling_bytes = document
            .interface_uses
            .iter()
            .map(String::len)
            .sum::<usize>();
        let per_scan_bytes = existing_bytes.saturating_add(spelling_bytes);
        let mut budget = AssistanceBudget::new(
            100_000,
            per_scan_bytes.saturating_mul(2).saturating_sub(1),
            "unit-prefix spelling test",
        );
        let cancel = AtomicBool::new(false);

        let first_scan = index
            .longest_visible_unit_prefix_with_budget(
                &uri,
                document,
                0,
                &parts,
                &cancel,
                &mut budget,
            )
            .expect("the first full spelling scan fits its byte allowance");
        assert_eq!(first_scan, None);

        let error = index
            .longest_visible_unit_prefix_with_budget(
                &uri,
                document,
                0,
                &parts,
                &cancel,
                &mut budget,
            )
            .expect_err("repeated long import spellings must consume scan bytes");
        assert!(
            error.contains("unit-prefix spelling test exceeds"),
            "unexpected spelling-budget error: {error}"
        );
        assert!(budget.exhausted());
    }

    #[test]
    fn unit_prefix_matching_honors_cancellation_during_component_comparison() {
        let uri =
            Url::parse("file:///tmp/cancellable-unit-prefix-spelling.pas").expect("fixture URI");
        let source = concat!(
            "unit Current.Namespace;\n",
            "interface\n",
            "uses First.Long.Import, Second.Long.Import;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("cancellable import spelling fixture parses");
        let document = index.documents.get(&uri).expect("consumer document");
        let parts = vec![
            "Current".to_owned(),
            "Other".to_owned(),
            "Member".to_owned(),
        ];
        let pre_match_work = parts.len().saturating_add(1) + document.interface_uses.len();
        let mut budget = AssistanceBudget::new(
            100_000,
            8 * 1024 * 1024,
            "cancellable unit-prefix spelling test",
        );
        budget.cancel_after_work(pre_match_work.saturating_add(1));
        let error = index
            .longest_visible_unit_prefix_with_budget(
                &uri,
                document,
                0,
                &parts,
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect_err("matching must poll cancellation during spelling comparison");
        assert_eq!(error, "request cancelled");
    }

    #[test]
    fn unit_prefix_matching_preserves_short_case_alias_and_namespaced_mapping() {
        let provider_uri =
            Url::parse("file:///tmp/unit-prefix-controls/Vendor.Core.pas").expect("provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/unit-prefix-controls/Consumer.pas").expect("consumer URI");
        let provider = "unit Vendor.Core;\ninterface\nconst Value = 1;\nimplementation\nend.\n";
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses Alias.Core;\n",
            "implementation\n",
            "end.\n",
        );

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider fixture parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer fixture parses");
        index.bind_imports(
            &consumer_uri,
            [("Alias.Core".to_owned(), provider_uri.clone())],
        );

        let document = index
            .documents
            .get(&consumer_uri)
            .expect("consumer document");
        let parts = vec!["alias".to_owned(), "CORE".to_owned(), "Value".to_owned()];
        let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "unit-prefix control");
        let result = index
            .longest_visible_unit_prefix_with_budget(
                &consumer_uri,
                document,
                0,
                &parts,
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("short case-insensitive alias path resolves");
        assert_eq!(result, Some((2, vec![provider_uri])));
    }

    #[test]
    fn qualified_alias_scan_keeps_small_valid_qualified_paths() {
        let source = concat!(
            "unit SmallQualifiedAlias;\n",
            "interface\n",
            "uses Alpha, Legacy;\n",
            "implementation\n",
            "procedure Call; begin\n",
            "  Legacy.Value := 1;\n",
            "  Receiver.Value := 1;\n",
            "end;\n",
            "end.\n",
        );
        let uri = Url::parse("file:///tmp/small-qualified-alias.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("small qualified fixture parses");
        let clause = SourceSpan {
            start: 0,
            end: source
                .find("implementation")
                .expect("implementation boundary"),
        };

        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(300_000, 8 * 1024 * 1024, "qualified-alias test");
        assert!(
            index
                .import_spelling_has_qualified_use_with_budget(
                    &uri,
                    clause,
                    "Legacy",
                    &cancel,
                    &mut budget,
                )
                .expect("valid qualified path scan")
        );

        let mut budget = AssistanceBudget::new(300_000, 8 * 1024 * 1024, "qualified-alias test");
        assert!(
            !index
                .import_spelling_has_qualified_use_with_budget(
                    &uri,
                    clause,
                    "Unused",
                    &cancel,
                    &mut budget,
                )
                .expect("unrelated qualified path scan")
        );
    }

    #[test]
    fn missing_unit_discovery_honors_cancellation_and_budget_exhaustion() {
        let uri = Url::parse("file:///tmp/missing-unit-budget.pas").expect("fixture URI");
        let source = "unit MissingUnitBudget;\ninterface\ntype TAlias = MissingType;\nimplementation\nend.\n";
        let position = text::offset_to_position(
            source,
            source.find("MissingType").expect("missing identifier"),
        )
        .expect("identifier position");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("budget fixture parses");

        let cancelled = AtomicBool::new(true);
        let mut budget = AssistanceBudget::new(1, 1, "missing-unit test");
        assert_eq!(
            index
                .missing_unit_candidates_with_budget(&uri, position, &cancelled, &mut budget)
                .expect_err("cancelled discovery must fail"),
            "request cancelled"
        );

        let cancel = AtomicBool::new(false);
        let mut exhausted = AssistanceBudget::new(0, 0, "missing-unit test");
        let error = index
            .missing_unit_candidates_with_budget(&uri, position, &cancel, &mut exhausted)
            .expect_err("exhausted discovery must fail closed");
        assert!(
            error.contains("missing-unit test exceeds"),
            "unexpected deterministic budget error: {error}"
        );
    }

    #[test]
    fn class_field_binding_work_does_not_repeat_root_lookup_per_occurrence() {
        let (small_count, small_root_lookups) = query_repeated_class_field(1);
        let (large_count, large_root_lookups) = query_repeated_class_field(128);

        assert_eq!(small_count, 2);
        assert_eq!(large_count, 129);
        assert_eq!(
            large_root_lookups, small_root_lookups,
            "known class owners must not restart root lookup for each field use"
        );
    }

    #[test]
    fn unchanged_document_update_reuses_the_immutable_parsed_model() {
        let uri = Url::parse("file:///tmp/unchanged-model.pas").expect("fixture URI");
        let source = "unit UnchangedModel;\ninterface\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("initial document parses");
        let first = index
            .documents
            .get(&uri)
            .expect("initial document retained")
            .parsed
            .clone();
        index.bind_imports(&uri, std::iter::once(("Provider".to_owned(), uri.clone())));

        index
            .update(uri.clone(), source.to_owned())
            .expect("unchanged document parses");
        let second = index
            .documents
            .get(&uri)
            .expect("unchanged document retained");

        assert!(
            Arc::ptr_eq(&first, &second.parsed),
            "unchanged source and defines must reuse the parsed model"
        );
        assert!(
            second.import_bindings.is_none(),
            "reused models must receive fresh per-index import bindings"
        );
    }

    #[test]
    fn changed_document_update_preserves_incremental_tree_reuse_and_fresh_results() {
        let uri = Url::parse("file:///tmp/incremental-model.pas").expect("fixture URI");
        let old_source = "unit IncrementalModel;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Use;\nbegin\n  Stable := 2;\nend;\nend.\n";
        let new_source = "unit IncrementalModel;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Use;\nbegin\n  Stable := 3;\nend;\nend.\n";
        let mut incremental = NavigationIndex::new();
        incremental
            .update(uri.clone(), old_source.to_owned())
            .expect("old document parses");
        let stable_id = incremental
            .documents
            .get(&uri)
            .expect("old document retained")
            .tree
            .root_node()
            .named_descendant_for_byte_range(5, 6)
            .expect("stable unit node")
            .id();
        incremental
            .update(uri.clone(), new_source.to_owned())
            .expect("changed document parses");

        let mut fresh = NavigationIndex::new();
        fresh
            .update(uri.clone(), new_source.to_owned())
            .expect("fresh document parses");
        let updated = incremental
            .documents
            .get(&uri)
            .expect("updated document retained");
        let updated_stable_id = updated
            .tree
            .root_node()
            .named_descendant_for_byte_range(5, 6)
            .expect("stable unit node after edit")
            .id();

        assert_eq!(
            stable_id, updated_stable_id,
            "unchanged syntax must be reused"
        );
        assert_eq!(
            incremental
                .document_symbols(&uri)
                .expect("incremental symbols"),
            fresh.document_symbols(&uri).expect("fresh symbols")
        );
        let use_offset = new_source.find("Stable := 3").expect("updated use") + 1;
        let use_position = text::offset_to_position(new_source, use_offset).expect("use position");
        assert_eq!(
            incremental.navigate(&uri, use_position, NavigationTarget::Declaration),
            fresh.navigate(&uri, use_position, NavigationTarget::Declaration),
            "navigation must match a fresh parse after the edit"
        );
    }

    #[test]
    fn changed_document_update_preserves_assistance_results() {
        let uri = Url::parse("file:///tmp/incremental-assistance.pas").expect("fixture URI");
        let old_source = "unit IncrementalAssistance;\ninterface\ntype\n  TThing = class\n    Value: Integer;\n    procedure SetValue(AValue: Integer);\n  end;\nimplementation\nprocedure TThing.SetValue(AValue: Integer);\nbegin\n  Value := AValue;\nend;\nprocedure Use;\nvar\n  Thing: TThing;\nbegin\n  Thing.Val;\n  Thing.SetValue(1);\nend;\nend.\n";
        let new_source = "unit IncrementalAssistance;\ninterface\ntype\n  TThing = class\n    Value: Integer;\n    procedure SetValue(AValue: Integer);\n  end;\nimplementation\nconst Added = 1;\nprocedure TThing.SetValue(AValue: Integer);\nbegin\n  Value := AValue + 1;\nend;\nprocedure Use;\nvar\n  Thing: TThing;\nbegin\n  Thing.Val;\n  Thing.SetValue(2);\nend;\nend.\n";
        let mut incremental = NavigationIndex::new();
        incremental
            .update(uri.clone(), old_source.to_owned())
            .expect("old assistance source parses");
        incremental
            .update(uri.clone(), new_source.to_owned())
            .expect("changed assistance source parses");

        let mut fresh = NavigationIndex::new();
        fresh
            .update(uri.clone(), new_source.to_owned())
            .expect("fresh assistance source parses");

        let completion_position = text::offset_to_position(
            new_source,
            new_source.find("Thing.Val").expect("completion expression") + "Thing.Val".len(),
        )
        .expect("completion position");
        assert_eq!(
            incremental.completion(&uri, completion_position),
            fresh.completion(&uri, completion_position),
            "completion must match a fresh parse after the edit"
        );

        let value_position = text::offset_to_position(
            new_source,
            new_source.find("Thing.Val").expect("hover expression") + "Thing.".len() + 1,
        )
        .expect("hover position");
        assert_eq!(
            incremental.hover(&uri, value_position),
            fresh.hover(&uri, value_position),
            "hover must match a fresh parse after the edit"
        );

        let signature_position = text::offset_to_position(
            new_source,
            new_source.find("Thing.SetValue(2").expect("signature call") + "Thing.SetValue(2".len(),
        )
        .expect("signature position");
        assert_eq!(
            incremental.signature_help(&uri, signature_position),
            fresh.signature_help(&uri, signature_position),
            "signature help must match a fresh parse after the edit"
        );
    }

    #[test]
    fn changed_effective_defines_rebuild_the_parsed_model() {
        let uri = Url::parse("file:///tmp/define-model.pas").expect("fixture URI");
        let source = "unit DefineModel;\ninterface\n{$IFDEF FEATURE}\nconst Enabled = 1;\n{$ENDIF}\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update_with_defines(uri.clone(), source.to_owned(), &[])
            .expect("document without feature parses");
        let without_feature = index
            .documents
            .get(&uri)
            .expect("document without feature retained")
            .parsed
            .clone();
        let hidden_index = without_feature
            .symbols
            .iter()
            .position(|symbol| symbol.name.eq_ignore_ascii_case("Enabled"))
            .expect("conditional symbol is retained for unknown-state handling");
        assert!(without_feature.conditional_unknown_symbols[hidden_index]);

        index
            .update_with_defines(uri.clone(), source.to_owned(), &["FEATURE".to_owned()])
            .expect("document with feature parses");
        let with_feature = index
            .documents
            .get(&uri)
            .expect("document with feature retained")
            .parsed
            .clone();

        assert!(
            !Arc::ptr_eq(&without_feature, &with_feature),
            "a define change must invalidate the semantic model"
        );
        let enabled_index = with_feature
            .symbols
            .iter()
            .position(|symbol| symbol.name.eq_ignore_ascii_case("Enabled"))
            .expect("conditional symbol with feature");
        assert!(!with_feature.conditional_unknown_symbols[enabled_index]);
    }

    #[test]
    fn changed_compiler_context_rebuilds_the_parsed_model() {
        let uri = Url::parse("file:///tmp/compiler-context-model.pas").expect("fixture URI");
        let source = "unit CompilerContextModel;\ninterface\n{$IF CompilerVersion >= 24}\nconst Enabled = 1;\n{$ENDIF}\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        let old_context = ConditionalContext::default();
        index
            .update_with_context_with_cancel(
                uri.clone(),
                source.to_owned(),
                &old_context,
                &AtomicBool::new(false),
            )
            .expect("document without compiler version parses");
        let without_version = index
            .documents
            .get(&uri)
            .expect("document without compiler version retained")
            .parsed
            .clone();

        let new_context =
            ConditionalContext::default().with_compiler_version(CompilerVersion::new(24, 0));
        index
            .update_with_context_with_cancel(
                uri.clone(),
                source.to_owned(),
                &new_context,
                &AtomicBool::new(false),
            )
            .expect("document with compiler version parses");
        let with_version = index
            .documents
            .get(&uri)
            .expect("document with compiler version retained")
            .parsed
            .clone();

        assert!(
            !Arc::ptr_eq(&without_version, &with_version),
            "a compiler-context change must invalidate the semantic model"
        );
        assert!(
            with_version
                .symbols
                .iter()
                .any(|symbol| symbol.name.eq_ignore_ascii_case("Enabled"))
        );
    }

    #[test]
    fn cancelled_update_does_not_publish_a_parsed_cache_entry() {
        let uri = Url::parse("file:///tmp/cancelled-model.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        let cancel = AtomicBool::new(true);

        let error = index
            .update_with_defines_with_cancel(
                uri.clone(),
                "unit CancelledModel;\ninterface\nimplementation\nend.\n".to_owned(),
                &[],
                &cancel,
            )
            .expect_err("cancelled parse must fail");

        assert_eq!(error, "request cancelled");
        assert!(
            !index.contains(&uri),
            "cancelled work must not publish a document"
        );
    }

    #[test]
    fn semantic_diagnostics_leave_an_unqualified_global_name_incomplete_without_system_source() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-local.pas").expect("fixture URI");
        let source = "unit SemanticDiagnosticsLocal;\ninterface\nvar Known: Integer;\nimplementation\nprocedure Run;\nbegin\n  Kno := 1;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("semantic diagnostic fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(diagnostics.is_empty());
        let start = source.rfind("Kno").expect("missing name");
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &uri,
                    text::offset_to_position(source, start).expect("missing position"),
                    &AtomicBool::new(false),
                )
                .expect("missing proof status"),
            SemanticProofStatus::Incomplete
        );
    }

    #[test]
    fn semantic_proof_status_distinguishes_absence_resolution_and_incompleteness() {
        let uri = Url::parse("file:///tmp/semantic-proof-status.pas").expect("fixture URI");
        let source = concat!(
            "unit SemanticProofStatus;\n",
            "interface\n",
            "var Known: Integer;\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  Known := 1;\n",
            "  Missing := 2;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("proof fixture parses");
        let cancel = AtomicBool::new(false);

        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &uri,
                    text::offset_to_position(source, source.find("Known :=").expect("known"))
                        .expect("known position"),
                    &cancel,
                )
                .expect("known proof status"),
            SemanticProofStatus::Resolved
        );
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &uri,
                    text::offset_to_position(source, source.find("Missing :=").expect("missing"))
                        .expect("missing position"),
                    &cancel,
                )
                .expect("missing proof status"),
            SemanticProofStatus::Incomplete
        );
    }

    #[test]
    fn semantic_diagnostics_report_a_missing_member_only_for_a_known_receiver() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-member.pas").expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsMember;\n",
            "interface\n",
            "type\n",
            "  TBox = class\n",
            "    Value: Integer;\n",
            "  end;\n",
            "implementation\n",
            "procedure Run;\n",
            "var Box: TBox;\n",
            "begin\n",
            "  Box.Missing := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("member diagnostic fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].kind, SemanticDiagnosticKind::MissingMember);
        let start = source.rfind("Missing").expect("missing member");
        assert_eq!(
            diagnostics[0].span,
            SourceSpan {
                start,
                end: start + "Missing".len(),
            }
        );
        assert_eq!(diagnostics[0].message, "missing member 'Missing'");
    }

    #[test]
    fn semantic_diagnostics_report_an_override_without_a_matching_virtual_ancestor() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-invalid-override.pas")
            .expect("override URI");
        let source = concat!(
            "unit SemanticDiagnosticsInvalidOverride;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  TBase = class(TObject)\n",
            "    procedure Run(Value: Integer); virtual;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Run(Value: string); override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Run(Value: Integer); begin end;\n",
            "procedure TChild.Run(Value: string); begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("invalid override fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::InvalidOverride
                    && diagnostic.message
                        == "invalid override 'Run': no inherited virtual or dynamic method matches"
            }),
            "the incompatible explicit override must be diagnosed: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_report_a_missing_interface_method_once() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-missing-interface.pas")
            .expect("interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsMissingInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required(Value: Integer);\n",
            "  end;\n",
            "  TImplementation = class(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("missing interface fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert_eq!(
            diagnostics.len(),
            1,
            "one interface obligation: {diagnostics:?}"
        );
        assert_eq!(
            diagnostics[0].kind,
            SemanticDiagnosticKind::MissingInterfaceImplementation
        );
        assert_eq!(
            diagnostics[0].message,
            "class 'TImplementation' does not implement interface method 'Required'"
        );
        let class_start = source
            .find("TImplementation = class")
            .expect("implementing class");
        assert_eq!(
            diagnostics[0].span,
            SourceSpan {
                start: class_start,
                end: class_start + "TImplementation".len(),
            }
        );
    }

    #[test]
    fn semantic_diagnostics_anchor_an_imported_interface_obligation_to_the_consumer_class() {
        let provider_uri = Url::parse("file:///tmp/semantic-diagnostics-contract-provider.pas")
            .expect("provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-contract-consumer.pas")
            .expect("consumer URI");
        let provider = concat!(
            "unit ContractProvider;\n",
            "interface\n",
            "type\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit ContractConsumer;\n",
            "interface\n",
            "uses ContractProvider;\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  TChild = class(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider fixture parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer fixture parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("ContractProvider".to_owned(), provider_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert_eq!(
            diagnostics.len(),
            1,
            "one imported obligation: {diagnostics:?}"
        );
        let class_start = consumer.find("TChild = class").expect("consumer class");
        assert_eq!(
            diagnostics[0].span,
            SourceSpan {
                start: class_start,
                end: class_start + "TChild".len(),
            },
            "foreign interface declarations must not be mapped as consumer offsets"
        );
    }

    #[test]
    fn semantic_diagnostics_keep_an_implicit_tobject_override_unknown() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-implicit-tobject-override.pas")
            .expect("implicit TObject URI");
        let source = concat!(
            "unit SemanticDiagnosticsImplicitTObjectOverride;\n",
            "interface\n",
            "type\n",
            "  TChild = class\n",
            "    destructor Destroy; override;\n",
            "  end;\n",
            "implementation\n",
            "destructor TChild.Destroy; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("implicit TObject fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride),
            "an implicit TObject root is not a proven empty ancestor: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_keep_an_implicit_tobject_interface_absence_unknown() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-implicit-tobject-interface.pas")
            .expect("implicit TObject interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsImplicitTObjectInterface;\n",
            "interface\n",
            "type\n",
            "  IRequired = interface\n",
            "    function ToString: string;\n",
            "  end;\n",
            "  TChild = class(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("implicit TObject interface fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "an implicit compiler root cannot prove ToString absent: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_use_a_source_backed_system_tobject_for_implicit_overrides() {
        let system_uri =
            Url::parse("file:///tmp/semantic-diagnostics-contract-system.pas").expect("System URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-diagnostics-contract-system-consumer.pas")
                .expect("consumer URI");
        let system = concat!(
            "unit System;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "    destructor Destroy; virtual;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit ContractSystemConsumer;\n",
            "interface\n",
            "uses System;\n",
            "type\n",
            "  TChild = class\n",
            "    destructor Destroy; override;\n",
            "  end;\n",
            "implementation\n",
            "destructor TChild.Destroy; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(system_uri.clone(), system.to_owned())
            .expect("System fixture parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer fixture parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("System".to_owned(), system_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride),
            "a complete source-backed TObject supplies Destroy: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_contract_claims_from_an_incomplete_provider() {
        let provider_uri = Url::parse("file:///tmp/semantic-diagnostics-incomplete-provider.pas")
            .expect("provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-incomplete-consumer.pas")
            .expect("consumer URI");
        let provider = concat!(
            "unit IncompleteProvider;\n",
            "interface\n",
            "type\n",
            "  TBase = class\n",
            "    procedure ;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit IncompleteConsumer;\n",
            "interface\n",
            "uses IncompleteProvider;\n",
            "type\n",
            "  TChild = class(TBase)\n",
            "    procedure Run; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TChild.Run; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("incomplete provider parses with recovery");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer fixture parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("IncompleteProvider".to_owned(), provider_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride),
            "provider recovery must suppress absence proofs: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_contract_claims_with_missing_import_context() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-missing-contract-import.pas")
            .expect("missing import URI");
        let source = concat!(
            "unit SemanticDiagnosticsMissingContractImport;\n",
            "interface\n",
            "uses MissingUnit;\n",
            "type\n",
            "  TBase = class\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Missing; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TChild.Missing; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("missing import fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride),
            "an unresolved import prevents a closed-world override proof: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_unknown_interface_obligation_signatures() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-unknown-contract-signature.pas")
            .expect("unknown signature URI");
        let source = concat!(
            "unit SemanticDiagnosticsUnknownContractSignature;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IUnknownType = interface\n",
            "    procedure Required(Value: UnknownType);\n",
            "  end;\n",
            "  IGeneric = interface\n",
            "    procedure Generic<T>(Value: T);\n",
            "  end;\n",
            "  TChild = class(TObject, IUnknownType, IGeneric)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("unknown signature fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "unknown obligation signatures cannot support a missing claim: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_contract_claims_for_an_uninstantiated_generic_owner() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-generic-contract-owner.pas")
            .expect("generic owner URI");
        let source = concat!(
            "unit SemanticDiagnosticsGenericContractOwner;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TGeneric<T: record> = class(TObject, IRequired)\n",
            "    procedure Missing; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TGeneric.Missing; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("generic owner fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::InvalidOverride
                    && diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "uninstantiated generic owners cannot support contract absence proofs: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_keep_a_valid_virtual_override_silent() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-valid-override.pas")
            .expect("override URI");
        let source = concat!(
            "unit SemanticDiagnosticsValidOverride;\n",
            "interface\n",
            "type\n",
            "  TBase = class\n",
            "    procedure Run(Value: Integer); virtual;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Run(Value: Integer); override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Run(Value: Integer); begin end;\n",
            "procedure TChild.Run(Value: Integer); begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("valid override fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride),
            "valid overrides must remain silent: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_treat_default_register_conventions_conservatively() {
        fn contract_diagnostics(source: &str, name: &str) -> Vec<SemanticDiagnostic> {
            let uri = Url::parse(&format!("file:///tmp/{name}.pas")).expect("fixture URI");
            let mut index = NavigationIndex::new();
            index
                .update(uri.clone(), source.to_owned())
                .expect("calling convention fixture parses");
            index
                .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
                .expect("semantic diagnostics complete")
        }

        let explicit_register_target = concat!(
            "unit SemanticDiagnosticsRegisterTarget;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  TBase = class(TObject)\n",
            "    procedure Run; register; virtual;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Run; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Run; begin end;\n",
            "procedure TChild.Run; begin end;\n",
            "end.\n",
        );
        assert!(
            contract_diagnostics(
                explicit_register_target,
                "semantic-diagnostics-register-target"
            )
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride)
        );

        let explicit_register_override = concat!(
            "unit SemanticDiagnosticsRegisterOverride;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  TBase = class(TObject)\n",
            "    procedure Run; virtual;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Run; register; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Run; begin end;\n",
            "procedure TChild.Run; begin end;\n",
            "end.\n",
        );
        assert!(
            contract_diagnostics(
                explicit_register_override,
                "semantic-diagnostics-register-override"
            )
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride)
        );

        let explicit_register_interface = concat!(
            "unit SemanticDiagnosticsRegisterInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required; register;\n",
            "  end;\n",
            "  TChild = class(TObject, IRequired)\n",
            "    procedure Required;\n",
            "  end;\n",
            "implementation\n",
            "procedure TChild.Required; begin end;\n",
            "end.\n",
        );
        assert!(
            contract_diagnostics(
                explicit_register_interface,
                "semantic-diagnostics-register-interface"
            )
            .iter()
            .all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            })
        );

        let explicit_register_implementation = concat!(
            "unit SemanticDiagnosticsRegisterImplementation;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TChild = class(TObject, IRequired)\n",
            "    procedure Required; register;\n",
            "  end;\n",
            "implementation\n",
            "procedure TChild.Required; begin end;\n",
            "end.\n",
        );
        assert!(
            contract_diagnostics(
                explicit_register_implementation,
                "semantic-diagnostics-register-implementation"
            )
            .iter()
            .all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            })
        );

        let explicit_incompatible_override = concat!(
            "unit SemanticDiagnosticsIncompatibleConventionOverride;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  TBase = class(TObject)\n",
            "    procedure Run; cdecl; virtual;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Run; register; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Run; begin end;\n",
            "procedure TChild.Run; begin end;\n",
            "end.\n",
        );
        assert_eq!(
            contract_diagnostics(
                explicit_incompatible_override,
                "semantic-diagnostics-incompatible-convention-override"
            )
            .iter()
            .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::InvalidOverride)
            .count(),
            1,
            "two known explicit conventions remain incompatible"
        );

        let explicit_incompatible_interface = concat!(
            "unit SemanticDiagnosticsIncompatibleConventionInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required; cdecl;\n",
            "  end;\n",
            "  TChild = class(TObject, IRequired)\n",
            "    procedure Required; register;\n",
            "  end;\n",
            "implementation\n",
            "procedure TChild.Required; begin end;\n",
            "end.\n",
        );
        assert_eq!(
            contract_diagnostics(
                explicit_incompatible_interface,
                "semantic-diagnostics-incompatible-convention-interface"
            )
            .iter()
            .filter(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::MissingInterfaceImplementation
            })
            .count(),
            1,
            "two known explicit interface conventions remain incompatible"
        );

        let unsupported_override = concat!(
            "unit SemanticDiagnosticsUnsupportedConventionOverride;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  TBase = class(TObject)\n",
            "    procedure Run; winapi; virtual;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "    procedure Run; override;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Run; begin end;\n",
            "procedure TChild.Run; begin end;\n",
            "end.\n",
        );
        assert!(
            contract_diagnostics(
                unsupported_override,
                "semantic-diagnostics-unsupported-convention-override"
            )
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::InvalidOverride)
        );

        let unsupported_interface = concat!(
            "unit SemanticDiagnosticsUnsupportedConventionInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required; winapi;\n",
            "  end;\n",
            "  TChild = class(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        assert!(
            contract_diagnostics(
                unsupported_interface,
                "semantic-diagnostics-unsupported-convention-interface"
            )
            .iter()
            .all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            })
        );
    }

    #[test]
    fn semantic_diagnostics_accept_an_inherited_interface_implementation() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-inherited-interface.pas")
            .expect("interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsInheritedInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TBase = class(TObject, IRequired)\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Required; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("inherited interface fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "inherited implementations must satisfy descendants: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_missing_methods_on_abstract_classes() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-abstract-interface.pas")
            .expect("interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsAbstractInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TAbstract = class abstract(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("abstract interface fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "abstract classes may defer interface methods: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_deduplicate_interface_diamond_obligations() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-interface-diamond.pas")
            .expect("interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsInterfaceDiamond;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRoot = interface\n",
            "    procedure Hit;\n",
            "  end;\n",
            "  ILeft = interface(IRoot)\n",
            "  end;\n",
            "  IRight = interface(IRoot)\n",
            "  end;\n",
            "  TDiamond = class(TObject, ILeft, IRight)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("diamond fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::MissingInterfaceImplementation
                })
                .count(),
            1,
            "a diamond obligation is reported once: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_replay_interface_requirements_for_each_class() {
        fn missing_classes(source: &str, name: &str) -> Vec<String> {
            let uri = Url::parse(&format!("file:///tmp/{name}.pas")).expect("fixture URI");
            let mut index = NavigationIndex::new();
            index
                .update(uri.clone(), source.to_owned())
                .expect("interface requirement fixture parses");
            index
                .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
                .expect("semantic diagnostics complete")
                .into_iter()
                .filter(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::MissingInterfaceImplementation
                })
                .map(|diagnostic| diagnostic.message)
                .collect()
        }

        let missing_missing = concat!(
            "unit SemanticDiagnosticsMemoMissingMissing;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TFirst = class(TObject, IRequired)\n",
            "  end;\n",
            "  TSecond = class(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let diagnostics =
            missing_classes(missing_missing, "semantic-diagnostics-memo-missing-missing");
        assert_eq!(diagnostics.len(), 2, "both classes have an obligation");
        assert!(
            diagnostics
                .iter()
                .any(|message| message.starts_with("class 'TFirst'"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|message| message.starts_with("class 'TSecond'"))
        );

        let valid_first_missing_second = concat!(
            "unit SemanticDiagnosticsMemoValidFirst;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TFirst = class(TObject, IRequired)\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TSecond = class(TObject, IRequired)\n",
            "  end;\n",
            "implementation\n",
            "procedure TFirst.Required; begin end;\n",
            "end.\n",
        );
        let diagnostics = missing_classes(
            valid_first_missing_second,
            "semantic-diagnostics-memo-valid-first-missing-second",
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "a later missing class remains visible"
        );
        assert!(diagnostics[0].starts_with("class 'TSecond'"));

        let missing_first_valid_second = concat!(
            "unit SemanticDiagnosticsMemoMissingFirst;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TFirst = class(TObject, IRequired)\n",
            "  end;\n",
            "  TSecond = class(TObject, IRequired)\n",
            "    procedure Required;\n",
            "  end;\n",
            "implementation\n",
            "procedure TSecond.Required; begin end;\n",
            "end.\n",
        );
        let diagnostics = missing_classes(
            missing_first_valid_second,
            "semantic-diagnostics-memo-missing-first-valid-second",
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "source order must not alter obligations"
        );
        assert!(diagnostics[0].starts_with("class 'TFirst'"));

        let inherited_shared = concat!(
            "unit SemanticDiagnosticsMemoInherited;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRoot = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  IChild = interface(IRoot)\n",
            "  end;\n",
            "  TBase = class(TObject)\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TFirst = class(TBase, IChild)\n",
            "  end;\n",
            "  TSecond = class(TBase, IChild)\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Required; begin end;\n",
            "end.\n",
        );
        assert!(
            missing_classes(
                inherited_shared,
                "semantic-diagnostics-memo-inherited-shared",
            )
            .is_empty()
        );

        let generic_diamond = concat!(
            "unit SemanticDiagnosticsMemoGenericDiamond;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRoot<T> = interface\n",
            "    procedure Required(Value: T);\n",
            "  end;\n",
            "  ILeft<T> = interface(IRoot<T>)\n",
            "  end;\n",
            "  IRight<T> = interface(IRoot<T>)\n",
            "  end;\n",
            "  TFirst = class(TObject, ILeft<Integer>, IRight<Integer>)\n",
            "  end;\n",
            "  TSecond = class(TObject, ILeft<Integer>, IRight<Integer>)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let diagnostics =
            missing_classes(generic_diamond, "semantic-diagnostics-memo-generic-diamond");
        assert_eq!(
            diagnostics.len(),
            2,
            "generic diamond requirements replay for both classes"
        );
        assert!(
            diagnostics
                .iter()
                .any(|message| message.starts_with("class 'TFirst'"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|message| message.starts_with("class 'TSecond'"))
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_cyclic_class_ancestry() {
        let uri =
            Url::parse("file:///tmp/semantic-diagnostics-class-cycle.pas").expect("cycle URI");
        let source = concat!(
            "unit SemanticDiagnosticsClassCycle;\n",
            "interface\n",
            "type\n",
            "  TFirst = class(TSecond)\n",
            "    procedure Run; override;\n",
            "  end;\n",
            "  TSecond = class(TFirst)\n",
            "    procedure Run; virtual;\n",
            "  end;\n",
            "implementation\n",
            "procedure TFirst.Run; begin end;\n",
            "procedure TSecond.Run; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("cycle fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| { diagnostic.kind != SemanticDiagnosticKind::InvalidOverride }),
            "cyclic ancestry must not produce a confident override claim: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_cyclic_interface_ancestry() {
        let uri =
            Url::parse("file:///tmp/semantic-diagnostics-interface-cycle.pas").expect("cycle URI");
        let source = concat!(
            "unit SemanticDiagnosticsInterfaceCycle;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  ILeft = interface(IRight)\n",
            "    procedure Required;\n",
            "  end;\n",
            "  IRight = interface(ILeft)\n",
            "  end;\n",
            "  TChild = class(TObject, ILeft)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("interface cycle fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "cyclic interface ancestry must remain unknown: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_accept_supported_method_resolution_clauses() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-method-resolution.pas")
            .expect("interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsMethodResolution;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TImplementation = class(TObject, IRequired)\n",
            "    procedure IRequired.Required = DoRequired;\n",
            "    procedure DoRequired;\n",
            "  end;\n",
            "implementation\n",
            "procedure TImplementation.DoRequired; begin end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("method resolution fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "method resolution should satisfy the interface obligation: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_accept_interface_delegation() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-interface-delegation.pas")
            .expect("interface URI");
        let source = concat!(
            "unit SemanticDiagnosticsInterfaceDelegation;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TImplementation = class(TObject, IRequired)\n",
            "  private\n",
            "    FRequired: IRequired;\n",
            "    property Delegate: IRequired read FRequired implements IRequired;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("delegation fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "interface delegation should satisfy the obligation: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_accept_delegation_to_a_derived_interface() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-derived-interface-delegation.pas")
            .expect("derived delegation URI");
        let source = concat!(
            "unit SemanticDiagnosticsDerivedInterfaceDelegation;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRoot = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  IChild = interface(IRoot)\n",
            "  end;\n",
            "  TChild = class(TObject, IChild)\n",
            "  private\n",
            "    F: IChild;\n",
            "    property Delegate: IChild read F implements IChild;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("derived delegation fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "delegating IChild supplies inherited IRoot methods: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_keep_inherited_cross_unit_delegation_context() {
        let provider_uri =
            Url::parse("file:///tmp/semantic-diagnostics-inherited-delegation-provider.pas")
                .expect("provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-diagnostics-inherited-delegation-consumer.pas")
                .expect("consumer URI");
        let provider = concat!(
            "unit InheritedDelegationProvider;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRoot = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  IChild = interface(IRoot)\n",
            "  end;\n",
            "  TBase = class(TObject, IRoot)\n",
            "  private\n",
            "    F: IChild;\n",
            "    property Delegate: IChild read F implements IChild;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit InheritedDelegationConsumer;\n",
            "interface\n",
            "uses InheritedDelegationProvider;\n",
            "type\n",
            "  TChild = class(TBase, IRoot)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("delegation provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("delegation consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("InheritedDelegationProvider".to_owned(), provider_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "inherited delegation must retain its provider owner context: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_defer_an_inherited_abstract_interface_method() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-inherited-abstract-interface.pas")
            .expect("abstract inheritance URI");
        let source = concat!(
            "unit SemanticDiagnosticsInheritedAbstractInterface;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  TBase = class abstract(TObject, IRequired)\n",
            "    procedure Required; virtual; abstract;\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("inherited abstract fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::MissingInterfaceImplementation
            }),
            "an inherited abstract method defers the interface obligation: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_report_only_a_genuinely_missing_obligation_after_abstract_inheritance()
    {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-abstract-inheritance-control.pas")
            .expect("abstract control URI");
        let source = concat!(
            "unit SemanticDiagnosticsAbstractInheritanceControl;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRequired = interface\n",
            "    procedure Required;\n",
            "  end;\n",
            "  IOther = interface\n",
            "    procedure Other;\n",
            "  end;\n",
            "  TBase = class abstract(TObject, IRequired)\n",
            "    procedure Required; virtual; abstract;\n",
            "  end;\n",
            "  TChild = class(TBase, IOther)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("abstract control fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::MissingInterfaceImplementation
                })
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["class 'TChild' does not implement interface method 'Other'"],
            "only the concrete child's unrelated obligation is missing: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_bound_empty_interface_diamond_traversal() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-empty-interface-diamond.pas")
            .expect("empty diamond URI");
        let mut source = String::from(
            "unit SemanticDiagnosticsEmptyInterfaceDiamond;\ninterface\ntype\n  TObject = class\n  end;\n",
        );
        source.push_str("  I0 = interface\n  end;\n  I1 = interface\n  end;\n");
        for index in 2..=26 {
            writeln!(
                &mut source,
                "  I{index} = interface(I{}, I{})\n  end;",
                index - 1,
                index - 2
            )
            .expect("write empty interface diamond");
        }
        source.push_str("  TChild = class(TObject, I26)\n  end;\nimplementation\nend.\n");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("empty diamond fixture parses");
        test_reset_contract_interface_visits();

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");
        let visits = test_contract_interface_visits();

        assert!(
            diagnostics.is_empty() && visits > 0 && visits <= MAX_SEMANTIC_DIAGNOSTIC_WORK,
            "empty interface diamonds must be bounded by the semantic budget; visits={visits}, diagnostics={diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_cancel_during_interface_contract_traversal() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-contract-cancel.pas")
            .expect("contract cancellation URI");
        let source = concat!(
            "unit SemanticDiagnosticsContractCancel;\n",
            "interface\n",
            "type\n",
            "  TObject = class\n",
            "  end;\n",
            "  IRoot = interface\n",
            "  end;\n",
            "  IChild = interface(IRoot)\n",
            "  end;\n",
            "  TChild = class(TObject, IChild)\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("contract cancellation fixture parses");
        test_reset_contract_interface_visits();
        let _cancel_guard = test_cancel_after_contract_interface();
        let cancel = AtomicBool::new(false);

        let result = index.semantic_diagnostics_with_cancel(&uri, &cancel);

        assert_eq!(
            result.expect_err("contract traversal cancellation must discard diagnostics"),
            "request cancelled"
        );
        assert!(cancel.load(Ordering::Relaxed));
        assert!(
            test_contract_interface_visits() > 0,
            "cancellation must occur after a contract traversal visit"
        );
    }

    #[test]
    fn semantic_diagnostics_do_not_call_an_inaccessible_member_missing() {
        let provider_uri = Url::parse("file:///tmp/semantic-diagnostics-access-provider.pas")
            .expect("provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-access-consumer.pas")
            .expect("consumer URI");
        let provider = concat!(
            "unit Provider;\n",
            "interface\n",
            "type\n",
            "  TBox = class\n",
            "  private\n",
            "    Secret: Integer;\n",
            "  public\n",
            "    Value: Integer;\n",
            "  end;\n",
            "  TRec = record\n",
            "    Value: Integer;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses Provider;\n",
            "implementation\n",
            "procedure Run;\n",
            "var Box: TBox; R: TRec;\n",
            "begin\n",
            "  Box.Secret := 1;\n",
            "  R.UnknownName := 2;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("Provider".to_owned(), provider_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "missing member 'UnknownName'")
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "missing member 'Secret'")
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_non_proven_uses_and_pascal_intrinsics() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-controls.pas").expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsControls;\n",
            "interface\n",
            "type\n",
            "  TUnknown = MissingType;\n",
            "var Known: Integer;\n",
            "implementation\n",
            "function Run: Integer;\n",
            "begin\n",
            "  // MissingComment\n",
            "  Known := 1;\n",
            "  Writeln('MissingString');\n",
            "  Abs(Known);\n",
            "  Copy('value', 1, 1);\n",
            "  Result := Known;\n",
            "  Exit;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("controls fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn semantic_diagnostics_suppress_unknown_conditional_branches() {
        let uri =
            Url::parse("file:///tmp/semantic-diagnostics-conditional.pas").expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsConditional;\n",
            "interface\n",
            "var I: Integer; B: Boolean;\n",
            "implementation\n",
            "{$IFDEF UNKNOWN_FEATURE}\n",
            "procedure Run;\n",
            "begin\n",
            "  Missing := 1;\n",
            "  B := I;\n",
            "end;\n",
            "{$ENDIF}\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("conditional fixture parses");

        assert!(
            index
                .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
                .expect("semantic diagnostics complete")
                .is_empty()
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_ambiguous_import_roots() {
        let first_uri = Url::parse("file:///tmp/semantic-diagnostics-provider-a.pas")
            .expect("first provider URI");
        let second_uri = Url::parse("file:///tmp/semantic-diagnostics-provider-b.pas")
            .expect("second provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-diagnostics-ambiguous.pas").expect("consumer URI");
        let provider = "unit Provider;\ninterface\nconst Exported = 1;\nimplementation\nend.\n";
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses Provider;\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  Missing := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(first_uri, provider.to_owned())
            .expect("first provider parses");
        index
            .update(second_uri, provider.to_owned())
            .expect("second provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer parses");

        assert!(
            index
                .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
                .expect("semantic diagnostics complete")
                .is_empty()
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_unknown_with_receivers_and_implicit_ancestry() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-incomplete-bindings.pas")
            .expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsIncompleteBindings;\n",
            "interface\n",
            "type\n",
            "  TBox = class(TUnknown)\n",
            "    procedure Run;\n",
            "  end;\n",
            "implementation\n",
            "procedure TBox.Run;\n",
            "var X: TUnknown;\n",
            "begin\n",
            "  with X do Missing := 1;\n",
            "  AncestorMethod;\n",
            "  Self.AncestorMethod;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("incomplete binding fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                !matches!(
                    diagnostic.message.as_str(),
                    "unresolved identifier 'Missing'" | "unresolved identifier 'AncestorMethod'"
                )
            }),
            "unknown receiver/ancestry must suppress claims: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_implicit_runtime_names_and_type_values() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-implicit-runtime.pas")
            .expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsImplicitRuntime;\n",
            "interface\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  I := Round(1.2);\n",
            "  Writeln(ParamCount);\n",
            "  Writeln(MaxInt);\n",
            "  I := Integer(1);\n",
            "  I := SizeOf(Integer);\n",
            "  I := Low(Integer);\n",
            "  I := High(Integer);\n",
            "  Typoo := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("implicit runtime fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert!(
            messages.is_empty(),
            "implicit System misses stay incomplete: {messages:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_do_not_close_the_unavailable_implicit_system_namespace() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-implicit-system-boundary.pas")
            .expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsImplicitSystemBoundary;\n",
            "interface\n",
            "type TRec = record Value: Integer; end;\n",
            "implementation\n",
            "procedure Run;\n",
            "var S: set of Byte; I: Integer; R: TRec;\n",
            "begin\n",
            "  Exclude(S, 1);\n",
            "  if IsConsole then ReadLn(Input, I);\n",
            "  WriteLn(Output, I);\n",
            "  I := Round(1.2);\n",
            "  I := Integer(1);\n",
            "  I := SizeOf(Integer);\n",
            "  I := Low(Integer);\n",
            "  I := High(Integer);\n",
            "  R.Missing := 1;\n",
            "  R.Integer := 1;\n",
            "  Typoo := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("implicit System boundary fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec!["missing member 'Missing'", "missing member 'Integer'"]
        );
    }

    #[test]
    fn semantic_diagnostics_do_not_treat_assignment_lhs_as_implicit_system_proof() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-assignment-targets.pas")
            .expect("assignment-target URI");
        let source = concat!(
            "unit SemanticDiagnosticsAssignmentTargets;\n",
            "interface\n",
            "var A: array[0..9] of Integer;\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  ExitCode := 0;\n",
            "  RandSeed := 42;\n",
            "  FileMode := 2;\n",
            "  IsMultiThread := True;\n",
            "  A[Random(10)] := 1;\n",
            "  A[Round(1.2)] := 1;\n",
            "  Typoo := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("assignment-target fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "unavailable implicit System namespace cannot prove any assignment miss: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_use_a_known_system_source_without_closing_compiler_exports() {
        let system_uri =
            Url::parse("file:///tmp/semantic-diagnostics-known-system.pas").expect("System URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-known-system-consumer.pas")
            .expect("consumer URI");
        let system = concat!(
            "unit System;\n",
            "interface\n",
            "const KnownSystem = 1; Round = 2;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "implementation\n",
            "procedure Run;\n",
            "var Round: Integer;\n",
            "begin\n",
            "  Round := KnownSystem;\n",
            "  UnknownCompilerExport := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(system_uri.clone(), system.to_owned())
            .expect("known System source parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("known System consumer parses");
        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "known source-backed names resolve while unavailable compiler exports stay open: {diagnostics:?}"
        );
        let known_offset = consumer.find("KnownSystem").expect("known System name");
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &consumer_uri,
                    text::offset_to_position(consumer, known_offset).expect("known position"),
                    &AtomicBool::new(false),
                )
                .expect("known System proof status"),
            SemanticProofStatus::Resolved
        );
        let shadowed_offset = consumer.find("Round :=").expect("shadowed System name");
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &consumer_uri,
                    text::offset_to_position(consumer, shadowed_offset).expect("shadow position"),
                    &AtomicBool::new(false),
                )
                .expect("shadowed System proof status"),
            SemanticProofStatus::Resolved
        );
    }

    #[test]
    fn semantic_diagnostics_keep_qualified_source_backed_system_exports_incomplete() {
        let system_uri = Url::parse("file:///tmp/semantic-diagnostics-qualified-system.pas")
            .expect("System URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-diagnostics-qualified-system-consumer.pas")
                .expect("consumer URI");
        let system = concat!(
            "unit System;\n",
            "interface\n",
            "const KnownSystem = 1;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses System;\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  ExitCode := 0;\n",
            "  I := Round(1.2);\n",
            "  System.ExitCode := 0;\n",
            "  I := System.Round(1.2);\n",
            "  I := System.KnownSystem;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(system_uri.clone(), system.to_owned())
            .expect("source-backed System parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("qualified System consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("System".to_owned(), system_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "source-backed System does not close compiler exports: {diagnostics:?}"
        );
        let known_offset = consumer.find("KnownSystem").expect("known System export");
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &consumer_uri,
                    text::offset_to_position(consumer, known_offset).expect("known position"),
                    &AtomicBool::new(false),
                )
                .expect("known System proof status"),
            SemanticProofStatus::Resolved
        );
    }

    #[test]
    fn semantic_diagnostics_keep_selected_system_domain_open_with_another_retained_system() {
        let first_system_uri =
            Url::parse("file:///tmp/semantic-diagnostics-selected-system-first.pas")
                .expect("first System URI");
        let second_system_uri =
            Url::parse("file:///tmp/semantic-diagnostics-selected-system-second.pas")
                .expect("second System URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-diagnostics-selected-system-consumer.pas")
                .expect("consumer URI");
        let system = concat!(
            "unit System;\n",
            "interface\n",
            "const KnownSystem = 1;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses System;\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  System.ExitCode := 0;\n",
            "  I := System.Round(1.2);\n",
            "  I := System.KnownSystem;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(first_system_uri.clone(), system.to_owned())
            .expect("first System parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("System".to_owned(), first_system_uri)),
        );

        assert!(
            index
                .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
                .expect("single-System diagnostics complete")
                .is_empty(),
            "the selected source-backed System domain stays open"
        );

        index
            .update(second_system_uri, system.to_owned())
            .expect("second System parses");
        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("multi-System diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "another retained System must not close the selected System domain: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_report_a_missing_export_on_a_proven_qualified_unit() {
        let provider_uri = Url::parse("file:///tmp/semantic-diagnostics-qualified-provider.pas")
            .expect("provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-qualified-consumer.pas")
            .expect("consumer URI");
        let provider = concat!(
            "unit Provider;\n",
            "interface\n",
            "var Known: Integer;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses Provider;\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  Provider.Known := 1;\n",
            "  Provider.MissingExport := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("Provider".to_owned(), provider_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert_eq!(diagnostics.len(), 1, "only the missing export is absent");
        assert_eq!(diagnostics[0].kind, SemanticDiagnosticKind::MissingMember);
        assert_eq!(diagnostics[0].message, "missing member 'MissingExport'");
        let missing_offset = consumer.find("MissingExport").expect("missing export");
        assert_eq!(
            diagnostics[0].span,
            SourceSpan {
                start: missing_offset,
                end: missing_offset + "MissingExport".len(),
            }
        );

        let known_offset = consumer.find("Known").expect("known export");
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &consumer_uri,
                    text::offset_to_position(consumer, known_offset).expect("known position"),
                    &AtomicBool::new(false),
                )
                .expect("known proof status"),
            SemanticProofStatus::Resolved
        );
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &consumer_uri,
                    text::offset_to_position(consumer, missing_offset).expect("missing position"),
                    &AtomicBool::new(false),
                )
                .expect("missing proof status"),
            SemanticProofStatus::ProvenAbsent
        );
    }

    #[test]
    fn semantic_diagnostics_report_a_known_incompatible_assignment() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-type-assignment.pas")
            .expect("assignment URI");
        let source = concat!(
            "unit SemanticDiagnosticsTypeAssignment;\n",
            "interface\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer; B: Boolean;\n",
            "begin\n",
            "  B := I;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("assignment fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("assignment diagnostics complete");
        let expected_start = source.rfind("I;").expect("assignment actual");

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message == "type mismatch: cannot assign 'Integer' to 'Boolean'"
                    && diagnostic.span
                        == SourceSpan {
                            start: expected_start,
                            end: expected_start + 1,
                        }
            }),
            "known assignment mismatch must be reported at the actual expression: {diagnostics:?}"
        );
    }

    #[test]
    fn anonymous_callable_signature_mismatch_reports_a_bound_argument() {
        let uri = Url::parse("file:///tmp/LambdaMismatch.pas").unwrap();
        let source = "unit LambdaMismatch;\ninterface\ntype TBoolHandler = reference to procedure(Value: Boolean);\nprocedure Choose(Handler: TBoolHandler);\nimplementation\nprocedure Choose(Handler: TBoolHandler); begin end;\nprocedure Run;\nbegin\n  Choose(procedure(Value: Integer) begin end);\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("callable mismatch fixture parses");
        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("callable diagnostics");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("incompatible argument")
                && diagnostic.span.start
                    == source.find("procedure(Value: Integer) begin end").unwrap()),
            "mismatched anonymous signature lacked an argument diagnostic: {diagnostics:?}"
        );
    }

    #[test]
    fn unsupported_byref_callable_type_does_not_claim_a_signature_mismatch() {
        let uri = Url::parse("file:///tmp/LambdaUnknown.pas").unwrap();
        let source = "unit LambdaUnknown;\ninterface\ntype TByRef = reference to procedure(var Value: Integer);\nprocedure Choose(Handler: TByRef);\nimplementation\nprocedure Choose(Handler: TByRef); begin end;\nprocedure Run;\nbegin\n  Choose(procedure(Value: Integer) begin end);\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("unsupported callable fixture parses");
        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("callable diagnostics");
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("incompatible argument")),
            "unsupported byref signature produced a false mismatch: {diagnostics:?}"
        );
    }

    #[test]
    fn aliased_callable_type_does_not_claim_a_signature_mismatch() {
        let uri = Url::parse("file:///tmp/LambdaAlias.pas").unwrap();
        let source = "unit LambdaAlias;\ninterface\ntype TDirect = reference to procedure(Value: Boolean);\n     TAlias = TDirect;\nprocedure Choose(Handler: TAlias);\nimplementation\nprocedure Choose(Handler: TAlias); begin end;\nprocedure Run;\nbegin\n  Choose(procedure(Value: Integer) begin end);\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("aliased callable fixture parses");
        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("alias diagnostics");
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("incompatible argument")),
            "indirect type alias led to a false mismatch: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_report_an_incompatible_bound_argument() {
        let uri =
            Url::parse("file:///tmp/semantic-diagnostics-type-argument.pas").expect("argument URI");
        let source = concat!(
            "unit SemanticDiagnosticsTypeArgument;\n",
            "interface\n",
            "implementation\n",
            "procedure Take(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  Take(I);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("argument fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("argument diagnostics complete");
        let expected_start = source.rfind("I);").expect("argument actual");

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message == "incompatible argument: expected 'Boolean', found 'Integer'"
                    && diagnostic.span
                        == SourceSpan {
                            start: expected_start,
                            end: expected_start + 1,
                        }
            }),
            "known argument mismatch must be reported at the actual expression: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r1_does_not_reject_integer_narrowing_or_synonyms() {
        let uri = Url::parse("file:///tmp/semantic-correction-r1.pas").expect("R1 URI");
        let source = concat!(
            "unit SemanticCorrectionR1;\n",
            "interface\n",
            "procedure TakeValue(Value: Byte);\n",
            "procedure TakeVar(var Value: Cardinal);\n",
            "procedure TakeOut(out Value: LongWord);\n",
            "implementation\n",
            "procedure Run;\n",
            "var B: Byte; I: Integer; C: Cardinal; L: LongWord;\n",
            "begin\n",
            "  B := I; TakeValue(I); I := C; C := I;\n",
            "  TakeVar(L); TakeOut(C);\n",
            "  B := True; TakeValue(True);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R1 fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R1 diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "type mismatch: cannot assign 'Boolean' to 'Byte'",
                "incompatible argument: expected 'Byte', found 'Boolean'",
            ],
            "integer range-dependent conversions and Cardinal/LongWord synonyms are not proof of incompatibility: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r2_types_character_literals_conservatively() {
        let uri = Url::parse("file:///tmp/semantic-correction-r2.pas").expect("R2 URI");
        let source = concat!(
            "unit SemanticCorrectionR2;\n",
            "interface\n",
            "procedure Take(Value: Char);\n",
            "implementation\n",
            "procedure Run;\n",
            "var C: Char;\n",
            "begin\n",
            "  C := 'a'; Take('a'); C := #65; Take(#65);\n",
            "  C := ''''; Take(''''); Take('ab'); Take('😀');\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R2 fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R2 diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["incompatible argument: expected 'Char', found 'String'"],
            "single-character literals must be Char-compatible while uncertain-width literals stay silent: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r2_counts_logical_characters_across_empty_literal_fragments() {
        let uri = Url::parse("file:///tmp/semantic-round2-r2-composite-char.pas")
            .expect("R2 composite URI");
        let source = concat!(
            "unit SemanticRound2R2CompositeChar;\n",
            "interface\n",
            "procedure Take(Value: Char);\n",
            "implementation\n",
            "procedure Run;\n",
            "var C: Char;\n",
            "begin\n",
            "  C := ''#65; Take(''#65); C := #65''; Take(#65'');\n",
            "  Take('ab');\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R2 composite fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R2 composite diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["incompatible argument: expected 'Char', found 'String'"],
            "empty literal fragments do not add logical characters: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r3_suppresses_unknown_lvalues_and_missing_member_cascades() {
        let uri = Url::parse("file:///tmp/semantic-correction-r3.pas").expect("R3 URI");
        let source = concat!(
            "unit SemanticCorrectionR3;\n",
            "interface\n",
            "type TRec = record Known: Integer; end;\n",
            "procedure Mutate(var Value: Integer);\n",
            "procedure MutateOut(out Value: Integer);\n",
            "implementation\n",
            "procedure Run;\n",
            "var A: array[0..1] of Integer; P: ^Integer; R: TRec; B: Boolean;\n",
            "begin\n",
            "  Mutate(A[0]); MutateOut(A[1]); Mutate(P^);\n",
            "  Mutate(UnknownValue); Mutate(UnknownCall()); Mutate(R.Missing);\n",
            "  Mutate(1); Mutate(B);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "missing member 'Missing'",
                "incompatible argument: expected 'Integer', found 'non-writable expression'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
            ],
            "indexed/dereferenced lvalues are writable, unresolved bindings are unknown, and missing members do not cascade: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r3_suppresses_incomplete_import_and_with_bindings() {
        let uri = Url::parse("file:///tmp/semantic-correction-r3-incomplete.pas")
            .expect("R3 incomplete URI");
        let source = concat!(
            "unit SemanticCorrectionR3Incomplete;\n",
            "interface\n",
            "uses MissingUnit;\n",
            "type TUnknown = MissingType;\n",
            "procedure Mutate(var Value: Integer);\n",
            "implementation\n",
            "procedure Run;\n",
            "var U: TUnknown;\n",
            "begin\n",
            "  Mutate(UnknownValue);\n",
            "  with U do Mutate(Value);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 incomplete fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 incomplete diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "incomplete imports and with receivers must suppress argument claims: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r3_suppresses_unknown_byref_actuals_and_keeps_known_controls() {
        let uri = Url::parse("file:///tmp/semantic-round2-r3-unknown-byref-actual.pas")
            .expect("R3 unknown actual URI");
        let source = concat!(
            "unit SemanticRound2R3UnknownByrefActual;\n",
            "interface\n",
            "type TUnknown = MissingType;\n",
            "procedure Take(var Value: Integer);\n",
            "procedure TakeOut(out Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var B: Boolean; U: TUnknown;\n",
            "begin\n",
            "  Take(B); Take(1); Take(U); TakeOut(U);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 unknown actual fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 unknown actual diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'non-writable expression'",
            ],
            "unresolved by-reference actual types remain unknown: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r3_suppresses_unknown_byref_formals_and_overload_candidates() {
        let uri = Url::parse("file:///tmp/semantic-round2-r3-unknown-byref-formal.pas")
            .expect("R3 unknown formal URI");
        let source = concat!(
            "unit SemanticRound2R3UnknownByrefFormal;\n",
            "interface\n",
            "type TAlias = MissingType;\n",
            "procedure Take(var Value: TAlias);\n",
            "procedure TakeOut(out Value: TAlias);\n",
            "procedure Overloaded(var Value: TAlias); overload;\n",
            "procedure Overloaded(var Value: Boolean); overload;\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  Take(I); TakeOut(I); Overloaded(I);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 unknown formal fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 unknown formal diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "unknown by-reference formals and overload candidates suppress claims: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r3_infers_indexed_and_dereferenced_element_types() {
        let uri =
            Url::parse("file:///tmp/semantic-round2-r3-element-types.pas").expect("R3 element URI");
        let source = concat!(
            "unit SemanticRound2R3ElementTypes;\n",
            "interface\n",
            "procedure TakeInteger(Value: Integer);\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "procedure MutateInteger(var Value: Integer);\n",
            "procedure MutateBoolean(var Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var A: array[0..1] of Boolean; P: ^Boolean; I: Integer;\n",
            "begin\n",
            "  I := A[0]; I := P^;\n",
            "  TakeInteger(A[0]); TakeInteger(P^);\n",
            "  TakeBoolean(A[0]); TakeBoolean(P^);\n",
            "  MutateInteger(A[0]); MutateInteger(P^);\n",
            "  MutateBoolean(A[0]); MutateBoolean(P^);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 element fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 element diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "type mismatch: cannot assign 'Boolean' to 'Integer'",
                "type mismatch: cannot assign 'Boolean' to 'Integer'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
            ],
            "known indexed and dereferenced element types must drive both positive and silent controls: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r3_suppresses_ambiguous_indexed_element_types() {
        let first_uri = Url::parse("file:///tmp/semantic-round2-r3-elements-a.pas")
            .expect("first element provider URI");
        let second_uri = Url::parse("file:///tmp/semantic-round2-r3-elements-b.pas")
            .expect("second element provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-round2-r3-elements-consumer.pas")
            .expect("element consumer URI");
        let first = concat!(
            "unit ElementsA;\n",
            "interface\n",
            "var Values: array[0..1] of Boolean;\n",
            "implementation\n",
            "end.\n",
        );
        let second = concat!(
            "unit ElementsB;\n",
            "interface\n",
            "var Values: array[0..1] of Integer;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit ElementsConsumer;\n",
            "interface\n",
            "uses ElementsA, ElementsB;\n",
            "procedure Take(Value: Integer);\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  Take(Values[0]);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(first_uri.clone(), first.to_owned())
            .expect("first element provider parses");
        index
            .update(second_uri.clone(), second.to_owned())
            .expect("second element provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("element consumer parses");
        index.bind_imports(
            &consumer_uri,
            [
                ("ElementsA".to_owned(), first_uri),
                ("ElementsB".to_owned(), second_uri),
            ],
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("ambiguous element diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "differing candidate element types remain uncertain: {diagnostics:?}"
        );
    }

    #[test]
    fn round3_r3_distinguishes_container_access_from_addressed_storage() {
        let uri = Url::parse("file:///tmp/semantic-round3-r3-addressed-storage.pas")
            .expect("R3 addressed-storage URI");
        let source = concat!(
            "unit SemanticRound3R3AddressedStorage;\n",
            "interface\n",
            "type\n",
            "  PInt = ^Integer;\n",
            "  TStatic = array[0..1] of Integer;\n",
            "  TDynamic = array of Integer;\n",
            "  TInt = Integer;\n",
            "  TBox = class\n",
            "    FValue: Integer;\n",
            "    FPtr: PInt;\n",
            "    FStatic: TStatic;\n",
            "    FDynamic: TDynamic;\n",
            "    property Value: Integer read FValue;\n",
            "    property Ptr: PInt read FPtr;\n",
            "    property StaticItems: TStatic read FStatic;\n",
            "    property Items: TDynamic read FDynamic;\n",
            "  end;\n",
            "procedure Take(var Value: Integer);\n",
            "procedure TakeOut(out Value: Integer);\n",
            "implementation\n",
            "procedure Run;\n",
            "var A: array[0..1] of Integer; P: ^Integer; Box: TBox; Scalar: TInt; B: Boolean;\n",
            "begin\n",
            "  Take(A[0]); TakeOut(P^);\n",
            "  Take(Box.Ptr^); TakeOut(Box.Ptr^); Take(Box.Items[0]);\n",
            "  Take(Box.StaticItems[0]);\n",
            "  Take(Scalar[0]);\n",
            "  Take(Box.Value); TakeOut(Box.Value); Take(1);\n",
            "  B := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 addressed-storage fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 addressed-storage diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "incompatible argument: expected 'Integer', found 'non-writable expression'",
                "incompatible argument: expected 'Integer', found 'non-writable expression'",
                "incompatible argument: expected 'Integer', found 'non-writable expression'",
                "type mismatch: cannot assign 'integer literal' to 'Boolean'",
            ],
            "pointer and dynamic-array property targets are addressed storage, while unsupported static-property elements stay unknown: {diagnostics:?}"
        );
    }

    #[test]
    fn round3_r3_resolves_default_property_result_types_without_container_fallback() {
        let uri = Url::parse("file:///tmp/semantic-round3-r3-default-property.pas")
            .expect("R3 default-property URI");
        let source = concat!(
            "unit SemanticRound3R3DefaultProperty;\n",
            "interface\n",
            "type\n",
            "  TBox = class\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "    procedure SetItem(Index: Integer; Value: Integer);\n",
            "    property Items[Index: Integer]: Integer read GetItem write SetItem; default;\n",
            "  end;\n",
            "  TReadOnlyBox = class\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "    property Items[Index: Integer]: Integer read GetItem; default;\n",
            "  end;\n",
            "  TNoDefault = class\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "    property Items[Index: Integer]: Integer read GetItem;\n",
            "  end;\n",
            "procedure Take(Value: Integer);\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "procedure Mutate(var Value: Integer);\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer; Box: TBox; ReadOnlyBox: TReadOnlyBox; UnknownBox: TNoDefault;\n",
            "begin\n",
            "  I := Box[0]; Take(Box[0]);\n",
            "  I := ReadOnlyBox[0]; Take(ReadOnlyBox[0]);\n",
            "  TakeBoolean(Box[0]); TakeBoolean(ReadOnlyBox[0]);\n",
            "  Mutate(ReadOnlyBox[0]); Mutate(UnknownBox[0]);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 default-property fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 default-property diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "incompatible argument: expected 'Boolean', found 'Integer'",
                "incompatible argument: expected 'Boolean', found 'Integer'",
            ],
            "default getter/setter properties prove their result type, while unsupported default-property storage remains unknown: {diagnostics:?}"
        );
    }

    #[test]
    fn round3_r3_suppresses_outer_element_claims_when_an_index_child_is_unresolved() {
        let uri = Url::parse("file:///tmp/semantic-round3-r3-unresolved-index-child.pas")
            .expect("R3 unresolved-index URI");
        let source = concat!(
            "unit SemanticRound3R3UnresolvedIndexChild;\n",
            "interface\n",
            "type TRec = record Known: Integer; end;\n",
            "procedure Take(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var B: Boolean; A: array[0..1] of Integer; R: TRec;\n",
            "begin\n",
            "  B := A[R.Missing];\n",
            "  Take(A[R.Missing]);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 unresolved-index fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R3 unresolved-index diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["missing member 'Missing'", "missing member 'Missing'",],
            "an unresolved index child prevents an outer element mismatch cascade: {diagnostics:?}"
        );
    }

    #[test]
    fn round3_r3_index_child_inference_honors_cancellation_during_child_traversal() {
        let uri = Url::parse("file:///tmp/semantic-round3-r3-index-cancelled.pas")
            .expect("R3 cancellation URI");
        let source = concat!(
            "unit SemanticRound3R3IndexCancelled;\n",
            "interface\n",
            "procedure Take(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var A: array[0..1] of Integer; I: Integer;\n",
            "begin\n",
            "  I := A[A[A[A[A[A[0]]]]]];\n",
            "  Take(A[A[A[A[A[A[0]]]]]]);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R3 cancellation fixture parses");
        let cancel = AtomicBool::new(false);
        let _cancel_guard = test_cancel_after_semantic_subscript_child();

        let error = index
            .semantic_diagnostics_with_cancel(&uri, &cancel)
            .expect_err("cancelled index inference must stop");

        assert_eq!(error, "request cancelled");
    }

    #[test]
    fn round4_r3_default_property_uses_its_declaring_generic_owner() {
        let uri = Url::parse("file:///tmp/semantic-round4-r3-generic-default.pas")
            .expect("R4 generic default-property URI");
        let source = concat!(
            "unit SemanticRound4R3GenericDefault;\n",
            "interface\n",
            "type\n",
            "  TBase<T> = class\n",
            "    function GetItem(Index: Integer): T;\n",
            "    property Items[Index: Integer]: T read GetItem; default;\n",
            "  end;\n",
            "  TFixed<T> = class(TBase<Integer>) end;\n",
            "  TForward<U> = class(TBase<U>) end;\n",
            "procedure TakeInteger(Value: Integer);\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer; Fixed: TFixed<Boolean>; Forward: TForward<Boolean>; Direct: TBase<Integer>;\n",
            "begin\n",
            "  I := Fixed[0];\n",
            "  TakeInteger(Fixed[0]);\n",
            "  I := Direct[0];\n",
            "  TakeInteger(Direct[0]);\n",
            "  I := Forward[0];\n",
            "  TakeInteger(Forward[0]);\n",
            "  TakeBoolean(Forward[0]);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R4 generic default-property fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R4 generic default-property diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "type mismatch: cannot assign 'Boolean' to 'Integer'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
            ],
            "default-property types must use declaring-owner ancestry substitutions: {diagnostics:?}"
        );
        let fixed_start =
            source.find("I := Forward[0]").expect("forward assignment") + "I := ".len();
        let fixed_call_start = source
            .find("TakeInteger(Forward[0])")
            .expect("forward call")
            + "TakeInteger(".len();
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.span)
                .collect::<Vec<_>>(),
            vec![
                SourceSpan {
                    start: fixed_start,
                    end: fixed_start + "Forward[0]".len(),
                },
                SourceSpan {
                    start: fixed_call_start,
                    end: fixed_call_start + "Forward[0]".len(),
                },
            ],
            "only the substituted descendant should be diagnosed: {diagnostics:?}"
        );
    }

    #[test]
    fn round4_r3_default_property_respects_inaccessible_candidates() {
        let provider_uri = Url::parse("file:///tmp/semantic-round4-r3-private-provider.pas")
            .expect("R4 private provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-round4-r3-private-consumer.pas")
            .expect("R4 private consumer URI");
        let provider = concat!(
            "unit SemanticRound4R3PrivateProvider;\n",
            "interface\n",
            "type\n",
            "  TPrivateBox = class\n",
            "  private\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "    property Items[Index: Integer]: Integer read GetItem; default;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit SemanticRound4R3PrivateConsumer;\n",
            "interface\n",
            "uses SemanticRound4R3PrivateProvider;\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var Imported: TPrivateBox;\n",
            "begin\n",
            "  TakeBoolean(Imported.Items[0]);\n",
            "  TakeBoolean(Imported[0]);\n",
            "end;\n",
            "end.\n",
        );
        let local_uri = Url::parse("file:///tmp/semantic-round4-r3-strict-private.pas")
            .expect("R4 strict-private URI");
        let local = concat!(
            "unit SemanticRound4R3StrictPrivate;\n",
            "interface\n",
            "type\n",
            "  TStrictBox = class\n",
            "  strict private\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "    property Items[Index: Integer]: Integer read GetItem; default;\n",
            "  end;\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var Local: TStrictBox;\n",
            "begin\n",
            "  TakeBoolean(Local.Items[0]);\n",
            "  TakeBoolean(Local[0]);\n",
            "end;\n",
            "end.\n",
        );

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("R4 private provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("R4 private consumer parses");
        index.bind_imports(
            &consumer_uri,
            [("SemanticRound4R3PrivateProvider".to_owned(), provider_uri)],
        );
        index
            .update(local_uri.clone(), local.to_owned())
            .expect("R4 strict-private fixture parses");

        let imported_diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("R4 imported-private diagnostics complete");
        assert!(
            imported_diagnostics.is_empty(),
            "private explicit and default properties must remain inaccessible: {imported_diagnostics:?}"
        );

        let local_diagnostics = index
            .semantic_diagnostics_with_cancel(&local_uri, &AtomicBool::new(false))
            .expect("R4 strict-private diagnostics complete");
        assert!(
            local_diagnostics.is_empty(),
            "strict-private explicit and default properties must remain inaccessible: {local_diagnostics:?}"
        );
    }

    #[test]
    fn round4_r3_requires_a_proven_index_argument_type() {
        let uri = Url::parse("file:///tmp/semantic-round4-r3-index-arguments.pas")
            .expect("R4 index-argument URI");
        let source = concat!(
            "unit SemanticRound4R3IndexArguments;\n",
            "interface\n",
            "type\n",
            "  TValidIndex = Integer;\n",
            "  TUnknownIndex = MissingType;\n",
            "  TCycleA = TCycleB;\n",
            "  TCycleB = TCycleA;\n",
            "function GetIndex(Value: Integer): Integer;\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "procedure TakeInteger(Value: Integer);\n",
            "implementation\n",
            "procedure Run;\n",
            "var B: Boolean; I: Integer; A: array[0..1] of Boolean; Good: TValidIndex; Unknown: TUnknownIndex; Cycle: TCycleA;\n",
            "begin\n",
            "  I := A[Good];\n",
            "  TakeInteger(A[Good]);\n",
            "  B := A[Unknown];\n",
            "  TakeInteger(A[Unknown]);\n",
            "  B := A[Cycle];\n",
            "  TakeInteger(A[Cycle]);\n",
            "  B := A[GetIndex(True)];\n",
            "  TakeBoolean(A[GetIndex(True)]);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R4 index-argument fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R4 index-argument diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "type mismatch: cannot assign 'Boolean' to 'Integer'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
            ],
            "only valid aliases and child argument errors may support outer element claims: {diagnostics:?}"
        );
    }

    #[test]
    fn round5_r3_public_default_property_ignores_unrelated_inaccessible_members() {
        let local_uri = Url::parse("file:///tmp/semantic-round5-r3-public-default.pas")
            .expect("R5 public-default URI");
        let local = concat!(
            "unit SemanticRound5R3PublicDefault;\n",
            "interface\n",
            "type\n",
            "  TFieldBox = class\n",
            "  public\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "    property Items[Index: Integer]: Integer read GetItem; default;\n",
            "  strict private\n",
            "    Unrelated: Integer;\n",
            "  end;\n",
            "  TGetterBox = class\n",
            "  strict private\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "  public\n",
            "    property Items[Index: Integer]: Integer read GetItem; default;\n",
            "  end;\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var B: Boolean; FieldBox: TFieldBox; GetterBox: TGetterBox;\n",
            "begin\n",
            "  B := FieldBox[0];\n",
            "  TakeBoolean(FieldBox[0]);\n",
            "  B := GetterBox[0];\n",
            "  TakeBoolean(GetterBox[0]);\n",
            "end;\n",
            "end.\n",
        );
        let provider_uri = Url::parse("file:///tmp/semantic-round5-r3-private-getter.pas")
            .expect("R5 private-getter provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-round5-r3-imported-default.pas")
            .expect("R5 imported-default URI");
        let provider = concat!(
            "unit SemanticRound5R3PrivateGetter;\n",
            "interface\n",
            "type\n",
            "  TImportedBox = class\n",
            "  strict private\n",
            "    function GetItem(Index: Integer): Integer;\n",
            "  public\n",
            "    property Items[Index: Integer]: Integer read GetItem; default;\n",
            "  end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit SemanticRound5R3ImportedDefault;\n",
            "interface\n",
            "uses SemanticRound5R3PrivateGetter;\n",
            "procedure TakeBoolean(Value: Boolean);\n",
            "implementation\n",
            "procedure Run;\n",
            "var B: Boolean; Imported: TImportedBox;\n",
            "begin\n",
            "  B := Imported[0];\n",
            "  TakeBoolean(Imported[0]);\n",
            "end;\n",
            "end.\n",
        );

        let mut index = NavigationIndex::new();
        index
            .update(local_uri.clone(), local.to_owned())
            .expect("R5 local public-default fixture parses");
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("R5 private-getter provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("R5 imported-default fixture parses");
        index.bind_imports(
            &consumer_uri,
            [("SemanticRound5R3PrivateGetter".to_owned(), provider_uri)],
        );

        let local_diagnostics = index
            .semantic_diagnostics_with_cancel(&local_uri, &AtomicBool::new(false))
            .expect("R5 local public-default diagnostics complete");
        assert_eq!(
            local_diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "type mismatch: cannot assign 'Integer' to 'Boolean'",
                "incompatible argument: expected 'Boolean', found 'Integer'",
                "type mismatch: cannot assign 'Integer' to 'Boolean'",
                "incompatible argument: expected 'Boolean', found 'Integer'",
            ],
            "unrelated private fields and private getters must not poison public default properties: {local_diagnostics:?}"
        );

        let imported_diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("R5 imported public-default diagnostics complete");
        assert_eq!(
            imported_diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "type mismatch: cannot assign 'Integer' to 'Boolean'",
                "incompatible argument: expected 'Boolean', found 'Integer'",
            ],
            "a public property may expose a private getter across units: {imported_diagnostics:?}"
        );
    }

    #[test]
    fn correction_r4_accepts_known_nil_references_and_suppresses_unknown_aliases() {
        let uri = Url::parse("file:///tmp/semantic-correction-r4.pas").expect("R4 URI");
        let source = concat!(
            "unit SemanticCorrectionR4;\n",
            "interface\n",
            "type\n",
            "  PInt = ^Integer;\n",
            "  TProc = procedure;\n",
            "  TIntArray = array of Integer;\n",
            "  TRec = record Value: Integer; end;\n",
            "  TAlias = MissingType;\n",
            "procedure TakeBool(Value: Boolean);\n",
            "procedure TakeP(Value: PInt);\n",
            "procedure TakeProc(Value: TProc);\n",
            "procedure TakeArr(Value: TIntArray);\n",
            "procedure TakeDirectP(Value: ^Integer);\n",
            "procedure TakeDirectProc(Value: procedure);\n",
            "procedure TakeDirectArr(Value: array of Integer);\n",
            "procedure TakeRec(Value: TRec);\n",
            "procedure TakeAlias(Value: TAlias);\n",
            "implementation\n",
            "procedure Run;\n",
            "var P: PInt; F: TProc; A: TIntArray; X: TAlias;\n",
            "begin\n",
            "  P := nil; F := nil; A := nil; X := nil;\n",
            "  TakeP(nil); TakeProc(nil); TakeArr(nil); TakeAlias(nil);\n",
            "  TakeDirectP(nil); TakeDirectProc(nil); TakeDirectArr(nil);\n",
            "  TakeBool(nil); TakeRec(nil);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R4 fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R4 diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "incompatible argument: expected 'Boolean', found 'nil'",
                "incompatible argument: expected 'trec', found 'nil'",
            ],
            "nil compatibility must use affirmative type evidence: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r5_suppresses_unmodeled_record_operators() {
        let uri = Url::parse("file:///tmp/semantic-correction-r5.pas").expect("R5 URI");
        let source = concat!(
            "unit SemanticCorrectionR5;\n",
            "interface\n",
            "type TRec = record class operator Implicit(Value: Integer): TRec; end;\n",
            "procedure Take(Value: TRec);\n",
            "implementation\n",
            "procedure Run;\n",
            "var R: TRec; I: Integer; B: Boolean;\n",
            "begin\n",
            "  R := I; Take(I); B := I;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R5 fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R5 diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["type mismatch: cannot assign 'Integer' to 'Boolean'"],
            "record/operator conversions outside the modeled subset stay unknown: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r5_suppresses_nil_for_records_with_unmodeled_operators() {
        let uri = Url::parse("file:///tmp/semantic-round2-r5-record-nil-operator.pas")
            .expect("R5 record nil URI");
        let source = concat!(
            "unit SemanticRound2R5RecordNilOperator;\n",
            "interface\n",
            "type TRec = record class operator Implicit(Value: Pointer): TRec; end;\n",
            "procedure Take(Value: TRec);\n",
            "implementation\n",
            "procedure Run;\n",
            "var R: TRec; B: Boolean; I: Integer;\n",
            "begin\n",
            "  R := nil; Take(nil); B := I;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("R5 record nil fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("R5 record nil diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["type mismatch: cannot assign 'Integer' to 'Boolean'"],
            "record operator conversions keep nil compatibility uncertain: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r6_resolves_the_implicit_tobject_ancestor() {
        let system_uri =
            Url::parse("file:///tmp/semantic-correction-r6-system.pas").expect("System URI");
        let main_uri = Url::parse("file:///tmp/semantic-correction-r6-main.pas").expect("Main URI");
        let system = concat!(
            "unit System;\n",
            "interface\n",
            "type TObject = class end;\n",
            "implementation\n",
            "end.\n",
        );
        let main = concat!(
            "unit Main;\n",
            "interface\n",
            "uses System;\n",
            "type TChild = class end;\n",
            "procedure Take(Value: TObject);\n",
            "implementation\n",
            "procedure Run;\n",
            "var Obj: TObject; Child: TChild; B: Boolean; I: Integer;\n",
            "begin\n",
            "  Obj := Child; Take(Child); B := I;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(system_uri.clone(), system.to_owned())
            .expect("System fixture parses");
        index
            .update(main_uri.clone(), main.to_owned())
            .expect("R6 fixture parses");
        index.bind_imports(
            &main_uri,
            std::iter::once(("System".to_owned(), system_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&main_uri, &AtomicBool::new(false))
            .expect("R6 diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["type mismatch: cannot assign 'Integer' to 'Boolean'"],
            "a class with no explicit parent has a proven source-backed TObject ancestor: {diagnostics:?}"
        );
    }

    #[test]
    fn round2_r6_resolves_implicit_tobject_when_child_is_in_selected_system_source() {
        let system_uri =
            Url::parse("file:///tmp/semantic-round2-r6-system.pas").expect("System URI");
        let main_uri = Url::parse("file:///tmp/semantic-round2-r6-main.pas").expect("Main URI");
        let system = concat!(
            "unit System;\n",
            "interface\n",
            "type TObject = class end; TChild = class end;\n",
            "implementation\n",
            "end.\n",
        );
        let main = concat!(
            "unit Main;\n",
            "interface\n",
            "uses System;\n",
            "procedure Take(Value: TObject);\n",
            "implementation\n",
            "procedure Run;\n",
            "var Obj: TObject; Child: TChild; B: Boolean; I: Integer;\n",
            "begin\n",
            "  Obj := Child; Take(Child); B := I;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(system_uri.clone(), system.to_owned())
            .expect("same-System fixture parses");
        index
            .update(main_uri.clone(), main.to_owned())
            .expect("same-System consumer parses");
        index.bind_imports(
            &main_uri,
            std::iter::once(("System".to_owned(), system_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&main_uri, &AtomicBool::new(false))
            .expect("same-System diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["type mismatch: cannot assign 'Integer' to 'Boolean'"],
            "the selected System.TObject remains the implicit root for co-located classes: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r7_charges_upcast_parent_resolution_to_the_shared_budget() {
        let provider_uri =
            Url::parse("file:///tmp/semantic-correction-r7-provider.pas").expect("provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-correction-r7-consumer.pas").expect("consumer URI");
        let mut provider = String::from("unit Provider;\ninterface\nconst\n  C0 = 0;\n");
        for index in 1..=3_000 {
            writeln!(&mut provider, "  C{index} = {index};").expect("write provider symbol");
        }
        provider
            .push_str("type TBase = class end; TChild = class(TBase) end;\nimplementation\nend.\n");
        let mut consumer = String::from(
            "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Base: TBase; Child: TChild; B: Boolean; I: Integer;\nbegin\n",
        );
        for _ in 0..30 {
            consumer.push_str("  Base := Child;\n");
        }
        consumer.push_str("  B := I;\nend;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider)
            .expect("provider fixture parses");
        index
            .update(consumer_uri.clone(), consumer)
            .expect("R7 fixture parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("Provider".to_owned(), provider_uri)),
        );
        test_reset_semantic_generic_symbol_visits();

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("R7 diagnostics complete");
        let visits = test_semantic_generic_symbol_visits();

        assert!(
            diagnostics.is_empty() && visits > 0 && visits <= MAX_SEMANTIC_DIAGNOSTIC_WORK,
            "upcast parent lookup must consume the shared budget and discard partial results after {visits} provider visits: {diagnostics:?}"
        );
    }

    #[test]
    fn correction_r7_cancels_during_upcast_ancestry_resolution() {
        let provider_uri = Url::parse("file:///tmp/semantic-correction-r7-cancel-provider.pas")
            .expect("provider cancellation URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-correction-r7-cancel-consumer.pas")
            .expect("consumer cancellation URI");
        let provider = concat!(
            "unit ProviderCancel;\n",
            "interface\n",
            "type TBase = class end; TChild = class(TBase) end;\n",
            "implementation\n",
            "end.\n",
        );
        let consumer = concat!(
            "unit ConsumerCancel;\n",
            "interface\n",
            "uses ProviderCancel;\n",
            "implementation\n",
            "procedure Run;\n",
            "var Base: TBase; Child: TChild; B: Boolean; I: Integer;\n",
            "begin\n",
            "  Base := Child;\n",
            "  B := I;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider cancellation fixture parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer cancellation fixture parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("ProviderCancel".to_owned(), provider_uri)),
        );

        test_reset_semantic_ancestry_visits();
        let _cancel_guard = test_cancel_after_semantic_ancestry();
        let cancel = AtomicBool::new(false);
        let result = index.semantic_diagnostics_with_cancel(&consumer_uri, &cancel);

        assert_eq!(
            result.expect_err("cancellation during ancestry must discard diagnostics"),
            "request cancelled"
        );
        assert!(cancel.load(Ordering::Relaxed));
        assert!(
            test_semantic_ancestry_visits() > 0,
            "the cancellation must occur after ancestry work, not at request entry"
        );
    }

    #[test]
    fn semantic_diagnostics_report_an_extra_argument_without_panicking() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-extra-argument.pas")
            .expect("extra argument URI");
        let source = concat!(
            "unit SemanticDiagnosticsExtraArgument;\n",
            "interface\n",
            "procedure Take(Value: Boolean);\n",
            "implementation\n",
            "procedure Take(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure Run;\n",
            "begin\n",
            "  Take(True, 1);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("extra argument fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("extra argument diagnostics complete");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["incompatible argument: expected 'no parameter', found 'integer literal'"]
        );
    }

    #[test]
    fn semantic_diagnostics_preserve_safe_widening_nil_and_defaulted_arguments() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-type-controls.pas")
            .expect("type controls URI");
        let source = concat!(
            "unit SemanticDiagnosticsTypeControls;\n",
            "interface\n",
            "type TInt = Integer; TBase = class end; TObj = class(TBase) end;\n",
            "implementation\n",
            "procedure TakeBool(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure TakeObject(Value: TObj);\n",
            "begin\n",
            "end;\n",
            "procedure Mutate(var Value: Integer);\n",
            "begin\n",
            "end;\n",
            "procedure MutateOut(out Value: Integer);\n",
            "begin\n",
            "end;\n",
            "procedure Defaults(A, B: Integer; C: Boolean = True);\n",
            "begin\n",
            "end;\n",
            "procedure Run;\n",
            "var I: Integer; R: Real; B: Boolean; Alias: TInt; Obj: TObj; Base: TBase;\n",
            "begin\n",
            "  R := I;\n",
            "  B := I;\n",
            "  B := UnknownValue;\n",
            "  B := Alias;\n",
            "  Base := Obj;\n",
            "  Obj := Base;\n",
            "  Obj := nil;\n",
            "  TakeBool(nil);\n",
            "  TakeObject(nil);\n",
            "  Mutate(1);\n",
            "  Mutate(B);\n",
            "  MutateOut(B);\n",
            "  Defaults(1, 2);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("type controls fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("type controls diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "type mismatch: cannot assign 'Integer' to 'Boolean'",
                "type mismatch: cannot assign 'tbase' to 'tobj'",
                "incompatible argument: expected 'Boolean', found 'nil'",
                "incompatible argument: expected 'Integer', found 'non-writable expression'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
                "incompatible argument: expected 'Integer', found 'Boolean'",
            ]
        );
    }

    #[test]
    fn semantic_diagnostics_require_all_overloads_to_be_incompatible() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-overload-controls.pas")
            .expect("overload controls URI");
        let source = concat!(
            "unit SemanticDiagnosticsOverloadControls;\n",
            "interface\n",
            "type TUnknown = MissingType;\n",
            "procedure Choice(Value: Integer); overload;\n",
            "procedure Choice(Value: Boolean); overload;\n",
            "procedure OneUnknown(Value: Integer); overload;\n",
            "procedure OneUnknown(Value: TUnknown); overload;\n",
            "procedure AllBad(Value: Integer); overload;\n",
            "procedure AllBad(Value: Boolean); overload;\n",
            "implementation\n",
            "procedure Choice(Value: Integer);\n",
            "begin\n",
            "end;\n",
            "procedure Choice(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure OneUnknown(Value: Integer);\n",
            "begin\n",
            "end;\n",
            "procedure OneUnknown(Value: TUnknown);\n",
            "begin\n",
            "end;\n",
            "procedure AllBad(Value: Integer);\n",
            "begin\n",
            "end;\n",
            "procedure AllBad(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  Choice(I);\n",
            "  OneUnknown(I);\n",
            "  AllBad('text');\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("overload controls fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("overload controls diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "incompatible argument: expected 'a compatible overload parameter', found 'String'"
            ]
        );
    }

    #[test]
    fn semantic_diagnostics_fail_closed_when_overload_groups_exceed_the_bound() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-overload-bound.pas")
            .expect("overload bound URI");
        let mut source = String::from("unit SemanticDiagnosticsOverloadBound;\ninterface\ntype\n");
        for index in 0..129 {
            source.push_str(&format!("  T{index} = record end;\n"));
        }
        for index in 0..129 {
            source.push_str(&format!("procedure Choice(Value: T{index}); overload;\n"));
        }
        source.push_str("implementation\nprocedure Run;\nbegin\n  Choice('text');\nend;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("overload bound fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("overload bound diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "an overload set beyond the supported bound must not publish a partial mismatch: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_reuse_known_generic_substitutions_and_suppress_unknown_ones() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-generic-types.pas")
            .expect("generic types URI");
        let source = concat!(
            "unit SemanticDiagnosticsGenericTypes;\n",
            "interface\n",
            "function Pair<T>(Value: T; Flag: Boolean): Integer;\n",
            "implementation\n",
            "function Pair<T>(Value: T; Flag: Boolean): Integer;\n",
            "begin\n",
            "  Result := 0;\n",
            "end;\n",
            "procedure Run;\n",
            "var I: Integer;\n",
            "begin\n",
            "  Pair(I, I);\n",
            "  Pair(UnknownValue, I);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("generic types fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("generic types diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec!["incompatible argument: expected 'Boolean', found 'Integer'"]
        );
        let actual_start =
            source.find("Pair(I, I)").expect("known generic call") + "Pair(I, ".len();
        assert_eq!(
            diagnostics[0].span,
            SourceSpan {
                start: actual_start,
                end: actual_start + 1,
            }
        );
    }

    #[test]
    fn semantic_diagnostics_follow_inherited_and_helper_call_bindings() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-inherited-helper.pas")
            .expect("inherited/helper URI");
        let source = concat!(
            "unit SemanticDiagnosticsInheritedHelper;\n",
            "interface\n",
            "type\n",
            "  TBase = class\n",
            "    procedure Take(Value: Boolean);\n",
            "  end;\n",
            "  TChild = class(TBase)\n",
            "  end;\n",
            "  TChildHelper = class helper for TChild\n",
            "    procedure Help(Value: Boolean);\n",
            "  end;\n",
            "implementation\n",
            "procedure TBase.Take(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure TChildHelper.Help(Value: Boolean);\n",
            "begin\n",
            "end;\n",
            "procedure Run;\n",
            "var I: Integer; Child: TChild;\n",
            "begin\n",
            "  Child.Take(I);\n",
            "  Child.Help(I);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("inherited/helper fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("inherited/helper diagnostics complete");
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            messages,
            vec![
                "incompatible argument: expected 'Boolean', found 'Integer'",
                "incompatible argument: expected 'Boolean', found 'Integer'",
            ]
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_calls_with_ambiguous_member_receivers() {
        let first_uri = Url::parse("file:///tmp/semantic-diagnostics-ambiguous-call-a.pas")
            .expect("first provider URI");
        let second_uri = Url::parse("file:///tmp/semantic-diagnostics-ambiguous-call-b.pas")
            .expect("second provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-ambiguous-call.pas")
            .expect("consumer URI");
        let provider = |unit| {
            format!(
                "unit {unit};\ninterface\ntype TBox = class\n  procedure Take(Value: Boolean);\nend;\nimplementation\nend.\n"
            )
        };
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses A, B;\n",
            "implementation\n",
            "procedure Run;\n",
            "var I: Integer; Box: TBox;\n",
            "begin\n",
            "  Box.Take(I);\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(first_uri.clone(), provider("A"))
            .expect("first provider parses");
        index
            .update(second_uri.clone(), provider("B"))
            .expect("second provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("ambiguous call consumer parses");
        index.bind_imports(
            &consumer_uri,
            [("A".to_owned(), first_uri), ("B".to_owned(), second_uri)],
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("ambiguous call diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::IncompatibleArgument),
            "ambiguous receiver types must suppress argument claims: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_do_not_treat_a_shadowed_unit_name_as_a_unit() {
        let provider_uri = Url::parse("file:///tmp/semantic-diagnostics-shadowed-provider.pas")
            .expect("provider URI");
        let consumer_uri = Url::parse("file:///tmp/semantic-diagnostics-shadowed-consumer.pas")
            .expect("consumer URI");
        let provider = "unit Provider;\ninterface\nvar Known: Integer;\nimplementation\nend.\n";
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses Provider;\n",
            "implementation\n",
            "procedure Run;\n",
            "var Provider: Integer;\n",
            "begin\n",
            "  Provider.MissingExport := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("shadowed consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("Provider".to_owned(), provider_uri)),
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "a shadowed unit name is not a proven unit namespace: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_a_missing_export_for_ambiguous_units() {
        let first_uri = Url::parse("file:///tmp/semantic-diagnostics-ambiguous-provider-a.pas")
            .expect("first provider URI");
        let second_uri = Url::parse("file:///tmp/semantic-diagnostics-ambiguous-provider-b.pas")
            .expect("second provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-diagnostics-ambiguous-provider-consumer.pas")
                .expect("consumer URI");
        let provider = "unit Provider;\ninterface\nvar Known: Integer;\nimplementation\nend.\n";
        let consumer = concat!(
            "unit Consumer;\n",
            "interface\n",
            "uses Provider;\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  Provider.MissingExport := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(first_uri, provider.to_owned())
            .expect("first provider parses");
        index
            .update(second_uri, provider.to_owned())
            .expect("second provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("ambiguous consumer parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "ambiguous unit identity cannot prove a missing export: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_a_missing_export_for_an_unknown_receiver() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-unknown-unit-receiver.pas")
            .expect("consumer URI");
        let source = concat!(
            "unit SemanticDiagnosticsUnknownUnitReceiver;\n",
            "interface\n",
            "implementation\n",
            "procedure Run;\n",
            "begin\n",
            "  UnknownReceiver.MissingExport := 1;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("unknown receiver parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "an unknown receiver cannot prove a missing export: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_suppress_missing_members_from_an_implicit_class_root() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-implicit-class-root.pas")
            .expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsImplicitClassRoot;\n",
            "interface\n",
            "type TBox = class end;\n",
            "implementation\n",
            "procedure Run;\n",
            "var Box: TBox;\n",
            "begin\n",
            "  Box.Free;\n",
            "  Box.Create;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("implicit class root fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.iter().all(|diagnostic| {
                !matches!(
                    diagnostic.message.as_str(),
                    "missing member 'Free'" | "missing member 'Create'"
                )
            }),
            "implicit class root members are not proven absent: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_ignore_qualified_method_declaration_owners() {
        let uri =
            Url::parse("file:///tmp/semantic-diagnostics-method-owner.pas").expect("fixture URI");
        let source = concat!(
            "unit SemanticDiagnosticsMethodOwner;\n",
            "interface\n",
            "type TBox = class\n",
            "  procedure Run;\n",
            "end;\n",
            "implementation\n",
            "procedure TBox.Run;\n",
            "begin\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("method owner fixture parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics.is_empty(),
            "declaration names and owners are not value uses: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_charge_provider_generic_symbol_scans() {
        let provider_uri =
            Url::parse("file:///tmp/semantic-budget-provider.pas").expect("provider URI");
        let consumer_uri =
            Url::parse("file:///tmp/semantic-budget-consumer.pas").expect("consumer URI");
        let mut provider = String::from("unit Provider;\ninterface\nconst\n  C0 = 0;\n");
        for index in 1..=3_000 {
            writeln!(&mut provider, "  C{index} = {index};").expect("write provider symbol");
        }
        provider.push_str(
            "type\n  TRec = record\n    Value: Integer;\n  end;\nvar Box: TRec;\nimplementation\nend.\n",
        );
        let mut consumer = String::from(
            "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n",
        );
        for _ in 0..100 {
            consumer.push_str("  with Box do Value := 1;\n");
        }
        consumer.push_str("  Typoo := 1;\nend;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider)
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer)
            .expect("consumer parses");
        index.bind_imports(
            &consumer_uri,
            std::iter::once(("Provider".to_owned(), provider_uri)),
        );
        test_reset_semantic_generic_symbol_visits();

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
            .expect("bounded semantic diagnostics complete");
        let visits = test_semantic_generic_symbol_visits();

        assert!(
            diagnostics.is_empty() && visits <= MAX_SEMANTIC_DIAGNOSTIC_WORK,
            "budget exhaustion must not publish a partial result after {visits} provider generic-symbol visits: {diagnostics:?}"
        );
    }

    #[test]
    fn semantic_diagnostics_honor_cancellation_before_traversal() {
        let uri =
            Url::parse("file:///tmp/semantic-diagnostics-cancelled.pas").expect("fixture URI");
        let source = "unit SemanticDiagnosticsCancelled;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Missing := 1;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("cancellation fixture parses");
        let cancel = AtomicBool::new(true);

        assert_eq!(
            index
                .semantic_diagnostics_with_cancel(&uri, &cancel)
                .expect_err("cancelled diagnostics must not publish a result"),
            "request cancelled"
        );
    }

    #[test]
    fn semantic_diagnostics_fail_closed_at_the_traversal_limit() {
        let uri = Url::parse("file:///tmp/semantic-diagnostics-limit.pas").expect("fixture URI");
        let mut source = String::from(
            "unit SemanticDiagnosticsLimit;\ninterface\nvar Known: Integer;\nimplementation\nprocedure Run;\nbegin\n",
        );
        for _ in 0..30_000 {
            source.push_str("  Known := 1;\n");
        }
        source.push_str("  Missing := 1;\nend;\nend.\n");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("limit fixture parses");

        assert!(
            index
                .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
                .expect("bounded semantic diagnostics complete")
                .is_empty()
        );
    }

    #[test]
    fn auto_import_context_uses_syntax_not_raw_text_qualification() {
        let cases = [
            (
                "ordinary",
                "unit Context;\ninterface\nimplementation\nvar Value: TTarget;\nend.\n",
                &[][..],
                true,
            ),
            (
                "line-comment-dot",
                "unit Context;\ninterface\nimplementation\nvar Value:\n// .\n  TTarget;\nend.\n",
                &[][..],
                true,
            ),
            (
                "brace-comment-dot",
                "unit Context;\ninterface\nimplementation\nvar Value:\n{ .\n}\n  TTarget;\nend.\n",
                &[][..],
                true,
            ),
            (
                "paren-comment-dot",
                "unit Context;\ninterface\nimplementation\nvar Value:\n(* .\n*)\n  TTarget;\nend.\n",
                &[][..],
                true,
            ),
            (
                "active-directive",
                "unit Context;\ninterface\nimplementation\nvar Value:\n{$IFDEF ENABLED}\n  TTarget;\n{$ENDIF}\nend.\n",
                &["ENABLED".to_owned()][..],
                true,
            ),
            (
                "string-dot",
                "unit Context;\ninterface\nimplementation\nconst Text = 'Provider.Target';\nend.\n",
                &[][..],
                false,
            ),
            (
                "qualified-receiver",
                "unit Context;\ninterface\ntype TBox = class\nend;\nimplementation\nprocedure Run;\nvar Value: TBox;\nbegin\n  Value.Target\nend;\nend.\n",
                &[][..],
                false,
            ),
        ];

        for (name, source, defines, expected) in cases {
            let uri = Url::parse(&format!("file:///tmp/auto-import-{name}.pas"))
                .expect("context fixture URI");
            let position = text::offset_to_position(
                source,
                source.rfind("Target").expect("context fixture target") + "Target".len(),
            )
            .expect("context fixture position");
            let mut index = NavigationIndex::new();
            index
                .update_with_defines(uri.clone(), source.to_owned(), defines)
                .expect("context fixture parses");
            let actual = index
                .completion_context_may_auto_import(&uri, position, &AtomicBool::new(false))
                .expect("context classification");
            assert_eq!(
                actual, expected,
                "unexpected auto-import context for {name}"
            );
        }
    }

    #[test]
    fn budgeted_uses_lookup_charges_ast_traversal_before_identifier_materialization() {
        let source =
            "unit UsesBudgetConsumer;\ninterface\nuses BudgetProvider;\nimplementation\nend.\n";
        let uri = Url::parse("file:///tmp/uses-budget-consumer.pas").expect("fixture URI");
        let cancel = AtomicBool::new(false);
        let document = Document::parse_with_cancel(
            uri,
            source.to_owned(),
            &ConditionalContext::default(),
            &cancel,
            None,
        )
        .expect("uses-budget fixture parses");
        let module_name = collect_nodes_matching(document.tree.root_node(), "moduleName")
            .into_iter()
            .find(|node| has_ancestor_kind(*node, "declUses"))
            .expect("uses module name");

        let mut count_budget = AssistanceBudget::new(4096, 4096, "uses traversal");
        let node_count = count_nodes_with_budget(module_name, &cancel, &mut count_budget)
            .expect("count uses nodes");
        let exact_work = node_count
            .saturating_add(node_count.saturating_mul(3))
            .saturating_add(3);
        let mut exact_budget = AssistanceBudget::new(exact_work, 4096, "uses lookup");
        assert_eq!(
            use_name_at_with_budget(module_name, &document.source, &cancel, &mut exact_budget,)
                .expect("exact uses lookup budget"),
            Some("budgetprovider".to_owned())
        );
        assert_eq!(exact_budget.remaining_work, 0);
    }

    #[test]
    fn wide_uses_lookup_charges_each_child_before_materializing_a_frontier() {
        let parts = (0..64)
            .map(|index| format!("UnitPart{index}"))
            .collect::<Vec<_>>();
        let source = format!(
            "unit WideUsesBudget;\ninterface\nuses {};\nimplementation\nend.\n",
            parts.join(".")
        );
        let uri = Url::parse("file:///tmp/wide-uses-budget.pas").expect("fixture URI");
        let cancel = AtomicBool::new(false);
        let document =
            Document::parse_with_cancel(uri, source, &ConditionalContext::default(), &cancel, None)
                .expect("wide uses fixture parses");
        let module_name = collect_nodes_matching(document.tree.root_node(), "moduleName")
            .into_iter()
            .find(|node| has_ancestor_kind(*node, "declUses"))
            .expect("wide uses module name");
        assert!(
            module_name.named_child_count() >= 8,
            "wide module name was not parsed"
        );

        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut zero_budget = AssistanceBudget::new(0, 4096, "wide uses zero budget");
        assert!(count_nodes_with_budget(module_name, &cancel, &mut zero_budget).is_err());
        assert_eq!(
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(Cell::get),
            0,
            "zero remaining work must reject before entering the module children"
        );

        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut one_budget = AssistanceBudget::new(1, 4096, "wide uses one budget");
        assert!(count_nodes_with_budget(module_name, &cancel, &mut one_budget).is_err());
        assert!(
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(Cell::get) <= 1,
            "one remaining unit must not materialize the whole sibling frontier"
        );

        let mut count_budget = AssistanceBudget::new(4096, 4096, "wide uses count");
        let node_count = count_nodes_with_budget(module_name, &cancel, &mut count_budget)
            .expect("count wide uses nodes");
        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut exact_budget = AssistanceBudget::new(node_count, 4096, "wide uses exact budget");
        assert_eq!(
            count_nodes_with_budget(module_name, &cancel, &mut exact_budget)
                .expect("exact wide uses budget"),
            node_count
        );
        assert_eq!(exact_budget.remaining_work, 0);

        let cancelled = AtomicBool::new(true);
        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut cancelled_budget = AssistanceBudget::new(4096, 4096, "wide uses cancelled");
        assert!(count_nodes_with_budget(module_name, &cancelled, &mut cancelled_budget).is_err());
        assert_eq!(
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(Cell::get),
            0,
            "cancelled traversal must not enter a child frontier"
        );
    }

    #[test]
    fn conditional_unknown_status_is_precomputed_per_symbol() {
        let source = "unit CachedConditionalStatus;\ninterface\nvar\n{$IFDEF MAYBE}\n  Hidden: Integer;\n{$ENDIF}\n  Visible: Integer;\nimplementation\nend.\n";
        let uri = Url::parse("file:///tmp/cached-conditional-status.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("conditional symbol fixture parses");
        let document = index.documents.get(&uri).expect("indexed document");
        let hidden = document
            .symbols
            .iter()
            .position(|symbol| symbol.name.eq_ignore_ascii_case("Hidden"))
            .expect("hidden symbol");
        let visible = document
            .symbols
            .iter()
            .position(|symbol| symbol.name.eq_ignore_ascii_case("Visible"))
            .expect("visible symbol");

        assert_eq!(
            document.conditional_unknown_symbols.len(),
            document.symbols.len()
        );
        assert!(document.conditional_unknown_symbols[hidden]);
        assert!(!document.conditional_unknown_symbols[visible]);
        assert!(index.candidate_is_conditionally_unknown(&Candidate {
            uri: uri.clone(),
            index: hidden,
        }));
        assert!(!index.candidate_is_conditionally_unknown(&Candidate {
            uri,
            index: visible
        }));
    }

    #[test]
    fn unknown_class_owner_cache_survives_a_large_method_body() {
        let mut source = String::from(
            "unit CachedUnknownOwner;\ninterface\ntype\n  TObj = class(TUnknown)\n    procedure Caller;\n  end;\nconst\n  GlobalName = 1;\nimplementation\nprocedure TObj.Caller;\nbegin\n  Glo;\n",
        );
        for _ in 0..120_000 {
            source.push_str("  WriteLn(1);\n");
        }
        source.push_str("end;\nend.\n");
        let uri = Url::parse("file:///tmp/cached-unknown-owner.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("large unknown-owner fixture parses");
        let document = index.documents.get(&uri).expect("indexed document");
        let offset = source.find("  Glo;").expect("global prefix") + "  Glo;".len();
        assert_eq!(
            document.owner_type_at(offset).as_deref(),
            Some("tobj"),
            "scopes: {:?}",
            document.scopes
        );
        assert!(document.unknown_class_owners.contains("tobj"));
    }

    #[test]
    fn inherited_member_dag_uses_bounded_memoized_expansion() {
        const DAG_WIDTH: usize = 37;
        let mut source = String::from(
            "unit InheritedMemberDag;\ninterface\ntype\n  I0 = interface\n    procedure Hit;\n  end;\n",
        );
        for index in 1..DAG_WIDTH {
            if index == 1 {
                writeln!(&mut source, "  I{index} = interface(I0)\n  end;")
                    .expect("write DAG node");
            } else {
                writeln!(
                    &mut source,
                    "  I{index} = interface(I{}, I{})\n  end;",
                    index - 1,
                    index - 2
                )
                .expect("write DAG node");
            }
        }
        source.push_str(
            "implementation\nprocedure Caller;\nvar\n  Obj: I36;\nbegin\n  Obj.Hit;\nend;\nend.\n",
        );
        let uri = Url::parse("file:///tmp/inherited-member-dag.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("inherited member DAG parses");

        let mut state = AncestryResolutionState::new();
        assert_eq!(
            index.resolve_type_ancestry(&uri, "i36", &mut state).status,
            AncestryStatus::Complete
        );
        let lookup = index.member_candidates_for_type_with_state(
            &uri,
            "i36",
            ROOT_SCOPE,
            Some("hit"),
            false,
            &mut state,
        );
        assert!(lookup.ancestry_known);
        assert_eq!(lookup.candidates.len(), 1);
        assert_eq!(
            index
                .symbol(&lookup.candidates[0])
                .map(|symbol| symbol.name.as_str()),
            Some("Hit")
        );
        assert_eq!(
            state.remaining_work,
            MAX_ANCESTRY_WORK.saturating_sub(DAG_WIDTH.saturating_mul(2)),
            "each type and type/member pair must consume one bounded expansion"
        );
    }

    #[test]
    fn unit_binding_locations_use_selected_provider_identity_and_bound_prefix_ranges() {
        let provider_uri = Url::parse("file:///workspace/provider.pas").expect("provider URI");
        let consumer_uri = Url::parse("file:///workspace/consumer.pas").expect("consumer URI");
        let provider =
            "unit Ns.Provider;\ninterface\ntype\n  TThing = class\n  end;\nimplementation\nend.\n";
        let consumer = "unit Consumer;\ninterface\nuses Alias;\nimplementation\nprocedure Run;\nbegin\n  Alias.TThing;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer parses");
        index.bind_imports(&consumer_uri, [("Alias".to_owned(), provider_uri.clone())]);

        let locations = index
            .binding_locations(&provider_uri, Position::new(0, 8), true)
            .expect("unit references resolve");

        assert_eq!(
            locations,
            vec![
                Location::new(
                    consumer_uri.clone(),
                    Range::new(Position::new(2, 5), Position::new(2, 10)),
                ),
                Location::new(
                    consumer_uri,
                    Range::new(Position::new(6, 2), Position::new(6, 7)),
                ),
                Location::new(
                    provider_uri,
                    Range::new(Position::new(0, 5), Position::new(0, 16)),
                ),
            ]
        );
    }

    #[test]
    fn document_highlights_follow_selected_binding_and_classify_assignments() {
        let uri = Url::parse("file:///workspace/highlight-kinds.pas").expect("source URI");
        let source = "unit HighlightKinds;\ninterface\nimplementation\nprocedure Run;\nvar\n  X, Y: Integer;\nbegin\n  X := Y;\n  Y := X;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("highlight source parses");
        let highlights_for = |position| {
            let mut budget = BindingWorkBudget::new(100_000);
            index
                .binding_highlights_in_document_with_cancel_and_work_budget(
                    &uri,
                    position,
                    &AtomicBool::new(false),
                    &mut budget,
                )
                .expect("highlight roles resolve")
        };

        assert_eq!(
            highlights_for(Position::new(5, 2)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(5, 2), Position::new(5, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(7, 2), Position::new(7, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(8, 7), Position::new(8, 8)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
        assert_eq!(
            highlights_for(Position::new(5, 5)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(5, 5), Position::new(5, 6)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(7, 7), Position::new(7, 8)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(8, 2), Position::new(8, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
            ]
        );
    }

    #[test]
    fn document_highlights_classify_ast_storage_and_call_modes() {
        let uri = Url::parse("file:///workspace/highlight-debug.pas").expect("source URI");
        let source = "unit HighlightDebug;\ninterface\nimplementation\nprocedure Take(var V: Integer; const C: Integer; out O: Integer);\nbegin\nend;\nprocedure Run;\nvar\n  X, Y, Z: Integer;\n  A: array[0..3] of Integer;\n  P: ^Integer;\nbegin\n  X := Y;\n  Take(X, Y, Z);\n  A[X] := Y;\n  P^ := X;\n  for X := Y to Z do\n    Y := X;\n  Inc(X);\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("highlight source parses");
        let highlights_for = |position| {
            let mut budget = BindingWorkBudget::new(100_000);
            index
                .binding_highlights_in_document_with_cancel_and_work_budget(
                    &uri,
                    position,
                    &AtomicBool::new(false),
                    &mut budget,
                )
                .expect("highlight roles resolve")
        };

        assert_eq!(
            highlights_for(Position::new(8, 2)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(8, 2), Position::new(8, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(12, 2), Position::new(12, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(13, 7), Position::new(13, 8)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(14, 4), Position::new(14, 5)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(15, 8), Position::new(15, 9)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(16, 6), Position::new(16, 7)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(17, 9), Position::new(17, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(18, 6), Position::new(18, 7)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
            ]
        );
        assert_eq!(
            highlights_for(Position::new(9, 2)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(9, 2), Position::new(9, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(14, 2), Position::new(14, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
        assert_eq!(
            highlights_for(Position::new(10, 2)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(10, 2), Position::new(10, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(15, 2), Position::new(15, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
    }

    #[test]
    fn document_highlights_resolve_call_identity_before_intrinsic_names() {
        let uri =
            Url::parse("file:///workspace/highlight-intrinsic-shadow.pas").expect("source URI");
        let source = "unit HighlightIntrinsicShadow;\ninterface\ntype\n  TBox = class\n    procedure Inc(const V: Integer);\n  end;\nprocedure Inc(const V: Integer);\nimplementation\nprocedure TBox.Inc(const V: Integer);\nbegin\nend;\nprocedure Inc(const V: Integer);\nbegin\nend;\nprocedure Run;\nvar\n  B: TBox;\n  X: Integer;\nbegin\n  Inc(X);\n  B.Inc(X);\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("intrinsic-shadow source parses");
        let mut budget = BindingWorkBudget::new(100_000);
        let highlights = index
            .binding_highlights_in_document_with_cancel_and_work_budget(
                &uri,
                Position::new(17, 2),
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("resolved call identities classify highlights");

        assert_eq!(
            highlights,
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(17, 2), Position::new(17, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(19, 6), Position::new(19, 7)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(20, 8), Position::new(20, 9)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
    }

    #[test]
    fn document_highlights_preserve_nested_call_storage_semantics() {
        let uri = Url::parse("file:///workspace/highlight-nested-call.pas").expect("source URI");
        let source = "unit HighlightNestedCall;\ninterface\nimplementation\nfunction Change(var V: Integer): Integer;\nbegin\n  Result := V;\nend;\nprocedure Run;\nvar\n  X: Integer;\n  A: array[0..4] of Integer;\nbegin\n  A[Change(X)] := 1;\n  A[Unknown(X)] := 1;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("nested-call source parses");
        let mut budget = BindingWorkBudget::new(100_000);
        let highlights = index
            .binding_highlights_in_document_with_cancel_and_work_budget(
                &uri,
                Position::new(9, 2),
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("nested call roles resolve");

        assert_eq!(
            highlights,
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(9, 2), Position::new(9, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(12, 11), Position::new(12, 12)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(13, 12), Position::new(13, 13)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
            ]
        );
    }

    #[test]
    fn document_highlights_bound_late_writable_binding_work_once() {
        let uri = Url::parse("file:///workspace/highlight-late-writable.pas").expect("source URI");
        let mut source = String::from(
            "unit HighlightLateWritable;\ninterface\nimplementation\nprocedure Run;\nvar\n",
        );
        for index in 0..256 {
            writeln!(&mut source, "  Before{index}: Integer;").expect("write preceding symbol");
        }
        source.push_str("  Target: Integer;\nbegin\n");
        for _ in 0..64 {
            source.push_str("  Target := Target + 1;\n");
        }
        source.push_str("end;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("late-writable source parses");
        let mut occurrence_budget = BindingWorkBudget::new(100_000);
        let mut semantic_budget =
            AssistanceBudget::new(10_000, 8 * 1024 * 1024, "late writable highlights");
        let highlights = index
            .binding_highlights_in_document_with_cancel_and_work_budget_and_shared_budget(
                &uri,
                Position::new(261, 2),
                &AtomicBool::new(false),
                &mut occurrence_budget,
                &mut semantic_budget,
            )
            .expect("late writable binding should stay within the shared budget");

        assert_eq!(highlights.len(), 129);
        assert!(
            semantic_budget.remaining_work > 0,
            "writability must be charged once, not once per occurrence"
        );
    }

    #[test]
    fn document_highlights_keep_uncertain_property_storage_text() {
        let uri = Url::parse("file:///workspace/highlight-members-debug.pas").expect("source URI");
        let source = "unit HighlightMembersDebug;\ninterface\ntype\n  TBox = class\n    F: Integer;\n    procedure SetP(Value: Integer);\n    function GetP: Integer;\n    property P: Integer read GetP write SetP;\n    property ReadOnly: Integer read GetP;\n  end;\nimplementation\nprocedure TBox.SetP(Value: Integer);\nbegin\nend;\nfunction TBox.GetP: Integer;\nbegin\n  Result := 0;\nend;\nprocedure Run;\nvar\n  B: TBox;\n  X: Integer;\nbegin\n  B.F := X;\n  X := B.F;\n  B.P := X;\n  X := B.P;\n  B.ReadOnly := X;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("highlight source parses");
        let highlights_for = |position| {
            let mut budget = BindingWorkBudget::new(100_000);
            index
                .binding_highlights_in_document_with_cancel_and_work_budget(
                    &uri,
                    position,
                    &AtomicBool::new(false),
                    &mut budget,
                )
                .expect("highlight roles resolve")
        };

        assert_eq!(
            highlights_for(Position::new(21, 2)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(21, 2), Position::new(21, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(23, 9), Position::new(23, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(24, 2), Position::new(24, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(25, 9), Position::new(25, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(26, 2), Position::new(26, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(27, 16), Position::new(27, 17)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
        assert_eq!(
            highlights_for(Position::new(4, 4)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(4, 4), Position::new(4, 5)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(23, 4), Position::new(23, 5)),
                    kind: Some(lsp_types::DocumentHighlightKind::WRITE),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(24, 9), Position::new(24, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
        assert_eq!(
            highlights_for(Position::new(7, 13)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(7, 13), Position::new(7, 14)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(25, 4), Position::new(25, 5)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(26, 9), Position::new(26, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::READ),
                },
            ]
        );
        assert_eq!(
            highlights_for(Position::new(8, 13)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(8, 13), Position::new(8, 21)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(27, 4), Position::new(27, 12)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
            ]
        );
    }

    #[test]
    fn document_highlights_keep_ambiguous_overload_arguments_text() {
        let uri = Url::parse("file:///workspace/highlight-overload.pas").expect("source URI");
        let source = "unit HighlightOverload;\ninterface\nprocedure Ambiguous(var V: Integer); overload;\nprocedure Ambiguous(const V: Integer); overload;\nimplementation\nprocedure Ambiguous(var V: Integer);\nbegin\nend;\nprocedure Ambiguous(const V: Integer);\nbegin\nend;\nprocedure Run;\nvar\n  X: Integer;\nbegin\n  Ambiguous(X);\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("overload fixture parses");
        let mut budget = BindingWorkBudget::new(100_000);
        let highlights = index
            .binding_highlights_in_document_with_cancel_and_work_budget(
                &uri,
                Position::new(15, 12),
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("ambiguous overload highlight roles resolve");

        assert_eq!(
            highlights,
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(13, 2), Position::new(13, 3)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(15, 12), Position::new(15, 13)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
            ]
        );
    }

    #[test]
    fn document_highlights_use_complete_unit_prefixes() {
        let provider_uri =
            Url::parse("file:///workspace/highlight-provider.pas").expect("provider URI");
        let consumer_uri =
            Url::parse("file:///workspace/highlight-consumer.pas").expect("consumer URI");
        let provider =
            "unit Ns.Provider;\ninterface\ntype\n  TThing = class\n  end;\nimplementation\nend.\n";
        let consumer = "unit Consumer;\ninterface\nuses Alias;\nimplementation\nprocedure Run;\nbegin\n  Alias.TThing;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("consumer parses");
        index.bind_imports(&consumer_uri, [("Alias".to_owned(), provider_uri.clone())]);

        let highlights_for = |uri: &Url, position| {
            let mut budget = BindingWorkBudget::new(100_000);
            index
                .binding_highlights_in_document_with_cancel_and_work_budget(
                    uri,
                    position,
                    &AtomicBool::new(false),
                    &mut budget,
                )
                .expect("unit highlight roles resolve")
        };

        assert_eq!(
            highlights_for(&provider_uri, Position::new(0, 8)),
            vec![lsp_types::DocumentHighlight {
                range: Range::new(Position::new(0, 5), Position::new(0, 16)),
                kind: Some(lsp_types::DocumentHighlightKind::TEXT),
            }]
        );
        assert_eq!(
            highlights_for(&consumer_uri, Position::new(2, 5)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(2, 5), Position::new(2, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(6, 2), Position::new(6, 7)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
            ]
        );
        assert_eq!(
            highlights_for(&consumer_uri, Position::new(6, 2)),
            vec![
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(2, 5), Position::new(2, 10)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
                lsp_types::DocumentHighlight {
                    range: Range::new(Position::new(6, 2), Position::new(6, 7)),
                    kind: Some(lsp_types::DocumentHighlightKind::TEXT),
                },
            ]
        );
    }
}
