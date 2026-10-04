---
id: TASK-6
title: 'LSP-1: Read-only requests return partial results with coverage metadata'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-1
dependencies: []
modified_files:
  - crates/pascal-lsp/src/coverage.rs
  - crates/pascal-lsp/src/lib.rs
  - crates/pascal-lsp/src/navigation/rename.rs
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/src/workspace/code_lenses.rs
  - crates/pascal-lsp/src/workspace/queries.rs
  - crates/pascal-lsp/src/workspace/rename.rs
  - crates/pascal-lsp/tests/protocol.rs
priority: high
type: enhancement
ordinal: 6000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-1). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime and usability.

**Problem.** References, workspace symbols, highlights, call hierarchy and
workspace diagnostics reject the whole result when any part of the snapshot
is incomplete. `crates/pascal-lsp/src/workspace/queries.rs:1257-1359`,
`:2063-2073` and `:2212-2222` reject on any include error or incomplete
scan. `navigation/rename.rs:1818-1830`, `:1893-1940`, `:2015-2040` and
`navigation/overload.rs:900-913`, `:962-982` add more withholding paths.
The reviewer counted at least 12 independent withholding families
(project-context incompleteness, discovery limits, source-read failure,
include incompleteness, opaque directive blocks, parser recovery,
unsupported reference syntax, unknown conditional declarations, overload
ambiguity, unknown type/ancestor, freshness witness change, post-edit proof
failure). Edits need that strictness. Read-only answers do not. Semantic
tokens already show the better policy by falling back to lexical tokens
(`navigation/semantic_tokens.rs`).

**Approach.**
1. Introduce one `Coverage { complete: bool, reasons: Vec<IncompleteReason> }`
   value returned alongside read-only results (references, workspace
   symbols, call/type hierarchy, highlights, hover, workspace diagnostics).
2. In each withholding site listed above, for read-only requests, record
   the reason into `Coverage` and continue with the facts already proven.
   Keep rename and code-action edit paths unchanged.
3. Surface `Coverage` to the client: for references and symbols, return the
   partial list; log one `window/logMessage` line with the reasons. For
   `workspace/diagnostic` use the `unchanged`/partial result machinery LSP
   already provides.

**Tests first.** Add to `crates/pascal-lsp/tests/navigation.rs`: the fixture
above; assert three locations returned; assert the log message names the
offending unit. Add a second test that rename on the same symbol still
refuses, to pin that edits keep strictness.

**Depends on.** nothing. **Do not** change edit-path semantics.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 On a fixture with one unit containing an unsupported active directive and another unit with a public symbol used in three places, `textDocument/references` on that symbol returns the three locations plus a logged incomplete reason, instead of an empty or error response. `cargo test -p pascal-lsp` green.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-partial-results, branch feat/lsp-partial-results (from master d55c0f6).

Root cause: read-only workspace requests share the rename snapshot and its fail-closed checks. queries.rs ensure_reference_ready rejects on any include error or incomplete snapshot; workspace_symbols_from_input and workspace_diagnostics_from_input reject when snapshot.complete is false; navigation/rename.rs collect_occurrences_bounded returns Err for any unprovable occurrence in strict mode (opaque directive block, parser recovery, with/inherited, unknown conditional declaration, unknown ancestor, ambiguity, unresolved).

Plan:
1. New crate-level coverage module: Coverage { gaps: Vec<CoverageGap { uri: Option<Url>, reason }> } with is_complete(), bounded storage (first N gaps kept, rest counted) and a one-line summary. Partial<T> { value, coverage } for read-only results. Same vocabulary as TASK-7's SemanticDiagnosticReport (gaps / is_complete).
2. navigation/rename.rs: OccurrenceCollectionOptions gains an optional gap sink. With a sink, each strict-resolution withholding site records the gap and skips the occurrence (or the document, for opaque blocks) instead of failing. New partial_binding_locations entry point; rename and the rename post-edit proof keep the strict path unchanged.
3. queries.rs references: replace ensure_reference_ready with a read-only check: the requested document must still be retained/readable and not itself be blocked (own unsupported directive), otherwise fail as before. Include errors and incomplete discovery become gaps; occurrences in untrusted units (unsupported-directive owners; units with include directives when other include errors exist) are withheld and named.
4. Workspace symbols and workspace diagnostics: incomplete snapshot returns the proven subset plus coverage.
5. Server: References / WorkspaceSymbols / WorkspaceDiagnostics results carry Coverage; the response stays spec-typed (no extra fields); an incomplete coverage sends one window/logMessage (Warning) naming the gaps. Code lenses keep strictness (an incomplete count is refused).
6. Rename and code actions unchanged.

Tests first: protocol test with a workspace where one unit has an active unsupported directive and three other uses of a public symbol: today references error; after: three locations + logMessage naming the unit. Second protocol test: rename on the same symbol still refuses. Update the unit test that pinned the old fail-closed references behaviour.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-83 (Decide workspace-wide request policy on very large or partially evaluable repos).

Ruling: coverage surfacing — responses keep their plain LSP result types (Location[], SymbolInformation[], WorkspaceDiagnosticReport); none of them has an isIncomplete flag and no server-specific field is added, so nvim and VS Code see nothing new in the payload. An incomplete result sends one window/logMessage (MessageType.Warning) before the response: "pascal-lsp: <method> returned an incomplete result; items that could not be proven were left out:" plus up to 8 gaps ("<uri>: <reason>") and "and N more". Coverage keeps at most 32 distinct gaps. Partial-result streaming ($/progress chunks) logs the same line once per committed result. The log is advisory: backpressure or oversize never fails the answer.

Ruling: what a read-only reference answer withholds — the requested document must still be proven (own unsupported directive, own broken include or incomplete own expansion still reject). Elsewhere: unsupported-directive owners; units indexed under an ambiguous/incomplete project context (new RenameSnapshot.incomplete_context_sources); when the include audit has other errors (owner not recorded, early stop leaves later owners unaudited) every unit that may contain an include directive; incomplete expansions; and files included by any withheld owner. Per-occurrence strict failures in collect_occurrences_bounded (opaque block, parser recovery, with/inherited, unknown conditional, unknown ancestor, ambiguity, unresolved) become gaps — except when the failing occurrence is the requested one, which still fails, because it identifies the binding.

Ruling: kept fail-closed — the 10000-entry physical reference cap (response-size guard; truncating would be a valid but arbitrary prefix), rename/prepareRename/code actions/rename post-edit proof (strict path unchanged, they never pass a gap sink), code lenses (a count has no room to say it is partial: incomplete coverage is reported as the lens error), call/type hierarchy and highlights (follow-up TASK-117).

Ruling: workspace diagnostics keep the early stop at the first incomplete project context (it bounds runtime); the answer is now the proven subset (empty when the stop fires) plus the log, instead of an error. Diagnosing the evaluable projects is follow-up TASK-118. Units in incomplete_context_sources are not diagnosed.

Accepted risk: when discovery is incomplete, a consumer whose used unit was not indexed may bind a name to a same-named declaration from another unit (shadowing by the missing unit). This is the same exposure complete snapshots already accept for units outside the workspace (VCL, SDK); the answer is flagged incomplete.

Tests that pinned the old fail-closed behaviour and now assert the partial answer + log: workspace.rs references_withhold_an_imported_unit_with_an_unsupported_directive (was ..._still_fail_closed_...; a new test keeps the document's own directive fail-closed), workspace_diagnostics_stop_discovery_at_the_first_incomplete_context; workspace/rename.rs workspace_queries_withhold_an_incomplete_non_priority_consumer_context, workspace_queries_withhold_a_missing_optset_non_priority_consumer_context (rename still refuses in both); protocol.rs references_withhold_a_mapped_consumer_with_a_missing_nested_include, references_mark_an_incomplete_workspace_without_inventing_locations, references_mark_exhausted_mapped_source_budgets_incomplete, references_withhold_a_variable_rhs_in_a_cast_receiver, local_references_report_unresolved_imports_when_a_same_source_shadow_is_present, workspace_symbol_queries_log_the_actual_file_count_bound / _total_byte_bound, workspace_symbols_keep_proven_symbols_when_the_override_context_is_incomplete. Tests were placed in tests/protocol.rs (dispatch) rather than tests/navigation.rs (task text), since the log needs a server.

Found: worktrees inside the main checkout inherit its .cargo/config.toml path overrides (built local ../cfg-pascal). TASK-119.

Implemented on branch feat/lsp-partial-results (worktree .worktrees/lsp-partial-results), awaiting review and merge.

Ruling (controller, from task review): Approach step 3 suggested workspace/diagnostic use 'the unchanged/partial result machinery LSP already provides'. No LSP diagnostic report has a completeness flag (unchanged reports only mean 'same as previousResultId'), so the branch reports incompleteness with the same window/logMessage used for references and workspace symbols. Review minors to pick up when convenient: tighten two rename.rs tests that use `.all(..)` (pass on empty) and the workspace-symbols test that uses `any`; make the workspace-diagnostics protocol test pin today's contents (TASK-118 then changes it deliberately); drop 'references' from the fail-closed doc comment at queries.rs blocking_include_error; qualify coverage.rs:32-33 ('every item is proven') given accepted risk #5.

Merged into master as 4553ad3 (branch feat/lsp-partial-results, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: references, workspace symbols and workspace diagnostics failed the whole request when any part of the workspace could not be proven (include errors, unsupported directives, ambiguous project contexts, discovery limits, any unprovable occurrence of the name), so on large repos (multidev) they never answered.

Change (commit 2831402 on feat/lsp-partial-results):
- New crate::coverage: Coverage { gaps: Vec<CoverageGap { uri, reason }>, omitted } with is_complete()/note()/summary(), and Partial<T> { value, coverage }. Same "gaps / is_complete" vocabulary as TASK-7's SemanticDiagnosticReport.
- navigation/rename.rs: OccurrenceCollectionOptions takes an optional gap sink; each strict withholding site goes through withhold(), which records a gap and skips the occurrence (or the document for opaque blocks) for read-only callers, and still fails when the unprovable occurrence is the requested one. New partial_binding_locations_with_cancel_and_work_budget; rename and the rename post-edit proof keep the strict path.
- queries.rs: references use read_only_reference_coverage (requested document must be proven; unsupported-directive owners, units with incomplete project contexts, units with includes when the audit had other errors, incomplete expansions and their included files are withheld and named). Workspace symbols and workspace diagnostics answer with the proven subset when the snapshot is incomplete.
- workspace/rename.rs: RenameSnapshot.incomplete_context_sources records units indexed under an ambiguous/incomplete project context.
- server.rs: References/WorkspaceSymbols results carry Partial, WorkspaceDiagnosticsAnalysis carries coverage; log_incomplete_coverage sends one window/logMessage (Warning) for incomplete results on both the ordinary and the partial-result-token paths. Payloads are unchanged LSP types.
- code_lenses.rs: reference-count lenses still require complete coverage.

Tests: RED first — protocol references_return_proven_locations_and_log_the_unit_they_could_not_prove failed on the original code with -32803 "reference workspace scan incomplete: ... Legacy.pas contains an unsupported directive"; rename_still_refuses_when_a_consumer_has_an_unsupported_directive passed before and after (pins edit strictness); workspace_symbols_keep_proven_symbols_when_the_override_context_is_incomplete and workspace_diagnostics_answer_and_log_when_a_project_cannot_be_evaluated failed on the original with -32803 errors. Coverage unit tests in coverage.rs. 13 existing tests that pinned fail-closed answers were rewritten to the partial answer + log (listed in notes). Verification: cargo test -p pascal-lsp all green (lib 694, protocol 721, navigation 317, rename 94, project 88, neovim 9, ...); with --features test-support protocol and protocol_barriers 905/907 each, failing only the two TASK-1.2 timing tests; fmt clean; clippy -D warnings clean with and without test-support.

Known limits: hierarchies/highlights/document diagnostics not converted (TASK-117); workspace diagnostics still stop discovery at the first incomplete context, so that answer is empty (TASK-118); the 10000-location response cap still fails.
<!-- SECTION:FINAL_SUMMARY:END -->
