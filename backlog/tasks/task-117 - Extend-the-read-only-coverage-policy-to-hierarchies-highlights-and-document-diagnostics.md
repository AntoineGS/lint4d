---
id: TASK-117
title: >-
  Extend the read-only coverage policy to hierarchies, highlights and document
  diagnostics
status: To Do
assignee: []
created_date: '2026-10-03 22:48'
labels:
  - lsp
dependencies:
  - TASK-6
priority: medium
type: enhancement
ordinal: 119000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-6 (commit 2831402 on feat/lsp-partial-results) added crate::coverage::{Coverage, Partial} and applied the partial-result policy to textDocument/references, workspace/symbol and workspace/diagnostic only (log via window/logMessage, see server.rs log_incomplete_coverage).

Still fail-closed or silent:
- callHierarchy/incomingCalls, outgoingCalls and typeHierarchy supertypes/subtypes use ensure_reference_ready (crates/pascal-lsp/src/workspace/queries.rs, call_hierarchy_edges_from_input / type_hierarchy_edges_from_input) and reject on any include error or incomplete snapshot. They could reuse read_only_reference_coverage.
- textDocument/documentHighlight uses ensure_document_ready and silently skips unprovable occurrences (strict_resolution false) with no coverage.
- TASK-7's SemanticDiagnosticReport.gaps (navigation.rs SemanticCoverageGap) are still not surfaced to the client; push/pull document diagnostics publish partial findings without saying so. Consider mapping them onto Coverage.
- navigation/overload.rs withholding paths (review cited :900-913, :962-982) were not touched.
Approach: same as TASK-6: Partial<T> result, Coverage gaps, one logMessage line; edits stay strict.
<!-- SECTION:DESCRIPTION:END -->
