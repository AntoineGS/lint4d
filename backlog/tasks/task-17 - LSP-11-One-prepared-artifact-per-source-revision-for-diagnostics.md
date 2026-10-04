---
id: TASK-17
title: 'LSP-11: One prepared artifact per source revision for diagnostics'
status: Deferred
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 00:27'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-12
priority: high
type: enhancement
ordinal: 17000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-11). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime cpu.

**Problem.** A diagnostic request at `workspace.rs:13249-13281`,
`:13495-13538`, `:13548-13569`, `:13618-13642`, `:13654-13662`,
`:14570-14584` expands includes, runs conditional analysis, parses the
source only to check tree depth, builds a semantic snapshot, builds a fresh
navigation index for imports, parses the original again to read the unit
name, resolves a project graph and builds a CFG snapshot. No stage shares
one artifact. Lint execution and the depth-check parse receive no
cancellation flag.

**Approach.** Define `PreparedSource { text: Arc<str>, tree: Arc<Tree>,
conditional: Arc<ConditionalAnalysis>, unit_name, import_sites }` keyed by
(uri, version, context fingerprint) in the project cache. All diagnostic
stages consume it. Thread the cancel flag into lint and the depth check.

**Tests first.** Parse-count characterization test; cancellation latency
test.

**Depends on.** LSP-6 for `Arc<str>` text.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 One diagnostic request parses the document once (assert with a parse counter in tests). Cancellation during lint returns within 100 ms.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Deferred by the LSP-W lane (branch perf/lsp-workspace-memory) after TASK-12 landed there. Reason: AC#1 ("one diagnostic request parses the document once" + "cancellation during lint returns within 100 ms") cannot be met from pascal-lsp's caching paths, which is the scope this lane was given; it needs API changes in the lint4d crate and a parse-count hook in pascal-core, i.e. a cross-crate design for who owns PreparedSource.

Parse inventory for one closed-or-open document diagnostic request at d55c0f6 + this lane (workspace.rs, grep identifiers):
1. ensure_safe_tree_depth(normalize_line_endings(source)) — pascal_core::parser::parse_file, before expansion.
2. ensure_safe_tree_depth(normalized.text) on the conditional projection of the expansion — second tree-sitter parse.
3. semantic_diagnostics_for_input -> rename::build_snapshot(LocalWithImports) — Document::parse of the document (and imports).
4. run_shared_lint_with_cancel: NavigationIndex::update_with_context_with_cancel(source) — another Document::parse, with no cached parse passed.
5. run_shared_lint_with_cancel: parser::parse_file(source) only to call lint4d::rules::helpers::extract_unit_name.
6. lint4d::cfg::to_cfg_project_snapshot -> parse_raw_inputs (crates/lint4d/src/cfg/project_snapshot.rs) parses every unit, the root included; with prepare_configured_sources it prepares again through cfg_pascal (sibling repo).
7. lint4d::engine::run_lint_with_cfg_project_mode (crates/lint4d/src/engine/mod.rs) parses lint_source again (parse_file at the top); the file-local fallbacks do the same.
Cancellation: lint4d::engine has no cancel parameter (rules loop, CFG build); to_cfg_project_snapshot and ensure_safe_tree_depth take none.

What would unblock it:
- lint4d engine entry point that takes a parsed (tree, parse diagnostics) and a cancel callback checked between rules and before CFG building (crates/lint4d/src/engine/mod.rs).
- to_cfg_project_snapshot accepting the root unit's already-parsed tree (crates/lint4d/src/cfg/project_snapshot.rs parse_raw_inputs); configured preparation sharing it needs cfg-pascal (sibling) support or acceptance that preparation is a separate artifact.
- A parse counter visible to pascal-lsp tests (pascal-core test-support feature around parser::parse_file, plus navigation's TEST_DOCUMENT_PARSE_CALLS).
- A decision on the 100 ms cancellation criterion: the day-run rules forbid new tests on tight wall-clock windows; a deterministic test (cancel set before or during lint stops before the next rule, via a hook) would replace it.
Cheap pascal-lsp-only steps that remain available (they reduce but do not meet AC#1): reuse the snapshot's ParsedDocument as the cached parse for source_index (4), take the unit name from source_index instead of parse 5, and skip parse 1 when normalize(source) equals normalized.text.
<!-- SECTION:NOTES:END -->
