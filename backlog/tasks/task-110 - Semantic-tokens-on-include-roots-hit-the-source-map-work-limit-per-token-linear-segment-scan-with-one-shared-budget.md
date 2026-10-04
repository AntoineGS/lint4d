---
id: TASK-110
title: >-
  Semantic tokens on include roots hit the source-map work limit: per-token
  linear segment scan with one shared budget
status: To Do
assignee: []
created_date: '2026-10-03 22:11'
labels:
  - lsp
  - runtime
  - spike-92
dependencies:
  - TASK-19
priority: high
type: bug
ordinal: 112000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-92. The nvim log shows 51 `include source-map work limit (1000000) reached` errors for textDocument/semanticTokens (full and range), all on 2026-10-01 13:40-13:57, i.e. the first build with commit 575e497 (project include-root tokens onto the physical file).

**Cause (still present at d55c0f6).** `semantic_tokens_from_input` (crates/pascal-lsp/src/workspace/queries.rs:441-457) creates one `MappingBudget` of MAX_SNAPSHOT_MAPPING_WORK (1_000_000, workspace/rename.rs:81) and `project_raw_tokens` (navigation/semantic_tokens.rs:360-383) calls `ExpandedSource::map_range_with_budget` once per raw token. That function (include_expansion.rs:138-184) scans `segments` from index 0 and charges one unit per segment visited, so cost is tokens x segments. A root with ~20k tokens and ~50+ include segments exhausts the budget; the whole response is refused with -32803.

**Repro idea.** Synthetic root with many {$I} directives (or one include repeated), a few thousand identifiers, request semanticTokens/full; expect tokens, currently expect the work-limit error. Unit test at include_expansion level: N tokens x M segments must not exceed the budget.

**Proposed fix.** Binary-search `map_range_with_budget` on virtual_range.start (segments are appended in ascending virtual order) so each lookup costs O(log M + spanned segments); charge per segment actually used. This is the forward half of TASK-19 and can land independently of its position-index work. The semantic-token path could also skip the projection map for tokens that lie in a single known physical segment.
<!-- SECTION:DESCRIPTION:END -->
