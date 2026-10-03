---
id: TASK-7
title: 'LSP-2: Semantic diagnostics keep proven findings when a budget trips'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:16'
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
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-2).

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
- [ ] #1 A fixture with 300 proven type mismatches yields 256 diagnostics plus incomplete coverage, not zero. A fixture with one syntax error at the bottom and one proven mismatch at the top yields the mismatch.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
