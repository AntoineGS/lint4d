use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use cfg_core::{BasicBlockKind, BlockId, Cfg, CfgBuildSink, DefaultCfgBuilder, EdgeKind, StmtRef};
use tree_sitter::Node;

use crate::constructs::{
    exit_has_argument, is_break_call, is_continue_call, is_exit_call, node_text,
    raise_may_throw_during_evaluation, raised_exception_type, Flow, LabelBindingId, LoopFrame,
    PendingTransfer, ScopeId, TransferKind,
};
use crate::exception_types::{ExceptionTypeFact, ExceptionTypeIndex, TypeMatch, UnitKey};
use crate::project::{ProjectBuildError, ProjectSnapshot, ProjectUnitId};

/// Build CFGs for all executable routine definitions in a parsed Pascal file.
///
/// Walks the tree looking for `defProc` nodes, extracts each routine name and
/// body block, then builds a CFG for each one. Top-level routine names retain
/// their source spelling. A nested routine is qualified with its lexical
/// parents, such as `Outer.Inner` or `TClass.Method.Inner`.
///
/// CFGs are returned in stable lexical pre-order (equivalently, by executable
/// scope start byte): each routine precedes its nested descendants, and
/// siblings retain source order. A routine's byte range covers its complete
/// `defProc` node, including its declaration, nested declarations, and
/// executable body.
///
/// Programs and libraries also receive a synthetic `<module>.<main>` CFG when
/// they contain a main `begin..end` body. Units receive one synthetic
/// `<module>.<initialization>` or `<module>.<finalization>` CFG for each
/// corresponding section node, including empty sections. A legacy unit
/// `implementation` followed by a direct `begin..end` body is exposed as the
/// initialization CFG. Synthetic section ranges cover only the executable
/// node: the main or legacy block's `begin..end` span, or the section keyword
/// through its last statement. Module headers, declarations, and the
/// program/library final `.` are excluded.
/// No main CFG is emitted for a body-less library, and no unit section CFG is
/// emitted when the corresponding section is absent.
///
/// Typed exception dispatch is precise only for proven, non-generic class
/// constructors and transparent aliases. The legacy entrypoint resolves
/// same-file declarations; [`build_file_cfgs_in_project`] additionally uses
/// the caller's explicit loaded-unit/import snapshot. Missing or unavailable
/// imports, unresolved method owners, class/value ambiguity, implicit `with`
/// members, and preprocessor directives remain conservative exception
/// alternatives; a preprocessor directive is a file-wide barrier because the
/// Pascal grammar exposes it as a sibling extra. Ordinary comments do not
/// disable resolution.
///
pub fn build_file_cfgs(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<Cfg> {
    let root = tree.root_node();
    let exception_types = ExceptionTypeIndex::build(root, source);
    let unit_key = ExceptionTypeIndex::singleton_unit_key();
    let mut cfgs = Vec::new();
    collect_def_proc_cfgs(root, source, None, &unit_key, &exception_types, &mut cfgs);
    collect_module_cfgs(root, source, &unit_key, &exception_types, &mut cfgs);
    cfgs.sort_by_key(|cfg| cfg.byte_range.start);
    cfgs
}

/// Build CFGs for one unit selected from an immutable project snapshot.
///
/// The snapshot owns the exact tree/source pair consumed by this call. Import
/// targets are never inferred from names: callers provide one [`crate::project::ImportBinding`]
/// per `uses` entry, including the qualifiers that are authorized for that
/// occurrence. A missing or ambiguous target remains conservative.
///
/// ```rust
/// use cfg_pascal::{
///     build_file_cfgs_in_project, ProjectSnapshot, ProjectSourceId, ProjectUnitId,
///     ProjectUnitInput,
/// };
/// use tree_sitter::Parser;
///
/// let source = b"unit Demo; interface implementation end.";
/// let mut parser = Parser::new();
/// parser.set_language(&cfg_pascal::LANGUAGE.into()).unwrap();
/// let tree = parser.parse(source, None).unwrap();
/// let input = ProjectUnitInput::new(
///     ProjectUnitId::from("demo"),
///     ProjectSourceId::from("demo.pas"),
///     tree,
///     source,
/// );
/// let snapshot = ProjectSnapshot::new(vec![input], Vec::new()).unwrap();
/// let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::from("demo")).unwrap();
/// assert!(cfgs.is_empty());
/// ```
///
pub fn build_file_cfgs_in_project(
    snapshot: &ProjectSnapshot,
    unit_id: &ProjectUnitId,
) -> Result<Vec<Cfg>, ProjectBuildError> {
    let unit = snapshot
        .unit(unit_id)
        .ok_or_else(|| ProjectBuildError::UnitNotFound(unit_id.clone()))?;
    let exception_types = ExceptionTypeIndex::build_project(snapshot);
    let unit_key = UnitKey::from_project(unit_id);
    let root = unit.tree().root_node();
    let mut cfgs = Vec::new();
    collect_def_proc_cfgs(
        root,
        unit.source(),
        None,
        &unit_key,
        &exception_types,
        &mut cfgs,
    );
    collect_module_cfgs(root, unit.source(), &unit_key, &exception_types, &mut cfgs);
    cfgs.sort_by_key(|cfg| cfg.byte_range.start);
    Ok(cfgs)
}

fn collect_def_proc_cfgs(
    node: Node,
    source: &[u8],
    parent_qualified_name: Option<&str>,
    unit_key: &UnitKey,
    exception_types: &ExceptionTypeIndex,
    out: &mut Vec<Cfg>,
) {
    if node.kind() == "defProc" {
        let Some(local_name) = extract_proc_name(node, source) else {
            return;
        };
        let full_local_name =
            extract_full_proc_name(node, source).unwrap_or_else(|| local_name.clone());
        let qualified_name = parent_qualified_name
            .map(|parent| format!("{parent}.{full_local_name}"))
            .unwrap_or_else(|| full_local_name.clone());
        let proc_name = parent_qualified_name
            .map(|_| qualified_name.clone())
            .unwrap_or(local_name);

        if let Some(cfg) = build_proc_cfg(node, source, proc_name, unit_key, exception_types) {
            out.push(cfg);
        }

        // A nested routine is a separate executable scope. Collect only the
        // definitions stored in `local`; in particular, never recurse into
        // the routine body while collecting descendants, or its statements
        // would be mistaken for part of the containing routine.
        for child in field_children(node, "local") {
            collect_def_proc_cfgs(
                child,
                source,
                Some(&qualified_name),
                unit_key,
                exception_types,
                out,
            );
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_def_proc_cfgs(
            child,
            source,
            parent_qualified_name,
            unit_key,
            exception_types,
            out,
        );
    }
}

/// Build a CFG for a single `defProc` node.
fn build_proc_cfg(
    def_proc: Node,
    source: &[u8],
    proc_name: String,
    unit_key: &UnitKey,
    exception_types: &ExceptionTypeIndex,
) -> Option<Cfg> {
    let block = def_proc.child_by_field_name("body")?;

    let byte_range = def_proc.start_byte()..def_proc.end_byte();
    Some(build_scope_cfg(
        proc_name,
        byte_range,
        block,
        ScopeBody::Block,
        source,
        unit_key,
        exception_types,
    ))
}

fn collect_module_cfgs(
    node: Node,
    source: &[u8],
    unit_key: &UnitKey,
    exception_types: &ExceptionTypeIndex,
    out: &mut Vec<Cfg>,
) {
    let Some(module) = direct_named_child(node, ["program", "library", "unit"]) else {
        return;
    };
    let Some(module_name) = extract_module_name(module, source) else {
        return;
    };

    match module.kind() {
        "program" | "library" => {
            if let Some(body) = direct_child(module, "block") {
                out.push(build_scope_cfg(
                    format!("{module_name}.<main>"),
                    body.start_byte()..body.end_byte(),
                    body,
                    ScopeBody::Block,
                    source,
                    unit_key,
                    exception_types,
                ));
            }
        }
        "unit" => {
            let mut cursor = module.walk();
            for section in module.named_children(&mut cursor) {
                match section.kind() {
                    "initialization" | "finalization" => {
                        out.push(build_scope_cfg(
                            format!("{module_name}.<{}>", section.kind()),
                            section.start_byte()..section.end_byte(),
                            section,
                            ScopeBody::Section,
                            source,
                            unit_key,
                            exception_types,
                        ));
                    }
                    _ => continue,
                }
            }
        }
        _ => unreachable!("module collector only accepts module nodes"),
    }
}

fn direct_named_child<'tree, const N: usize>(
    node: Node<'tree>,
    kinds: [&str; N],
) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let child = node
        .named_children(&mut cursor)
        .find(|child| kinds.contains(&child.kind()));
    child
}

fn extract_module_name(module: Node, source: &[u8]) -> Option<String> {
    let module_name = direct_child(module, "moduleName")?;
    let name = node_text(module_name, source);
    (!name.is_empty()).then_some(name)
}

enum ScopeBody {
    Block,
    Section,
}

type PreprocessorBranchKey = (usize, usize, usize);
type PreprocessorGroupId = usize;

#[derive(Debug, Clone, Copy)]
struct PreprocessorBranchInfo {
    group: PreprocessorGroupId,
    index: usize,
}

fn build_scope_cfg(
    scope_name: String,
    byte_range: Range<usize>,
    scope_node: Node,
    scope_body: ScopeBody,
    source: &[u8],
    unit_key: &UnitKey,
    exception_types: &ExceptionTypeIndex,
) -> Cfg {
    let mut builder = DefaultCfgBuilder::new(scope_name, byte_range);

    let entry = builder.new_block(BasicBlockKind::Entry);
    let exit = builder.new_block(BasicBlockKind::Exit);
    builder.set_entry(entry);
    builder.set_exit(exit);

    let body = builder.new_block(BasicBlockKind::Normal);
    builder.add_edge(entry, body, EdgeKind::Normal);

    let mut label_prepass = LabelScopePrepass::new(source);
    label_prepass.collect(scope_node, &[], 0);
    let LabelScopePrepass {
        scope_ids,
        label_scopes,
        preprocessor_branch_bindings,
        preprocessor_branch_info,
        label_binding_parents,
        next_label_binding,
        ..
    } = label_prepass;
    let preprocessor_label_bindings = preprocessor_branch_bindings.values().copied().collect();
    let preprocessor_branch_keys = preprocessor_branch_bindings
        .iter()
        .map(|(key, binding)| (*binding, *key))
        .collect();
    let preprocessor_binding_sources = preprocessor_branch_bindings
        .values()
        .map(|binding| (*binding, *binding))
        .collect();
    let preprocessor_runtime_bindings = preprocessor_branch_bindings
        .iter()
        .map(|(key, binding)| ((0, *key), *binding))
        .collect();

    let mut ctx = BuildContext {
        builder: &mut builder,
        exit,
        source,
        unit_key,
        exception_types,
        loop_stack: Vec::new(),
        cleanup_scopes: Vec::new(),
        scope_ids,
        current_label_binding: 0,
        next_label_binding,
        label_binding_parents,
        finalizer_cache: HashMap::new(),
        finalizer_continuation_ids: HashMap::new(),
        next_finalizer_continuation_id: 0,
        active_finalizer_continuation: None,
        active_finalizer_suspended_transfer: None,
        next_exception_dispatch_id: 0,
        exception_dispatch_stack: Vec::new(),
        implicit_exception_depth: 0,
        handled_exception_stack: Vec::new(),
        block_has_executable_stmt: HashSet::new(),
        label_scopes,
        preprocessor_branch_bindings,
        preprocessor_branch_info,
        preprocessor_branch_keys,
        preprocessor_binding_sources,
        preprocessor_runtime_bindings,
        preprocessor_label_bindings,
        label_targets: HashMap::new(),
    };

    let final_flow = match scope_body {
        ScopeBody::Block => walk_block_stmts(&mut ctx, scope_node, body),
        ScopeBody::Section => walk_section_stmts(&mut ctx, scope_node, body),
    };
    finish_flow(&mut ctx, final_flow);

    builder.finish()
}

struct LabelScopePrepass<'a> {
    source: &'a [u8],
    next_scope_id: ScopeId,
    scope_ids: HashMap<(usize, usize), ScopeId>,
    label_scopes: HashMap<(LabelBindingId, String), Vec<ScopeId>>,
    preprocessor_branch_bindings: HashMap<PreprocessorBranchKey, LabelBindingId>,
    preprocessor_branch_info: HashMap<LabelBindingId, PreprocessorBranchInfo>,
    next_preprocessor_group: PreprocessorGroupId,
    label_binding_parents: Vec<Option<LabelBindingId>>,
    next_label_binding: LabelBindingId,
}

impl<'a> LabelScopePrepass<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            next_scope_id: 0,
            scope_ids: HashMap::new(),
            label_scopes: HashMap::new(),
            preprocessor_branch_bindings: HashMap::new(),
            preprocessor_branch_info: HashMap::new(),
            next_preprocessor_group: 0,
            label_binding_parents: vec![None],
            next_label_binding: 1,
        }
    }

    fn collect(
        &mut self,
        node: Node,
        active_scopes: &[ScopeId],
        current_label_binding: LabelBindingId,
    ) {
        if node.kind() == "defProc" {
            return;
        }

        if node.kind() == "ppBlock" {
            let (branches, has_else) = preprocessor_branches(node, self.source);
            if branches.iter().all(Vec::is_empty) && !has_else {
                return;
            }

            let group = self.next_preprocessor_group;
            self.next_preprocessor_group += 1;
            for (index, branch) in branches.into_iter().enumerate() {
                let branch_binding = self.next_label_binding;
                self.next_label_binding += 1;
                self.label_binding_parents.push(Some(current_label_binding));
                self.preprocessor_branch_bindings
                    .insert((node.start_byte(), node.end_byte(), index), branch_binding);
                self.preprocessor_branch_info
                    .insert(branch_binding, PreprocessorBranchInfo { group, index });

                for child in branch {
                    self.collect(child, active_scopes, branch_binding);
                }
            }
            return;
        }

        if node.kind() == "label" {
            if let Some(identifier) = label_name_node(node) {
                self.label_scopes.insert(
                    (
                        current_label_binding,
                        normalize_label_name(node_text(identifier, self.source)),
                    ),
                    active_scopes.to_vec(),
                );
            }
            return;
        }

        if node.kind() == "try" && try_has_finally(node) {
            let scope_id = self.next_scope_id;
            self.next_scope_id += 1;
            self.scope_ids
                .insert((node.start_byte(), node.end_byte()), scope_id);
            let mut try_scopes = active_scopes.to_vec();
            try_scopes.push(scope_id);

            for child in field_children(node, "try") {
                self.collect(child, &try_scopes, current_label_binding);
            }
            for field in ["except", "finally"] {
                for child in field_children(node, field) {
                    self.collect(child, active_scopes, current_label_binding);
                }
            }
            return;
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect(child, active_scopes, current_label_binding);
        }
    }
}

fn try_has_finally(node: Node) -> bool {
    field_children(node, "finally")
        .iter()
        .any(|child| child.kind() == "kFinally")
}

fn normalize_label_name(name: String) -> String {
    let name = name.to_ascii_lowercase();
    if name.bytes().all(|byte| byte.is_ascii_digit()) {
        let canonical = name.trim_start_matches('0');
        if canonical.is_empty() {
            "0".to_string()
        } else {
            canonical.to_string()
        }
    } else {
        name
    }
}

fn proc_header<'tree>(def_proc: Node<'tree>) -> Option<Node<'tree>> {
    if let Some(header) = def_proc.child_by_field_name("header") {
        return Some(header);
    }

    let mut cursor = def_proc.walk();
    let header = def_proc
        .children(&mut cursor)
        .find(|child| child.kind() == "declProc");
    header
}

/// Extract the complete lexical procedure/function name from a `defProc` node.
///
/// Unlike [`extract_proc_name`], this preserves every nested `genericDot`
/// component and generic argument for qualification of descendants.
fn extract_full_proc_name(def_proc: Node, source: &[u8]) -> Option<String> {
    let header = proc_header(def_proc)?;
    let name_node = header.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    (!name.is_empty()).then_some(name)
}

/// Extract the procedure/function name from a `defProc` node.
///
/// For standalone procedures: `declProc > identifier`
/// For methods: `declProc > genericDot > identifier, identifier`
fn extract_proc_name(def_proc: Node, source: &[u8]) -> Option<String> {
    let decl_proc = proc_header(def_proc)?;

    // Try genericDot first (for method implementations like TClass.Method)
    let mut cursor = decl_proc.walk();
    if let Some(generic_dot) = decl_proc
        .children(&mut cursor)
        .find(|c| c.kind() == "genericDot")
    {
        let mut generic_cursor = generic_dot.walk();
        let idents: Vec<Node> = generic_dot
            .children(&mut generic_cursor)
            .filter(|c| c.kind() == "identifier")
            .collect();

        if idents.len() >= 2 {
            return Some(format!(
                "{}.{}",
                node_text(idents[0], source),
                node_text(idents[1], source)
            ));
        }
        if !idents.is_empty() {
            return Some(node_text(idents[0], source));
        }
    }

    // Try the `name` field
    if let Some(name_node) = decl_proc.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    // Fallback: first direct identifier child
    let mut cursor2 = decl_proc.walk();
    for child in decl_proc.children(&mut cursor2) {
        if child.kind() == "identifier" {
            return Some(node_text(child, source));
        }
    }

    None
}

/// Mutable context passed through the CFG building walk.
struct BuildContext<'a> {
    builder: &'a mut DefaultCfgBuilder,
    exit: BlockId,
    source: &'a [u8],
    unit_key: &'a UnitKey,
    exception_types: &'a ExceptionTypeIndex,
    loop_stack: Vec<LoopFrame>,
    cleanup_scopes: Vec<ScopeId>,
    /// Stable IDs for syntactic try/finally scopes, shared by all runtime
    /// clones of the same finalizer body.
    scope_ids: HashMap<(usize, usize), ScopeId>,
    /// The label namespace for the walk currently being constructed.
    current_label_binding: LabelBindingId,
    /// Fresh namespace IDs for cloned finalizer walks.
    next_label_binding: LabelBindingId,
    /// Enclosing label namespace for each walk, starting at the routine body.
    label_binding_parents: Vec<Option<LabelBindingId>>,
    /// Finalizer bodies keyed by their stable syntactic scope and effective
    /// pending continuation. A cached body is safe to reuse only when its
    /// normal completion preserves the same continuation.
    finalizer_cache: HashMap<FinalizerCacheKey, CachedFinalizerBody>,
    /// Interned semantic identities for finalizer return continuations. The
    /// identity is stable across cache hits and intentionally excludes the
    /// concrete handler-dispatch stack, which is an execution context rather
    /// than part of a normal return suffix.
    finalizer_continuation_ids: HashMap<FinalizerContinuationKey, FinalizerContinuationId>,
    next_finalizer_continuation_id: usize,
    /// The semantic identity of the finalizer body currently being walked.
    /// Nested normal completions must retain this caller continuation instead
    /// of being merged solely because they have no pending transfer.
    active_finalizer_continuation: Option<FinalizerContinuationId>,
    /// The transfer suspended beyond the current finalizer's local suffix.
    /// This is used only to identify equivalent return continuations; it must
    /// never become a nested finalizer input, or its local tail would be
    /// skipped.
    active_finalizer_suspended_transfer: Option<ContinuationKey>,
    /// Fresh identities for concrete try/except handler dispatch contexts.
    next_exception_dispatch_id: usize,
    /// The active handler contexts that can receive unknown exceptions.
    exception_dispatch_stack: Vec<ExceptionDispatchId>,
    /// Nonzero while walking a try body, handler, or finally body.  This is
    /// intentionally independent from `cleanup_scopes`: handlers/finalizers
    /// may throw outward even though the scope whose handler they belong to
    /// is no longer an active catch target.
    implicit_exception_depth: usize,
    /// Facts for the exception currently handled by each nested handler body.
    handled_exception_stack: Vec<ExceptionTypeFact>,
    /// Protected executable statements are split into separate blocks so
    /// their exceptional edge cannot also cover an earlier unprotected
    /// statement. Labels do not count as executable statements: consecutive
    /// labels must all target the statement that follows them.
    block_has_executable_stmt: HashSet<BlockId>,
    /// Cleanup scopes containing each label, collected before CFG construction
    /// so forward gotos can be routed through finalizers precisely. Conditional
    /// preprocessor branches have separate namespaces so duplicate labels retain
    /// their own target scope sets.
    label_scopes: HashMap<(LabelBindingId, String), Vec<ScopeId>>,
    /// Stable label namespaces assigned to preprocessor branches during the
    /// prepass. Runtime walks use the same IDs when routing branch-local gotos.
    preprocessor_branch_bindings: HashMap<PreprocessorBranchKey, LabelBindingId>,
    /// Syntactic alternative-group and branch metadata for each prepass
    /// namespace. Branches in one group are mutually exclusive; independent
    /// groups remain compatible label targets.
    preprocessor_branch_info: HashMap<LabelBindingId, PreprocessorBranchInfo>,
    /// Reverse lookup from a prepass namespace to its syntax node and branch.
    preprocessor_branch_keys: HashMap<LabelBindingId, PreprocessorBranchKey>,
    /// Static preprocessor namespaces and runtime clone namespaces both point
    /// to the prepass namespace they represent.
    preprocessor_binding_sources: HashMap<LabelBindingId, LabelBindingId>,
    /// Runtime namespace for one preprocessor branch, keyed by its containing
    /// finalizer walk and syntactic branch key. The root walk uses instance 0.
    preprocessor_runtime_bindings: HashMap<(LabelBindingId, PreprocessorBranchKey), LabelBindingId>,
    /// Runtime branch namespaces, including fresh copies created while walking
    /// a cloned finalizer whose lexical parent differs from the prepass.
    preprocessor_label_bindings: HashSet<LabelBindingId>,
    /// Label targets discovered while walking the procedure body.
    label_targets: HashMap<(LabelBindingId, String), BlockId>,
}

fn new_block(ctx: &mut BuildContext<'_>, kind: BasicBlockKind) -> BlockId {
    ctx.builder.new_block(kind)
}

/// Add a final edge for every flow that has reached the procedure boundary.
fn finish_flow(ctx: &mut BuildContext<'_>, flow: Flow) {
    if let Some(normal) = flow.normal {
        ctx.builder.add_edge(normal, ctx.exit, EdgeKind::Normal);
    }

    for transfer in flow.transfers {
        route_transfer(ctx, transfer);
    }
}

fn route_transfer(ctx: &mut BuildContext<'_>, transfer: PendingTransfer) {
    match transfer.kind {
        TransferKind::Exception => {
            ctx.builder
                .add_edge(transfer.source, ctx.exit, EdgeKind::ExceptionThrow);
        }
        TransferKind::Exit => {
            ctx.builder.add_edge(
                transfer.source,
                ctx.exit,
                transfer_completion_edge(&transfer),
            );
        }
        TransferKind::Goto => {
            let target = transfer
                .target
                .or_else(|| resolve_label_target(ctx, &transfer));
            if let Some(target) = target {
                ctx.builder
                    .add_edge(transfer.source, target, transfer_completion_edge(&transfer));
            } else {
                ctx.builder
                    .add_edge(transfer.source, ctx.exit, EdgeKind::Goto);
            }
        }
        TransferKind::Break | TransferKind::Continue => {
            if let Some(target) = transfer.target {
                ctx.builder
                    .add_edge(transfer.source, target, transfer_completion_edge(&transfer));
            }
        }
    }
}

fn resolve_label_target(ctx: &BuildContext<'_>, transfer: &PendingTransfer) -> Option<BlockId> {
    let label = transfer.target_label.as_ref()?;
    let mut binding = transfer.target_label_binding?;

    loop {
        if let Some(target) = ctx.label_targets.get(&(binding, label.clone())) {
            return Some(*target);
        }

        let Some(Some(parent)) = ctx.label_binding_parents.get(binding) else {
            return None;
        };
        binding = *parent;
    }
}

fn label_target_bindings(
    ctx: &BuildContext<'_>,
    label: &str,
) -> Vec<(LabelBindingId, Vec<ScopeId>)> {
    label_target_bindings_from_namespace(ctx, ctx.current_label_binding, label)
}

fn label_target_bindings_from_namespace(
    ctx: &BuildContext<'_>,
    start: LabelBindingId,
    label: &str,
) -> Vec<(LabelBindingId, Vec<ScopeId>)> {
    let mut binding = Some(start);
    while let Some(candidate) = binding {
        if let Some(scopes) = ctx.label_scopes.get(&(candidate, label.to_string())) {
            return vec![(candidate, scopes.clone())];
        }
        binding = parent_label_binding(ctx, candidate);
    }

    let source_path = preprocessor_path(ctx, start);
    let mut targets = Vec::new();

    for &prepass_binding in ctx.preprocessor_branch_info.keys() {
        let Some(scopes) = ctx
            .label_scopes
            .get(&(prepass_binding, label.to_string()))
            .cloned()
        else {
            continue;
        };
        let target_path = preprocessor_path(ctx, prepass_binding);
        if !compatible_preprocessor_paths(&source_path, &target_path) {
            continue;
        }
        let Some(runtime_binding) = runtime_target_binding(ctx, start, prepass_binding) else {
            continue;
        };
        targets.push((runtime_binding, scopes));
    }

    targets.sort_by_key(|(binding, _)| *binding);
    targets
}

fn parent_label_binding(ctx: &BuildContext<'_>, binding: LabelBindingId) -> Option<LabelBindingId> {
    ctx.label_binding_parents.get(binding).copied().flatten()
}

fn is_preprocessor_binding(ctx: &BuildContext<'_>, binding: LabelBindingId) -> bool {
    ctx.preprocessor_label_bindings.contains(&binding)
}

fn label_namespace_instance(ctx: &BuildContext<'_>, binding: LabelBindingId) -> LabelBindingId {
    let mut current = binding;
    while current != 0 && is_preprocessor_binding(ctx, current) {
        let Some(parent) = parent_label_binding(ctx, current) else {
            break;
        };
        current = parent;
    }
    current
}

fn preprocessor_path(
    ctx: &BuildContext<'_>,
    start: LabelBindingId,
) -> Vec<(PreprocessorGroupId, usize)> {
    let mut path = Vec::new();
    let mut binding = Some(start);
    while let Some(candidate) = binding {
        if let Some(source) = ctx.preprocessor_binding_sources.get(&candidate) {
            if let Some(info) = ctx.preprocessor_branch_info.get(source) {
                path.push((info.group, info.index));
            }
        }
        binding = parent_label_binding(ctx, candidate);
    }
    path
}

fn compatible_preprocessor_paths(
    source: &[(PreprocessorGroupId, usize)],
    target: &[(PreprocessorGroupId, usize)],
) -> bool {
    source.iter().all(|(source_group, source_index)| {
        match target
            .iter()
            .find(|(target_group, _)| target_group == source_group)
        {
            None => true,
            Some((_, target_index)) => target_index == source_index,
        }
    })
}

fn runtime_target_binding(
    ctx: &BuildContext<'_>,
    start: LabelBindingId,
    prepass_binding: LabelBindingId,
) -> Option<LabelBindingId> {
    // A target branch can belong to this namespace, an enclosing finalizer
    // namespace, or the routine root. Never search sibling or unrelated
    // runtime instances: those may contain the same syntactic branch but are
    // not valid owners for this transfer.
    let mut binding = Some(start);
    while let Some(candidate) = binding {
        if ctx
            .preprocessor_binding_sources
            .get(&candidate)
            .is_some_and(|source| *source == prepass_binding)
        {
            return Some(candidate);
        }
        binding = parent_label_binding(ctx, candidate);
    }

    let key = ctx.preprocessor_branch_keys.get(&prepass_binding)?;
    let mut instance = label_namespace_instance(ctx, start);
    loop {
        if instance == 0 {
            return Some(prepass_binding);
        }
        if let Some(runtime_binding) = ctx.preprocessor_runtime_bindings.get(&(instance, *key)) {
            return Some(*runtime_binding);
        }

        let parent = parent_label_binding(ctx, instance)?;
        let enclosing_instance = label_namespace_instance(ctx, parent);
        if enclosing_instance == instance {
            return None;
        }
        instance = enclosing_instance;
    }
}

fn runtime_preprocessor_binding(
    ctx: &mut BuildContext<'_>,
    prepass_binding: LabelBindingId,
    parent_binding: LabelBindingId,
) -> LabelBindingId {
    let key = *ctx
        .preprocessor_branch_keys
        .get(&prepass_binding)
        .expect("preprocessor branch key missing from prepass");
    let instance = label_namespace_instance(ctx, parent_binding);
    if let Some(runtime_binding) = ctx.preprocessor_runtime_bindings.get(&(instance, key)) {
        return *runtime_binding;
    }

    let runtime_binding = ctx.next_label_binding;
    ctx.next_label_binding += 1;
    ctx.label_binding_parents.push(Some(parent_binding));
    ctx.preprocessor_label_bindings.insert(runtime_binding);
    ctx.preprocessor_binding_sources
        .insert(runtime_binding, prepass_binding);
    ctx.preprocessor_runtime_bindings
        .insert((instance, key), runtime_binding);

    let branch_scopes: Vec<_> = ctx
        .label_scopes
        .iter()
        .filter_map(|((binding, label), scopes)| {
            (*binding == prepass_binding).then_some((label.clone(), scopes.clone()))
        })
        .collect();
    for (label, scopes) in branch_scopes {
        ctx.label_scopes.insert((runtime_binding, label), scopes);
    }

    runtime_binding
}

fn prepare_finalizer_preprocessor_bindings(
    ctx: &mut BuildContext<'_>,
    try_node: Node,
    current: LabelBindingId,
) {
    let Some(body) = field_children(try_node, "finally")
        .into_iter()
        .find(|child| child.kind() == "statements")
    else {
        return;
    };
    prepare_preprocessor_bindings_in_node(ctx, body, current);
}

fn prepare_preprocessor_bindings_in_node(
    ctx: &mut BuildContext<'_>,
    node: Node,
    current: LabelBindingId,
) {
    if node.kind() == "defProc" {
        return;
    }

    if node.kind() == "ppBlock" {
        let (branches, has_else) = preprocessor_branches(node, ctx.source);
        if branches.iter().all(Vec::is_empty) && !has_else {
            return;
        }

        for (index, branch) in branches.into_iter().enumerate() {
            let prepass_binding = ctx
                .preprocessor_branch_bindings
                .get(&(node.start_byte(), node.end_byte(), index))
                .copied()
                .expect("preprocessor branch label namespace missing from prepass");
            let branch_binding = runtime_preprocessor_binding(ctx, prepass_binding, current);
            for child in branch {
                prepare_preprocessor_bindings_in_node(ctx, child, branch_binding);
            }
        }
        return;
    }

    if node.kind() == "try" && try_has_finally(node) {
        for child in field_children(node, "try") {
            prepare_preprocessor_bindings_in_node(ctx, child, current);
        }
        for child in field_children(node, "except") {
            prepare_preprocessor_bindings_in_node(ctx, child, current);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        prepare_preprocessor_bindings_in_node(ctx, child, current);
    }
}

fn copy_visible_label_scopes(
    ctx: &mut BuildContext<'_>,
    start: LabelBindingId,
    target: LabelBindingId,
) {
    let mut visible = HashMap::new();
    let mut binding = Some(start);
    while let Some(candidate) = binding {
        let scopes: Vec<_> = ctx
            .label_scopes
            .iter()
            .filter_map(|((scope_binding, label), scopes)| {
                (*scope_binding == candidate).then_some((label.clone(), scopes.clone()))
            })
            .collect();
        for (label, scopes) in scopes {
            visible.entry(label).or_insert(scopes);
        }
        binding = parent_label_binding(ctx, candidate);
    }

    for (label, scopes) in visible {
        ctx.label_scopes.insert((target, label), scopes);
    }
}

fn transfer_completion_edge(transfer: &PendingTransfer) -> EdgeKind {
    match transfer.kind {
        TransferKind::Goto if !transfer.from_finally => EdgeKind::Goto,
        _ if transfer.from_finally => EdgeKind::FinallyExit,
        _ => EdgeKind::Normal,
    }
}

/// Walk the children of a `block`, preserving all abrupt paths from branches.
fn walk_block_stmts(ctx: &mut BuildContext<'_>, block: Node, current: BlockId) -> Flow {
    let mut cursor = block.walk();
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for child in block.children(&mut cursor) {
        if matches!(
            child.kind(),
            "kBegin" | "kEnd" | ";" | "declVars" | "declConsts" | "declTypes"
        ) {
            continue;
        }

        let child_flow = process_sequence_child(ctx, child, current);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

/// Walk a `statements` node, processing each child statement.
fn walk_statements_node(
    ctx: &mut BuildContext<'_>,
    statements_node: Node,
    current: BlockId,
) -> Flow {
    let mut cursor = statements_node.walk();
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for child in statements_node.children(&mut cursor) {
        if child.kind() == ";" {
            continue;
        }

        let child_flow = process_sequence_child(ctx, child, current);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

/// Walk the statements directly contained by a unit initialization or
/// finalization section. Those sections use an implicit `begin`, so their
/// statement nodes are siblings of the section keyword rather than children
/// of a `block` node. A legacy unit's `begin ... end.` is an initialization
/// section headed by `begin`; the closing `end.` belongs to the unit.
fn walk_section_stmts(ctx: &mut BuildContext<'_>, section: Node, current: BlockId) -> Flow {
    let header_kinds: &[&str] = match section.kind() {
        "initialization" => &["kInitialization", "kBegin"],
        "finalization" => &["kFinalization"],
        _ => &[],
    };
    let mut cursor = section.walk();
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for child in section.children(&mut cursor) {
        if child.is_extra() || child.kind() == ";" || header_kinds.contains(&child.kind()) {
            continue;
        }

        let child_flow = process_sequence_child(ctx, child, current);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

/// Process all children stored under a statement field such as `then`,
/// `else`, `body`, or an exception handler's body.
fn walk_field_children(
    ctx: &mut BuildContext<'_>,
    parent: Node,
    field_name: &str,
    current: BlockId,
    skip_k_else: bool,
) -> Flow {
    let children = field_children(parent, field_name);
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for child in children {
        if child.kind() == ";" || (skip_k_else && child.kind() == "kElse") {
            continue;
        }

        let child_flow = process_sequence_child(ctx, child, current);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

/// Handle an `ifElse` node (if/then/else).
fn handle_if_else(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let condition_block = prepare_condition_block(ctx, node, current);
    let mut transfers = implicit_exception_transfers(ctx, condition_block);

    let then_block = new_block(ctx, BasicBlockKind::Normal);
    let else_block = new_block(ctx, BasicBlockKind::Normal);
    ctx.builder
        .add_edge(condition_block, then_block, EdgeKind::ConditionalTrue);
    ctx.builder
        .add_edge(condition_block, else_block, EdgeKind::ConditionalFalse);

    let then_flow = walk_field_children(ctx, node, "then", then_block, true);
    let else_flow = walk_field_children(ctx, node, "else", else_block, true);
    let normal = join_branch_flows(ctx, then_flow.normal, else_flow.normal);

    transfers.extend(then_flow.transfers);
    transfers.extend(else_flow.transfers);

    Flow { normal, transfers }
}

/// Handle an `if` node (if/then without else).
fn handle_if_only(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let condition_block = prepare_condition_block(ctx, node, current);
    let mut transfers = implicit_exception_transfers(ctx, condition_block);

    let then_block = new_block(ctx, BasicBlockKind::Normal);
    let join = new_block(ctx, BasicBlockKind::Normal);
    ctx.builder
        .add_edge(condition_block, then_block, EdgeKind::ConditionalTrue);
    ctx.builder
        .add_edge(condition_block, join, EdgeKind::ConditionalFalse);

    let then_flow = walk_field_children(ctx, node, "then", then_block, true);
    if let Some(then_end) = then_flow.normal {
        ctx.builder.add_edge(then_end, join, EdgeKind::Normal);
    }
    transfers.extend(then_flow.transfers);

    Flow {
        normal: Some(join),
        transfers,
    }
}

fn join_branch_flows(
    ctx: &mut BuildContext<'_>,
    then_end: Option<BlockId>,
    else_end: Option<BlockId>,
) -> Option<BlockId> {
    match (then_end, else_end) {
        (Some(then_end), Some(else_end)) => {
            let join = new_block(ctx, BasicBlockKind::Normal);
            ctx.builder.add_edge(then_end, join, EdgeKind::Normal);
            ctx.builder.add_edge(else_end, join, EdgeKind::Normal);
            Some(join)
        }
        (Some(end), None) | (None, Some(end)) => {
            let join = new_block(ctx, BasicBlockKind::Normal);
            ctx.builder.add_edge(end, join, EdgeKind::Normal);
            Some(join)
        }
        (None, None) => None,
    }
}

/// Handle a `for` or `while` loop.
fn handle_for_or_while(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let cond_block = new_block(ctx, BasicBlockKind::Normal);
    let body_block = new_block(ctx, BasicBlockKind::Normal);
    let after_block = new_block(ctx, BasicBlockKind::Normal);

    ctx.builder.add_edge(current, cond_block, EdgeKind::Normal);
    add_loop_header_stmt(ctx, cond_block, node);
    let mut transfers = implicit_exception_transfers(ctx, cond_block);

    ctx.builder
        .add_edge(cond_block, body_block, EdgeKind::ConditionalTrue);
    ctx.builder
        .add_edge(cond_block, after_block, EdgeKind::LoopExit);

    let target_scopes = ctx.cleanup_scopes.clone();
    ctx.loop_stack.push(LoopFrame {
        continue_target: cond_block,
        break_target: after_block,
        continue_scopes: target_scopes.clone(),
        break_scopes: target_scopes,
    });
    let body_flow = walk_field_children(ctx, node, "body", body_block, false);
    ctx.loop_stack.pop();

    if let Some(body_end) = body_flow.normal {
        ctx.builder
            .add_edge(body_end, cond_block, EdgeKind::LoopBack);
    }

    for transfer in body_flow.transfers {
        if transfer.kind == TransferKind::Break && transfer.target == Some(after_block) {
            ctx.builder.add_edge(
                transfer.source,
                after_block,
                transfer_completion_edge(&transfer),
            );
        } else if transfer.kind == TransferKind::Continue && transfer.target == Some(cond_block) {
            ctx.builder.add_edge(
                transfer.source,
                cond_block,
                transfer_completion_edge(&transfer),
            );
        } else {
            transfers.push(transfer);
        }
    }

    Flow {
        normal: Some(after_block),
        transfers,
    }
}

/// Handle a `repeat..until` loop.
fn handle_repeat(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let body_block = new_block(ctx, BasicBlockKind::Normal);
    let cond_block = new_block(ctx, BasicBlockKind::Normal);
    let after_block = new_block(ctx, BasicBlockKind::Normal);

    ctx.builder.add_edge(current, body_block, EdgeKind::Normal);

    let target_scopes = ctx.cleanup_scopes.clone();
    ctx.loop_stack.push(LoopFrame {
        continue_target: cond_block,
        break_target: after_block,
        continue_scopes: target_scopes.clone(),
        break_scopes: target_scopes,
    });
    let body_flow = walk_field_children(ctx, node, "body", body_block, false);
    ctx.loop_stack.pop();

    if let Some(body_end) = body_flow.normal {
        ctx.builder.add_edge(body_end, cond_block, EdgeKind::Normal);
    }

    let mut transfers = implicit_exception_transfers(ctx, cond_block);
    for transfer in body_flow.transfers {
        if transfer.kind == TransferKind::Break && transfer.target == Some(after_block) {
            ctx.builder.add_edge(
                transfer.source,
                after_block,
                transfer_completion_edge(&transfer),
            );
        } else if transfer.kind == TransferKind::Continue && transfer.target == Some(cond_block) {
            ctx.builder.add_edge(
                transfer.source,
                cond_block,
                transfer_completion_edge(&transfer),
            );
        } else {
            transfers.push(transfer);
        }
    }

    add_repeat_header_stmts(ctx, cond_block, node);
    ctx.builder
        .add_edge(cond_block, body_block, EdgeKind::LoopBack);
    ctx.builder
        .add_edge(cond_block, after_block, EdgeKind::LoopExit);

    Flow {
        normal: Some(after_block),
        transfers,
    }
}

/// Handle a `case..of` statement.
fn handle_case(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let selector_block = prepare_statement_block(ctx, current);
    if let Some(selector) = case_selector(node) {
        add_stmt_ref_span(
            ctx,
            selector_block,
            node.kind(),
            node.start_byte()..selector.end_byte(),
        );
    }
    let mut transfers = implicit_exception_transfers(ctx, selector_block);
    let mut normal_ends = Vec::new();

    let mut cursor = node.walk();
    for arm in node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "caseCase")
    {
        let arm_block = new_block(ctx, BasicBlockKind::Normal);
        ctx.builder
            .add_edge(selector_block, arm_block, EdgeKind::CaseArm);
        let arm_flow = walk_field_children(ctx, arm, "body", arm_block, false);
        if let Some(arm_end) = arm_flow.normal {
            normal_ends.push(arm_end);
        }
        transfers.extend(arm_flow.transfers);
    }

    let default_children = case_default_children(node);
    if let Some(default_children) = &default_children {
        let default_block = new_block(ctx, BasicBlockKind::Normal);
        ctx.builder
            .add_edge(selector_block, default_block, EdgeKind::CaseArm);
        let default_flow = walk_node_children(ctx, default_children, default_block);
        if let Some(default_end) = default_flow.normal {
            normal_ends.push(default_end);
        }
        transfers.extend(default_flow.transfers);
    }

    let after_block = if default_children.is_none() || !normal_ends.is_empty() {
        Some(new_block(ctx, BasicBlockKind::Normal))
    } else {
        None
    };
    if let Some(after_block) = after_block {
        for normal_end in normal_ends {
            ctx.builder
                .add_edge(normal_end, after_block, EdgeKind::Normal);
        }
        if default_children.is_none() {
            ctx.builder
                .add_edge(selector_block, after_block, EdgeKind::CaseArm);
        }
    }

    Flow {
        normal: after_block,
        transfers,
    }
}

/// Handle a `with` statement.
fn handle_with(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let context_block = prepare_statement_block(ctx, current);
    if let Some(entity) = field_children(node, "entity").last() {
        add_stmt_ref_span(
            ctx,
            context_block,
            node.kind(),
            node.start_byte()..entity.end_byte(),
        );
    }
    let mut transfers = implicit_exception_transfers(ctx, context_block);

    let body_block = new_block(ctx, BasicBlockKind::Normal);
    ctx.builder
        .add_edge(context_block, body_block, EdgeKind::Normal);
    let body_flow = walk_field_children(ctx, node, "body", body_block, false);
    let after_block = body_flow.normal.map(|body_end| {
        let after_block = new_block(ctx, BasicBlockKind::Normal);
        ctx.builder
            .add_edge(body_end, after_block, EdgeKind::Normal);
        after_block
    });
    transfers.extend(body_flow.transfers);

    Flow {
        normal: after_block,
        transfers,
    }
}

fn case_selector<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let selector = node.named_children(&mut cursor).find(|child| {
        !child.is_extra()
            && !matches!(
                child.kind(),
                "caseCase" | "kCase" | "kOf" | "kElse" | "kOtherwise" | "kEnd"
            )
    });
    selector
}

fn case_default_children<'tree>(node: Node<'tree>) -> Option<Vec<Node<'tree>>> {
    let mut cursor = node.walk();
    let mut after_else = false;
    let mut children = Vec::new();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "kElse" | "kOtherwise") {
            after_else = true;
            continue;
        }
        if after_else && !matches!(child.kind(), "kEnd" | ":" | ";" | ",") {
            children.push(child);
        }
    }
    after_else.then_some(children)
}

fn walk_node_children(ctx: &mut BuildContext<'_>, children: &[Node<'_>], current: BlockId) -> Flow {
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for &child in children {
        if child.kind() == ";" {
            continue;
        }
        let child_flow = process_sequence_child(ctx, child, current);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

fn process_sequence_child(
    ctx: &mut BuildContext<'_>,
    child: Node,
    current: Option<BlockId>,
) -> Flow {
    if child.is_extra() {
        return Flow {
            normal: current,
            transfers: Vec::new(),
        };
    }

    let current = current.unwrap_or_else(|| new_block(ctx, BasicBlockKind::Normal));
    if child.kind() == "label" {
        return Flow::normal(register_label(ctx, child, current));
    }

    process_single_stmt(ctx, child, current)
}

fn register_label(ctx: &mut BuildContext<'_>, label: Node, current: BlockId) -> BlockId {
    let target = if ctx.block_has_executable_stmt.contains(&current) {
        let next = new_block(ctx, BasicBlockKind::Normal);
        ctx.builder.add_edge(current, next, EdgeKind::Normal);
        next
    } else {
        current
    };

    if let Some(identifier) = label_name_node(label) {
        let name = normalize_label_name(node_text(identifier, ctx.source));
        ctx.label_targets
            .insert((ctx.current_label_binding, name), target);
    }
    add_stmt_ref(ctx, target, label);
    target
}

/// Process a single statement node in any syntactic context.
fn process_single_stmt(ctx: &mut BuildContext<'_>, child: Node, current: BlockId) -> Flow {
    if child.is_extra() {
        return Flow::normal(current);
    }

    match child.kind() {
        "ppBlock" => handle_preprocessor_block(ctx, child, current),
        "labeledStatement" | "labeledStatementTr" => walk_labeled_statement(ctx, child, current),
        "block" => walk_block_stmts(ctx, child, current),
        "statements" => walk_statements_node(ctx, child, current),
        "ifElse" => handle_if_else(ctx, child, current),
        "if" => handle_if_only(ctx, child, current),
        "for" | "foreach" | "while" => handle_for_or_while(ctx, child, current),
        "case" => handle_case(ctx, child, current),
        "repeat" => handle_repeat(ctx, child, current),
        "try" => handle_try(ctx, child, current),
        "with" => handle_with(ctx, child, current),
        "raise" => {
            let statement_block = prepare_statement_block(ctx, current);
            add_stmt_ref(ctx, statement_block, child);
            let source_exception_type = raised_exception_type(child, ctx.source);
            let exception_fact = if child.child_by_field_name("exception").is_none() {
                ctx.handled_exception_stack
                    .last()
                    .copied()
                    .unwrap_or(ExceptionTypeFact::Unknown)
            } else {
                ctx.exception_types
                    .raised_fact(ctx.unit_key, child, ctx.source)
            };
            let mut transfers = Vec::new();
            if raise_may_throw_during_evaluation(child, ctx.source) {
                if ctx.implicit_exception_depth > 0 {
                    transfers.push(PendingTransfer::exception(statement_block));
                }
                let successful_raise = new_block(ctx, BasicBlockKind::Normal);
                ctx.builder
                    .add_edge(statement_block, successful_raise, EdgeKind::Normal);
                transfers.push(PendingTransfer::exception_with_fact(
                    successful_raise,
                    exception_fact,
                    source_exception_type,
                ));
            } else {
                transfers.push(PendingTransfer::exception_with_fact(
                    statement_block,
                    exception_fact,
                    source_exception_type,
                ));
            }
            Flow {
                normal: None,
                transfers,
            }
        }
        "statement" if is_exit_call(child, ctx.source) => {
            let statement_block = prepare_statement_block(ctx, current);
            add_stmt_ref(ctx, statement_block, child);
            let mut transfers = vec![PendingTransfer::exit(statement_block)];
            if exit_has_argument(child, ctx.source) {
                transfers.extend(implicit_exception_transfers(ctx, statement_block));
            }
            Flow {
                normal: None,
                transfers,
            }
        }
        "statement" if is_break_call(child, ctx.source) => {
            let statement_block = prepare_statement_block(ctx, current);
            add_stmt_ref(ctx, statement_block, child);
            let transfer = if let Some(frame) = ctx.loop_stack.last() {
                PendingTransfer::block_target(
                    statement_block,
                    TransferKind::Break,
                    frame.break_target,
                    frame.break_scopes.clone(),
                )
            } else {
                PendingTransfer::exit(statement_block)
            };
            Flow::transfer(transfer)
        }
        "statement" if is_continue_call(child, ctx.source) => {
            let statement_block = prepare_statement_block(ctx, current);
            add_stmt_ref(ctx, statement_block, child);
            let transfer = if let Some(frame) = ctx.loop_stack.last() {
                PendingTransfer::block_target(
                    statement_block,
                    TransferKind::Continue,
                    frame.continue_target,
                    frame.continue_scopes.clone(),
                )
            } else {
                PendingTransfer::exit(statement_block)
            };
            Flow::transfer(transfer)
        }
        "goto" => {
            let statement_block = prepare_statement_block(ctx, current);
            add_stmt_ref(ctx, statement_block, child);
            let label = label_name_node(child)
                .map(|identifier| normalize_label_name(node_text(identifier, ctx.source)))
                .unwrap_or_default();
            let target_bindings = label_target_bindings(ctx, &label);
            let transfers = if target_bindings.is_empty() {
                vec![PendingTransfer::goto(
                    statement_block,
                    label,
                    ctx.current_label_binding,
                    Vec::new(),
                )]
            } else {
                target_bindings
                    .into_iter()
                    .map(|(binding, target_scopes)| {
                        PendingTransfer::goto(
                            statement_block,
                            label.clone(),
                            binding,
                            target_scopes,
                        )
                    })
                    .collect()
            };
            Flow {
                normal: None,
                transfers,
            }
        }
        _ => {
            let statement_block = prepare_statement_block(ctx, current);
            add_stmt_ref(ctx, statement_block, child);
            Flow {
                normal: Some(statement_block),
                transfers: implicit_exception_transfers(ctx, statement_block),
            }
        }
    }
}

/// Walk an unknown preprocessor conditional as mutually exclusive CFG
/// alternatives.  The parser deliberately does not evaluate project defines,
/// so every branch is possible; a conditional without an `else` also retains
/// the path where none of its statements are compiled.
fn handle_preprocessor_block(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let (branches, has_else) = preprocessor_branches(node, ctx.source);

    if branches.iter().all(Vec::is_empty) && !has_else {
        return Flow::normal(current);
    }

    let mut normal_ends = Vec::new();
    let mut transfers = Vec::new();

    let parent_binding = ctx.current_label_binding;
    for (index, branch) in branches.iter().enumerate() {
        let branch_entry = new_block(ctx, BasicBlockKind::Normal);
        let edge_kind = if index == 0 {
            EdgeKind::ConditionalTrue
        } else {
            EdgeKind::ConditionalFalse
        };
        ctx.builder.add_edge(current, branch_entry, edge_kind);

        let prepass_binding = ctx
            .preprocessor_branch_bindings
            .get(&(node.start_byte(), node.end_byte(), index))
            .copied()
            .expect("preprocessor branch label namespace missing from prepass");
        let branch_binding = runtime_preprocessor_binding(ctx, prepass_binding, parent_binding);
        ctx.current_label_binding = branch_binding;
        let branch_flow = walk_node_children(ctx, branch, branch_entry);
        ctx.current_label_binding = parent_binding;
        if let Some(branch_end) = branch_flow.normal {
            normal_ends.push(branch_end);
        }
        transfers.extend(branch_flow.transfers);
    }

    // The no-branch path is connected directly to the join below with a
    // ConditionalFalse edge. It is not an executable branch and must not
    // receive a synthetic statement block.
    let normal = if normal_ends.is_empty() && has_else {
        None
    } else {
        let join = new_block(ctx, BasicBlockKind::Normal);
        for normal_end in normal_ends {
            if normal_end != join {
                ctx.builder.add_edge(normal_end, join, EdgeKind::Normal);
            }
        }
        if !has_else {
            ctx.builder
                .add_edge(current, join, EdgeKind::ConditionalFalse);
        }
        Some(join)
    };

    Flow { normal, transfers }
}

fn preprocessor_branches<'tree>(node: Node<'tree>, source: &[u8]) -> (Vec<Vec<Node<'tree>>>, bool) {
    let mut branches = vec![Vec::new()];
    let mut has_else = false;
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "ppIf" | "ppEndIf" | "ppDirective" | "ppText" => continue,
            "ppElse" => {
                has_else = has_else || is_unconditional_preprocessor_else(child, source);
                branches.push(Vec::new());
            }
            ";" | "," => continue,
            _ => branches
                .last_mut()
                .expect("preprocessor branch list always has a first branch")
                .push(child),
        }
    }

    (branches, has_else)
}

fn is_unconditional_preprocessor_else(node: Node, source: &[u8]) -> bool {
    let directive = node_text(node, source);
    let directive = directive.trim();
    let directive = directive
        .strip_prefix("{$")
        .and_then(|body| body.strip_suffix('}'))
        .or_else(|| {
            directive
                .strip_prefix("(*$")
                .and_then(|body| body.strip_suffix("*)"))
        })
        .unwrap_or(directive);
    directive
        .split_whitespace()
        .next()
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case("else"))
}

fn walk_labeled_statement(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let mut cursor = node.walk();
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for child in node.named_children(&mut cursor) {
        if child.kind() == "label" {
            if let Some(block) = current {
                current = Some(register_label(ctx, child, block));
            }
            continue;
        }

        let Some(block) = current else {
            continue;
        };
        let child_flow = process_single_stmt(ctx, child, block);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

/// Handle either `try..finally` or `try..except` based on parser fields.
fn handle_try(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let has_finally = try_has_finally(node);
    let has_except = field_children(node, "except")
        .iter()
        .any(|child| child.kind() == "kExcept");

    if has_finally {
        handle_try_finally(ctx, node, current)
    } else if has_except {
        handle_try_except(ctx, node, current)
    } else {
        // A malformed try node should not swallow the following statement.
        Flow::normal(current)
    }
}

#[derive(Debug)]
enum FinalizerInput {
    Normal(BlockId),
    Transfer(PendingTransfer),
}

/// Semantic identity of a pending continuation after a finalizer completes.
/// The source block is deliberately absent: equivalent continuations can
/// share one cleanup body without introducing cross-path edges.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ContinuationKey {
    Transfer {
        kind: TransferKind,
        target: Option<BlockId>,
        target_label: Option<String>,
        target_label_binding: Option<LabelBindingId>,
        target_scopes: Vec<ScopeId>,
        exception_fact: Option<ExceptionTypeFact>,
        /// Source-level constructor metadata keeps evaluation and successful
        /// raise continuations distinct without affecting semantic matching.
        exception_type: Option<String>,
    },
}

impl ContinuationKey {
    fn from_transfer(transfer: &PendingTransfer) -> Self {
        Self::Transfer {
            kind: transfer.kind,
            target: transfer.target,
            target_label: transfer
                .target_label
                .as_ref()
                .map(|label| label.to_ascii_lowercase()),
            target_label_binding: transfer.target_label_binding,
            target_scopes: transfer.target_scopes.clone(),
            exception_fact: transfer.exception_fact,
            exception_type: transfer
                .exception_type
                .as_ref()
                .map(|exception_type| exception_type.to_ascii_lowercase()),
        }
    }

    fn transfer_from_source(&self, source: BlockId) -> PendingTransfer {
        match self {
            Self::Transfer {
                kind,
                target,
                target_label,
                target_label_binding,
                target_scopes,
                exception_fact,
                exception_type,
            } => PendingTransfer {
                source,
                kind: *kind,
                target: *target,
                target_label: target_label.clone(),
                target_label_binding: *target_label_binding,
                target_scopes: target_scopes.clone(),
                from_finally: true,
                exception_fact: *exception_fact,
                exception_type: exception_type.clone(),
            },
        }
    }
}

#[derive(Debug)]
struct FinalizerGroup {
    /// `None` is a normal completion with a caller-local continuation. A
    /// `Some` key is a pending transfer that remains semantically identical
    /// after this finalizer completes.
    key: Option<ContinuationKey>,
    inputs: Vec<FinalizerInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FinalizerCacheKey {
    start_byte: usize,
    end_byte: usize,
    scope_id: ScopeId,
    continuation: Option<ContinuationKey>,
    /// For a local normal completion, the enclosing finalizer walk supplies
    /// the remaining suffix and any suspended transfer. An explicit transfer
    /// already carries that continuation, so splitting its body by the
    /// enclosing caller would duplicate every nested cleanup combination.
    caller_continuation: Option<FinalizerContinuationId>,
    cleanup_scopes: Vec<ScopeId>,
    loop_context: Vec<(BlockId, BlockId, Vec<ScopeId>, Vec<ScopeId>)>,
    /// Concrete handler-dispatch instances; depth alone can alias different
    /// handler blocks when a finalizer is cloned.
    exception_dispatch_context: Vec<ExceptionDispatchId>,
    implicit_exception_depth: usize,
    handled_exception_context: Vec<ExceptionTypeFact>,
    label_binding: Option<LabelBindingId>,
}

/// Semantic identity of a finalizer's normal return continuation.
///
/// The syntactic finalizer identifies the remaining local suffix. The local
/// continuation distinguishes normal completion inherited from the caller
/// from a fresh transfer that happens to suspend the same transfer, while the
/// suspended transfer and enclosing continuation preserve the complete chain.
/// Handler-dispatch identities are deliberately absent: they are retained by
/// [`FinalizerCacheKey`] for exceptional edges, but must not prevent equivalent
/// normal suffixes from sharing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FinalizerContinuationKey {
    start_byte: usize,
    end_byte: usize,
    scope_id: ScopeId,
    /// Distinguishes a caller-local normal completion from a fresh transfer
    /// that happens to suspend the same continuation beyond this finalizer.
    local_continuation: Option<ContinuationKey>,
    suspended_transfer: Option<ContinuationKey>,
    caller: Option<FinalizerContinuationId>,
    handled_exception_context: Vec<ExceptionTypeFact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FinalizerContinuationId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ExceptionDispatchId(usize);

#[derive(Debug, Clone)]
struct CachedFinalizerBody {
    entry: BlockId,
    flow: Flow,
}

fn dedup_transfers(transfers: &mut Vec<PendingTransfer>) {
    let mut seen = HashSet::new();
    transfers.retain(|transfer| seen.insert(transfer.clone()));
}

/// Handle a `try..finally` block.
///
/// Incoming continuations share a finalizer body only when their effective
/// pending transfer is equivalent. Distinct normal, return, loop-target, and
/// exception continuations retain separate cleanup paths. Bodies whose normal
/// completion preserves a pending transfer are memoized across construction
/// instances; normal completion without such a transfer remains caller-local.
fn handle_try_finally(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let scope_id = ctx
        .scope_ids
        .get(&(node.start_byte(), node.end_byte()))
        .copied()
        .expect("try/finally scope missing from prepass");

    ctx.cleanup_scopes.push(scope_id);
    ctx.implicit_exception_depth += 1;
    let mut try_flow = walk_try_body(ctx, node, current);
    ctx.implicit_exception_depth -= 1;
    ctx.cleanup_scopes.pop();
    dedup_transfers(&mut try_flow.transfers);

    let mut inputs = Vec::new();
    if let Some(normal) = try_flow.normal {
        inputs.push(FinalizerInput::Normal(normal));
    }
    inputs.extend(try_flow.transfers.into_iter().map(FinalizerInput::Transfer));

    let mut output = Flow::default();
    let mut groups = Vec::new();
    for input in inputs {
        if let FinalizerInput::Transfer(transfer) = &input {
            if !transfer.leaves_scope(scope_id) {
                output.transfers.push(transfer.clone());
                continue;
            }
        }

        let key = match &input {
            FinalizerInput::Normal(_) => None,
            FinalizerInput::Transfer(transfer) => Some(ContinuationKey::from_transfer(transfer)),
        };
        if let Some(group) = groups
            .iter_mut()
            .find(|group: &&mut FinalizerGroup| group.key == key)
        {
            group.inputs.push(input);
        } else {
            groups.push(FinalizerGroup {
                key,
                inputs: vec![input],
            });
        }
    }

    for group in groups {
        let is_normal = group.key.is_none();
        let (finally_block, finally_flow) =
            walk_or_reuse_finally_body(ctx, node, scope_id, group.key.as_ref());
        for input in &group.inputs {
            let (source, entry_edge) = match input {
                FinalizerInput::Normal(source) => (*source, EdgeKind::FinallyEntry),
                FinalizerInput::Transfer(transfer) => (
                    transfer.source,
                    if transfer.kind == TransferKind::Exception {
                        EdgeKind::ExceptionThrow
                    } else {
                        EdgeKind::FinallyEntry
                    },
                ),
            };
            ctx.builder.add_edge(source, finally_block, entry_edge);
        }

        if let Some(finally_end) = finally_flow.normal {
            if is_normal {
                let after_block = new_block(ctx, BasicBlockKind::Normal);
                ctx.builder
                    .add_edge(finally_end, after_block, EdgeKind::FinallyExit);
                output.normal = Some(after_block);
            } else if let Some(transfer) = group.inputs.iter().find_map(|input| match input {
                FinalizerInput::Transfer(transfer) => Some(transfer),
                FinalizerInput::Normal(_) => None,
            }) {
                // The finalizer completed normally, so the original transfer
                // remains pending for outer cleanup scopes.
                output.transfers.push(transfer.with_source(finally_end));
            } else if let Some(key) = group.key.as_ref() {
                output.transfers.push(key.transfer_from_source(finally_end));
            }
        }

        // Any transfer produced by the finalizer itself supersedes the
        // incoming transfer. Mark it as finalizer-sourced before routing it
        // through an enclosing cleanup scope (or the procedure boundary).
        output.transfers.extend(
            finally_flow
                .transfers
                .iter()
                .map(|transfer| transfer.with_source(transfer.source)),
        );
    }

    dedup_transfers(&mut output.transfers);
    output
}

fn walk_or_reuse_finally_body(
    ctx: &mut BuildContext<'_>,
    node: Node,
    scope_id: ScopeId,
    continuation: Option<&ContinuationKey>,
) -> (BlockId, Flow) {
    let caller_continuation = match continuation {
        // A normal body resumes its enclosing finalizer's local suffix, so
        // that suffix is part of its cache identity.
        None => ctx.active_finalizer_continuation,
        // A transfer body already carries the complete pending continuation;
        // retaining the caller here would create a product of equivalent
        // transfer bodies at every nested cleanup depth.
        Some(_) => None,
    };
    let cache_key = FinalizerCacheKey {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        scope_id,
        continuation: continuation.cloned(),
        caller_continuation,
        cleanup_scopes: ctx.cleanup_scopes.clone(),
        loop_context: ctx
            .loop_stack
            .iter()
            .map(|frame| {
                (
                    frame.continue_target,
                    frame.break_target,
                    frame.continue_scopes.clone(),
                    frame.break_scopes.clone(),
                )
            })
            .collect(),
        exception_dispatch_context: ctx.exception_dispatch_stack.clone(),
        implicit_exception_depth: ctx.implicit_exception_depth,
        handled_exception_context: ctx.handled_exception_stack.clone(),
        label_binding: finally_body_contains_goto(node).then_some(ctx.current_label_binding),
    };

    // A normal nested finalizer completion resumes the enclosing finalizer's
    // local suffix before any suspended transfer is routed. In the cache key,
    // however, that suspended transfer is part of the continuation identity;
    // carrying it here is metadata only and does not alter Flow construction.
    let suspended_transfer = continuation
        .cloned()
        .or_else(|| ctx.active_finalizer_suspended_transfer.clone());
    let continuation_key = FinalizerContinuationKey {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        scope_id,
        local_continuation: continuation.cloned(),
        suspended_transfer: suspended_transfer.clone(),
        caller: ctx.active_finalizer_continuation,
        handled_exception_context: ctx.handled_exception_stack.clone(),
    };
    let continuation_id =
        if let Some(continuation_id) = ctx.finalizer_continuation_ids.get(&continuation_key) {
            *continuation_id
        } else {
            let continuation_id = FinalizerContinuationId(ctx.next_finalizer_continuation_id);
            ctx.next_finalizer_continuation_id += 1;
            ctx.finalizer_continuation_ids
                .insert(continuation_key, continuation_id);
            continuation_id
        };

    if let Some(cached) = ctx.finalizer_cache.get(&cache_key).cloned() {
        return (cached.entry, cached.flow);
    }

    let finally_block = new_block(ctx, BasicBlockKind::FinallyHandler);
    ctx.implicit_exception_depth += 1;
    let previous_label_binding = ctx.current_label_binding;
    let label_binding = ctx.next_label_binding;
    ctx.next_label_binding += 1;
    ctx.label_binding_parents.push(Some(previous_label_binding));
    copy_visible_label_scopes(ctx, previous_label_binding, label_binding);
    ctx.current_label_binding = label_binding;
    prepare_finalizer_preprocessor_bindings(ctx, node, label_binding);
    let previous_finalizer_continuation = ctx.active_finalizer_continuation;
    let previous_suspended_transfer = ctx.active_finalizer_suspended_transfer.clone();
    ctx.active_finalizer_continuation = Some(continuation_id);
    ctx.active_finalizer_suspended_transfer = suspended_transfer;
    let finally_flow = walk_finally_body(ctx, node, finally_block);
    ctx.active_finalizer_continuation = previous_finalizer_continuation;
    ctx.active_finalizer_suspended_transfer = previous_suspended_transfer;
    ctx.current_label_binding = previous_label_binding;
    ctx.implicit_exception_depth -= 1;

    ctx.finalizer_cache.insert(
        cache_key,
        CachedFinalizerBody {
            entry: finally_block,
            flow: finally_flow.clone(),
        },
    );

    (finally_block, finally_flow)
}

fn finally_body_contains_goto(node: Node) -> bool {
    field_children(node, "finally")
        .into_iter()
        .any(node_contains_goto)
}

fn node_contains_goto(node: Node) -> bool {
    if node.kind() == "goto" {
        return true;
    }

    let mut cursor = node.walk();
    let result = node.named_children(&mut cursor).any(node_contains_goto);
    result
}

/// Handle a `try..except` block.
///
/// Typed `on` handlers use the per-file exception type index when it can prove
/// a match. Unknown or ambiguous facts remain alternatives, and a missing
/// catch-all retains an unmatched exception transfer so an enclosing handler
/// can receive it.
fn handle_try_except(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> Flow {
    let dispatch_id = ExceptionDispatchId(ctx.next_exception_dispatch_id);
    ctx.next_exception_dispatch_id += 1;
    ctx.exception_dispatch_stack.push(dispatch_id);
    ctx.implicit_exception_depth += 1;
    let try_flow = walk_try_body(ctx, node, current);
    ctx.implicit_exception_depth -= 1;
    let popped_dispatch_id = ctx.exception_dispatch_stack.pop();
    debug_assert_eq!(popped_dispatch_id, Some(dispatch_id));

    let mut output = Flow::default();
    let mut normal_ends = Vec::new();
    if let Some(try_end) = try_flow.normal {
        normal_ends.push(try_end);
    }

    let mut exception_sources = Vec::new();
    for transfer in try_flow.transfers {
        if transfer.kind == TransferKind::Exception {
            exception_sources.push(transfer);
        } else {
            output.transfers.push(transfer);
        }
    }

    let incoming_facts: Vec<_> = exception_sources
        .iter()
        .map(|transfer| {
            transfer
                .exception_fact
                .unwrap_or(ExceptionTypeFact::Unknown)
        })
        .collect();
    let handlers = build_except_handlers(ctx, node, &incoming_facts);

    for transfer in exception_sources {
        let raised_fact = transfer
            .exception_fact
            .unwrap_or(ExceptionTypeFact::Unknown);
        let mut may_escape = true;
        for handler in &handlers {
            if handler.catch_all {
                ctx.builder
                    .add_edge(transfer.source, handler.entry, EdgeKind::ExceptionThrow);
                may_escape = false;
                break;
            }

            match ctx
                .exception_types
                .match_handler(raised_fact, handler.exception_fact)
            {
                TypeMatch::Yes => {
                    ctx.builder
                        .add_edge(transfer.source, handler.entry, EdgeKind::ExceptionThrow);
                    may_escape = false;
                    break;
                }
                TypeMatch::No => continue,
                TypeMatch::Unknown => {
                    ctx.builder
                        .add_edge(transfer.source, handler.entry, EdgeKind::ExceptionThrow);
                }
            }
        }

        if may_escape {
            output.transfers.push(transfer);
        }
    }

    for handler in handlers {
        if let Some(handler_end) = handler.flow.normal {
            normal_ends.push(handler_end);
        }
        output.transfers.extend(handler.flow.transfers);
    }

    if !normal_ends.is_empty() {
        let after_block = new_block(ctx, BasicBlockKind::Normal);
        for normal_end in normal_ends {
            ctx.builder
                .add_edge(normal_end, after_block, EdgeKind::Normal);
        }
        output.normal = Some(after_block);
    }

    output
}

/// A handler body and its dispatch entry block.
#[derive(Debug)]
struct HandlerFlow {
    entry: BlockId,
    catch_all: bool,
    exception_fact: ExceptionTypeFact,
    flow: Flow,
}

fn build_except_handlers(
    ctx: &mut BuildContext<'_>,
    node: Node,
    incoming_facts: &[ExceptionTypeFact],
) -> Vec<HandlerFlow> {
    let except_children = field_children(node, "except");
    let mut handlers = Vec::new();

    for child in except_children {
        let (entry, catch_all, exception_fact) = match child.kind() {
            "exceptionHandler" => (
                new_block(ctx, BasicBlockKind::ExceptHandler),
                false,
                ctx.exception_types
                    .handler_fact(ctx.unit_key, child, ctx.source),
            ),
            "exceptionElse" | "statements" => (
                new_block(ctx, BasicBlockKind::BareExceptHandler),
                true,
                ExceptionTypeFact::Unknown,
            ),
            _ => continue,
        };

        let handled_fact = if catch_all {
            ExceptionTypeFact::Unknown
        } else {
            ctx.exception_types
                .handler_context(exception_fact, incoming_facts)
        };
        ctx.handled_exception_stack.push(handled_fact);
        ctx.implicit_exception_depth += 1;
        let flow = if child.kind() == "exceptionHandler" {
            walk_field_children(ctx, child, "body", entry, false)
        } else if child.kind() == "exceptionElse" {
            walk_exception_else_body(ctx, child, entry)
        } else {
            walk_statements_node(ctx, child, entry)
        };
        ctx.implicit_exception_depth -= 1;
        ctx.handled_exception_stack.pop();

        handlers.push(HandlerFlow {
            entry,
            catch_all,
            exception_fact,
            flow,
        });
    }

    if handlers.is_empty() {
        let entry = new_block(ctx, BasicBlockKind::BareExceptHandler);
        handlers.push(HandlerFlow {
            entry,
            catch_all: true,
            exception_fact: ExceptionTypeFact::Unknown,
            flow: Flow::normal(entry),
        });
    }

    handlers
}

fn walk_exception_else_body(
    ctx: &mut BuildContext<'_>,
    exception_else: Node,
    current: BlockId,
) -> Flow {
    let mut cursor = exception_else.walk();
    let mut current = Some(current);
    let mut transfers = Vec::new();

    for child in exception_else.children(&mut cursor) {
        if child.kind() == "kElse" || child.kind() == ";" {
            continue;
        }
        let child_flow = process_sequence_child(ctx, child, current);
        current = child_flow.normal;
        transfers.extend(child_flow.transfers);
    }

    Flow {
        normal: current,
        transfers,
    }
}

/// Walk the try body: the `statements` node stored in the `try` field.
fn walk_try_body(ctx: &mut BuildContext<'_>, try_node: Node, current: BlockId) -> Flow {
    let Some(body) = field_children(try_node, "try")
        .into_iter()
        .find(|child| child.kind() == "statements")
    else {
        return Flow::normal(current);
    };

    // The first protected statement must not share a block with statements
    // immediately preceding the try. This prevents a handler edge from
    // making unprotected code appear to throw into the inner handler.
    let protected_entry = new_block(ctx, BasicBlockKind::Normal);
    ctx.builder
        .add_edge(current, protected_entry, EdgeKind::Normal);
    walk_statements_node(ctx, body, protected_entry)
}

/// Walk the `statements` node after `kFinally`.
fn walk_finally_body(ctx: &mut BuildContext<'_>, try_node: Node, finally_block: BlockId) -> Flow {
    let Some(body) = field_children(try_node, "finally")
        .into_iter()
        .find(|child| child.kind() == "statements")
    else {
        return Flow::normal(finally_block);
    };

    walk_statements_node(ctx, body, finally_block)
}

fn field_children<'tree>(node: Node<'tree>, field_name: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field_name, &mut cursor)
        .collect()
}

fn prepare_condition_block(ctx: &mut BuildContext<'_>, node: Node, current: BlockId) -> BlockId {
    let condition_block = prepare_statement_block(ctx, current);
    if let Some(condition) = node.child_by_field_name("condition") {
        let end = condition.end_byte();
        add_stmt_ref_span(ctx, condition_block, node.kind(), node.start_byte()..end);
    }
    condition_block
}

fn add_loop_header_stmt(ctx: &mut BuildContext<'_>, block: BlockId, node: Node) {
    let end = match node.kind() {
        "while" => node.child_by_field_name("condition"),
        "for" => node.child_by_field_name("end"),
        "foreach" => node.child_by_field_name("iterable"),
        _ => None,
    };
    if let Some(end) = end {
        add_stmt_ref_span(ctx, block, node.kind(), node.start_byte()..end.end_byte());
    }
}

fn add_repeat_header_stmts(ctx: &mut BuildContext<'_>, block: BlockId, node: Node) {
    if let Some(repeat_keyword) = direct_child(node, "kRepeat") {
        add_stmt_ref_span(
            ctx,
            block,
            node.kind(),
            repeat_keyword.start_byte()..repeat_keyword.end_byte(),
        );
    }
    if let Some(condition) = node.child_by_field_name("condition") {
        add_stmt_ref(ctx, block, condition);
    }
}

fn direct_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let child = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    child
}

fn label_name_node<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    direct_child(node, "identifier").or_else(|| direct_child(node, "labelNumber"))
}

/// Use a fresh protected block after an existing statement. Unprotected
/// straight-line code remains coalesced into the historical body block.
fn prepare_statement_block(ctx: &mut BuildContext<'_>, current: BlockId) -> BlockId {
    if ctx.implicit_exception_depth == 0 || !ctx.block_has_executable_stmt.contains(&current) {
        return current;
    }

    let next = new_block(ctx, BasicBlockKind::Normal);
    ctx.builder.add_edge(current, next, EdgeKind::Normal);
    next
}

fn implicit_exception_transfers(ctx: &BuildContext<'_>, source: BlockId) -> Vec<PendingTransfer> {
    if ctx.implicit_exception_depth == 0 {
        Vec::new()
    } else {
        vec![PendingTransfer::exception(source)]
    }
}

/// Add a source-level statement reference and remember that the block is no
/// longer safe to reuse for another protected statement.
fn add_stmt_ref(ctx: &mut BuildContext<'_>, block: BlockId, node: Node) {
    add_stmt_ref_span(ctx, block, node.kind(), node.start_byte()..node.end_byte());
}

fn add_stmt_ref_span(
    ctx: &mut BuildContext<'_>,
    block: BlockId,
    node_kind: &str,
    byte_range: Range<usize>,
) {
    ctx.builder.add_stmt(
        block,
        StmtRef {
            byte_range,
            node_kind: node_kind.to_string(),
        },
    );
    if node_kind != "label" {
        ctx.block_has_executable_stmt.insert(block);
    }
}
