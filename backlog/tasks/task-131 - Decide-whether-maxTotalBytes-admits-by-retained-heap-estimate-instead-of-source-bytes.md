---
id: TASK-131
title: >-
  Decide whether maxTotalBytes admits by retained-heap estimate instead of
  source bytes
status: To Do
assignee: []
created_date: '2026-10-04 00:06'
labels:
  - lsp
  - runtime
dependencies:
  - TASK-10
priority: high
type: task
ordinal: 133000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Split out of TASK-10 (LSP-4), Approach item 3: "Admission for max_total_bytes uses the 48x estimate, not raw bytes."

Not done in TASK-10 because it changes the meaning of a documented user option: README.md documents `maxTotalBytes` as "268,435,456 source bytes per budget" (README.md ~line 2221) and `DEFAULT_MAX_TOTAL_BYTES` (workspace.rs:358) is 256 MiB of source. Charging `RETAINED_BYTES_PER_SOURCE_BYTE` (48) per source byte against the same default admits only ~5.3 MiB of source per workspace, which would make workspace-wide requests on the 24k-file repository fail early. Protocol tests also size `maxTotalBytes` in source bytes (tests/protocol.rs: `"maxTotalBytes": source.len() + 1`, `source.len() * 2 + 1`, `1024`, `64`).

Admission is `Workspace::make_room_for_with_control` (workspace.rs, compares `indexed_bytes` + size to `limits.max_total_bytes`); `indexed_sizes` / `indexed_bytes` record source (or expanded) lengths.

Options for the user:
1. Keep `maxTotalBytes` in source bytes and add a separate retained-heap admission bound (e.g. 48x estimate against `maxCacheBytes` or a new `maxIndexBytes`).
2. Redefine `maxTotalBytes` as retained bytes, raise its default (e.g. 2 GiB), update README and the protocol tests that size it in source bytes.
3. Leave admission as is and rely on the cache budget plus TASK-10 outstanding accounting.

Done when: the decision is recorded and, if 1 or 2, admission charges the estimate with a test that a source set whose estimate exceeds the bound is refused.
<!-- SECTION:DESCRIPTION:END -->
