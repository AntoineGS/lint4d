---
id: TASK-17
title: 'LSP-11: One prepared artifact per source revision for diagnostics'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
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
