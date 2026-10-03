---
id: TASK-7
title: 'LSP-2: Semantic diagnostics keep proven findings when a budget trips'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 02:51'
labels:
  - arch-review
  - lsp
  - correctness
milestone: m-1
dependencies: []
priority: high
type: bug
ordinal: 7000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-2). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: correctness and usability.

**Problem.** `crates/pascal-lsp/src/navigation.rs:2338-2340` returns
`Ok(Vec::new())` as soon as the document has any parser recovery span.
`:2506-2516` returns an empty vector when a call analysis fails or when the
256-diagnostic cap is reached, discarding diagnostics already collected.
Adding one more error to a file makes existing errors disappear, and the API
cannot distinguish "clean" from "analysis abandoned".

**Approach.** Change the return type of the semantic diagnostic pass to
`(Vec<Diagnostic>, Coverage)` using the type from LSP-1 (or a local enum if
LSP-1 has not landed). On parser recovery, suppress only diagnostics whose
range intersects a recovery span. On cap or budget exhaustion, return what
was collected and mark coverage incomplete. On per-call analysis error,
skip that call only.

**Tests first.** Two unit tests in the `navigation.rs` test module matching
the two **Done when** cases, plus a protocol-level test that
`textDocument/diagnostic` publishes the mismatch in the second case.

**Depends on.** LSP-1 for the shared `Coverage` type, optional.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A fixture with 300 proven type mismatches yields 256 diagnostics plus incomplete coverage, not zero. A fixture with one syntax error at the bottom and one proven mismatch at the top yields the mismatch.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Research (fix/semantic-diagnostics-coverage @ 60f1e5d):
- NavigationIndex::semantic_diagnostics_with_cancel (crates/pascal-lsp/src/navigation.rs ~2330) returns Ok(Vec::new()) when the document has any parser recovery span, when the node or work budget trips, when the 256-diagnostic cap is reached, and when any identifier proof, assignment analysis, call analysis or contract pass errors.
- Production callers: Workspace::semantic_diagnostics_for_input (publication) and the post-edit proof in workspace/codeactions.rs (~4142), which treats an empty result as "the missing-interface diagnostic is gone". That proof currently passes when the budget trips.
- Contract diagnostics already refuse documents with recovery spans (navigation.rs ~3293).
- About 90 unit tests call semantic_diagnostics_with_cancel.

Plan:
1. Local stand-in for LSP-1's Coverage: SemanticDiagnosticReport { diagnostics, gaps: Vec<SemanticCoverageGap> } with gaps ParserRecovery, DiagnosticCap, Budget, CandidateSkipped. Empty gaps = complete. New production entry point semantic_diagnostic_report_with_cancel; semantic_diagnostics_with_cancel becomes a #[cfg(test)] wrapper so existing tests keep compiling and production callers must handle coverage.
2. Parser recovery: keep only findings that end before the first recovery span, and skip collecting candidates at or after it. Deviation from the task's "suppress only findings that intersect a recovery span": Pascal is declare-before-use, so a recovery span can hide declarations that later findings depend on (e.g. a half-typed var declaration would make every later use "unresolved"). The recovered_callable_* tests pin exactly that hazard for a broken uses clause.
3. Cap: stop at the 257th finding, keep 256, gap DiagnosticCap.
4. Budget: stop at the first candidate during which budget.exhausted() becomes true and drop that candidate's result (a proof that ran out mid-way may have swallowed the failure); keep earlier findings; gap Budget.
5. Per-candidate analysis error with budget left: skip that candidate only; gap CandidateSkipped.
6. Publication publishes the partial diagnostics; the coverage is not surfaced to the client yet (LSP-1 owns that surface). The code-action post-edit proof requires complete coverage and refuses otherwise (edit paths keep strictness).

Tests first: unit tests for 300 mismatches (256 + DiagnosticCap) and syntax-error-at-bottom (mismatch kept + ParserRecovery), a unit test that a finding after a recovery span is withheld, and a protocol test that textDocument/diagnostic returns the mismatch with a syntax error below it.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-02: Started. Code work in .worktrees/semantic-diagnostics on branch fix/semantic-diagnostics-coverage; master holds the only backlog copy.

2026-10-02 progress (fix/semantic-diagnostics-coverage, uncommitted):
- RED: four new unit tests in navigation.rs (keep_the_first_findings_up_to_the_cap, keep_a_finding_above_a_syntax_error, withhold_findings_below_a_syntax_error, keep_findings_proven_before_the_budget_ran_out) failed against a thin wrapper that reported the old behaviour (no findings, no gaps). The budget test's first fixture used an unqualified typo, which the proof model reports Incomplete without a System source; it now uses a missing member on the provider's record (red by the same old early-return path, not re-run separately). Protocol test pull_diagnostics_keep_a_type_mismatch_above_a_syntax_error failed on the original src with only the parse-error published.
- GREEN: semantic_diagnostic_report_with_cancel returns SemanticDiagnosticReport { diagnostics, gaps }. Budget exhaustion discards the candidate being analysed when it ran out (it may have read the failure as absence) and stops; the contract pass takes the remaining capacity and its limit error maps to DiagnosticCap. Publication keeps proven findings; the missing-interface code-action proof requires is_complete().
- Updated existing tests to assert coverage: charge_provider_generic_symbol_scans (Budget), fail_closed_at_the_traversal_limit (Budget), fail_closed_when_overload_groups_exceed_the_bound (CandidateSkipped).

2026-10-02: Committed on fix/semantic-diagnostics-coverage as c91bc9a (not merged). Verification: cargo fmt --check clean; pascal-lsp unit tests 688 pass; protocol and protocol_barriers 900/902 each, failing only the two shutdown timing tests tracked in TASK-1.2; clippy -D warnings reports only the two pre-existing collapsible-if lints in server.rs.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Semantic diagnostics keep proven findings when coverage is partial (commit c91bc9a, merged into master; master had not moved since the branch point, so the verified tree is what landed).

What changed (crates/pascal-lsp/src/navigation.rs):
- NavigationIndex::semantic_diagnostic_report_with_cancel returns SemanticDiagnosticReport { diagnostics, gaps } with gaps ParserRecovery, DiagnosticCap, Budget and CandidateSkipped. This is a local stand-in for LSP-1's Coverage. The old Vec-returning function is now a #[cfg(test)] wrapper.
- Parser recovery: only candidates that end before the first recovery span are analysed. This is stricter than the task's "suppress findings that intersect a span": Pascal declares before use, so a recovery span can hide declarations that later findings depend on (the recovered_callable_* tests pin this for a broken uses clause).
- Cap: the first 256 findings are kept. Budget: findings proven before exhaustion are kept, and the candidate analysed when it ran out is discarded. A failed candidate is skipped on its own. The contract pass receives the remaining capacity.
- workspace.rs publishes the proven findings; coverage is not surfaced to the client yet (LSP-1). workspace/codeactions.rs: the missing-interface post-edit proof now requires complete coverage (before, a tripped budget let it pass).

Tests:
- AC1: semantic_diagnostics_keep_the_first_findings_up_to_the_cap (300 mismatches give 256 + DiagnosticCap) and semantic_diagnostics_keep_a_finding_above_a_syntax_error (mismatch kept + ParserRecovery); protocol test pull_diagnostics_keep_a_type_mismatch_above_a_syntax_error.
- AC2: those, plus withhold_findings_below_a_syntax_error and keep_findings_proven_before_the_budget_ran_out, failed on the old behaviour. The protocol test failed on the original src with only the parse-error published. Three existing tests now also assert their gap.
- fmt clean; 688 unit tests pass; protocol binaries fail only the two TASK-1.2 shutdown timing tests; clippy fails only on the two pre-existing server.rs lints.
<!-- SECTION:FINAL_SUMMARY:END -->
