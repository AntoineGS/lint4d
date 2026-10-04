---
id: TASK-120
title: >-
  Fix-all per-candidate rename proofs are quadratic and exhaust the work budget
  past ~150 candidates
status: To Do
assignee: []
created_date: '2026-10-03 23:00'
labels:
  - lsp
  - runtime
dependencies:
  - TASK-9
priority: medium
type: enhancement
ordinal: 122000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After TASK-9 (commit 2f21b69) fix-all validates compatible candidates once, but the per-candidate phase before it is still quadratic: fix_all_plan_from_input (crates/pascal-lsp/src/workspace/codeactions.rs, the proven_candidates loop) calls snapshot.index.rename_edits_with_cancel_and_work_budget per candidate, and each call's collect_occurrences_bounded (navigation/rename.rs) walks every identifier node of the document, charging the shared fix-all budget (MAX_FIX_ALL_WORK = 1_000_000).

Measured with the test fixture of codeactions.rs fix_all_validates_compatible_candidates_once (N constants "badConstI = I;" with constant_style UPPER_CASE): at N=500 the proof phase alone exhausts the budget before any validation (about 940k work used before the first validation is reached), so no action is offered; N=300 fits the proofs but not proofs + validation; N=150 works. TASK-9 AC #1 asks for 500 candidates and is blocked by this.

Approach idea: collect occurrences for all candidates in one identifier walk (index identifiers by canonical name once per document, per request), or batch rename planning, so the proof phase is linear in document size plus candidates.
<!-- SECTION:DESCRIPTION:END -->
