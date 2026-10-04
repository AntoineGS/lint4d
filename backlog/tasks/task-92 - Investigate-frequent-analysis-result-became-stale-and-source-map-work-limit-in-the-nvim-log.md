---
id: TASK-92
title: >-
  Investigate frequent 'analysis result became stale' and source-map work limit
  in the nvim log
status: Done
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 22:11'
labels:
  - agent-todos
  - rtl-units
  - lsp
dependencies: []
priority: medium
type: spike
ordinal: 94000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

Find out why the user's nvim log shows frequent `analysis result became
stale` for semantic tokens and `include source-map work limit (1000000)
reached`.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-19 (LSP-14). Check it before starting to avoid duplicate work.

Related review task: TASK-36 (LSP-19). Check it before starting to avoid duplicate work.

Log location (user, 2026-10-03): ~/.local/state/nvim/lsp.log on the dev machine (2.1 MB, last modified 2026-09-15, so it predates recent fixes).

Spike findings (log ~/.local/state/nvim/lsp.log is the 2026-08-13..09-15 file and has neither message; the matching log is ~/.local/state/nvim/logs/lsp.log, 2026-09-14..10-03, 599 sessions).
Counts (all from nvim semantic_tokens handler, code -32803, both full and range requests): 54 "analysis result became stale" (2026-09-16:5+... bursts of 1-8 within seconds, 13 bursts, mostly 10-170 s after server start; two on 10-01 and one on 09-29/09-30); 51 "include source-map work limit (1000000) reached", all 2026-10-01 13:40-13:57 in 5 bursts (24, 4, 19, 4). Other semantic-token errors in the same log, not in scope: 159 "analysis server is busy", 98 "closed source changed while resolving", 92 "semantic token resolution exceeds the 8 MiB scan limit" (10-02), 23 "include expansion work limit" (09-28).
Source-map limit: emitted by MappingBudget::charge (include_expansion.rs:48-52). Only the semantic-token projection path (queries.rs:441, project_raw_tokens) calls it per token with one shared budget, and map_range_with_budget scans segments linearly: tokens x segments. Appeared with commit 575e497 (10-01 13:04), the first build that projects tokens of include roots; the 10-01 13:40 binary is target/release of the main checkout. Still present at d55c0f6. Tracked as TASK-110 (links TASK-19).
Stale: emitted at server.rs:9225-9232 (and 8070-8085 for partial delivery) after analysis_result_is_stale (server.rs:9145). Cause is ordinary edits (advance_document_version, workspace.rs:4319, bumps the generation; open-document records are compared by text) plus warm-up ClosureStored refreshes (server.rs:11805), reported as -32803 so nvim logs an error per attempt. Not fixed on master; TASK-5 (permanent fence, would give a different "analysis is disabled" message) and TASK-7 (diagnostics coverage) do not change this. Tracked as TASK-111; structural fix is TASK-36.
TASK-91 does not explain it: nvim sends a raw @, and the log has no %40 URIs (the sessions were stale on a stale-by-edit pattern, not on every pull).
Limits: log is at the default level, so there are no request ids, URIs or versions for the stale errors; the burst timing and code path are inference, not proof.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Both messages quantified and traced. Source-map work limit (51 hits, one afternoon, 2026-10-01): per-token linear scan of the include source map with one shared 1M budget in semantic-token projection; still present, new task TASK-110 (part of TASK-19). Stale (54 hits, 13 bursts): normal edit/refresh invalidation reported as -32803 errors; still present, new task TASK-111, structural fix TASK-36. TASK-91 (%40) does not explain either. Caveat: default log level, so request-level detail is inferred from timing and code. No code changed. Report: .superpowers/sdd/day-2026-10-03/LOG-TASK-92-report.md
<!-- SECTION:FINAL_SUMMARY:END -->
