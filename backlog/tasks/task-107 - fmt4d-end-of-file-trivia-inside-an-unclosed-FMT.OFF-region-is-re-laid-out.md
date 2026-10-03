---
id: TASK-107
title: 'fmt4d: end-of-file trivia inside an unclosed {$FMT.OFF} region is re-laid-out'
status: To Do
assignee: []
created_date: '2026-10-03 03:28'
labels:
  - fmt
  - correctness
dependencies:
  - TASK-26
priority: medium
type: bug
ordinal: 109000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Recorded by the TASK-26 reviewer (not fixed in that branch).

TASK-26 (crates/fmt4d/src/doc_builder.rs, DocBuilder::build / eof_trivia_doc) emits comments and directives after the file's last leaf using the source's line layout: one item per line, same-line spacing and a single blank line kept. That layout ignores any {$FMT.OFF} region. Only when the whole root is in a format-off region is the root emitted verbatim, trivia included.

Repro: a {$FMT.OFF} region that is never closed and covers trivia after `end.`:
```pascal
unit T;
interface
implementation
{$FMT.OFF}
end.
  // tail
```
Output ends with `end.\n// tail`: the indentation (and any extra blank lines) of the trailing trivia is lost although it is inside a format-off region.

Note: whether the root counts as format-off depends on the trailing newline. is_in_format_off_region(root) compares the root's end row, and a final newline moves it to the row after the region's end line.

Suggested fix: when the end-of-file trivia lies inside a format-off region, emit the source slice from the last leaf's end (after its trailing trivia) to EOF verbatim instead of laying the items out.

Done when: a regression test in fmt_bugs_test.rs shows trailing trivia inside an unclosed {$FMT.OFF} region kept byte-for-byte (with and without a final newline) and idempotent.
<!-- SECTION:DESCRIPTION:END -->
