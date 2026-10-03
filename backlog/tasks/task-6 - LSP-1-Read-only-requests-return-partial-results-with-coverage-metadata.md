---
id: TASK-6
title: 'LSP-1: Read-only requests return partial results with coverage metadata'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-1
dependencies: []
priority: high
type: enhancement
ordinal: 6000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-1).

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
- [ ] #1 On a fixture with one unit containing an unsupported active directive and another unit with a public symbol used in three places, `textDocument/references` on that symbol returns the three locations plus a logged incomplete reason, instead of an empty or error response. `cargo test -p pascal-lsp` green.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-83 (Decide workspace-wide request policy on very large or partially evaluable repos).
<!-- SECTION:NOTES:END -->
