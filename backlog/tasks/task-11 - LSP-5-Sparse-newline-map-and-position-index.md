---
id: TASK-11
title: 'LSP-5: Sparse newline map and position index'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies: []
modified_files:
  - crates/pascal-lsp/src/text.rs
  - crates/pascal-lsp/src/workspace.rs
priority: high
type: enhancement
ordinal: 11000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-5). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime memory.

**Problem.** `workspace.rs:14505-14541` `NormalizedSource` stores one
`usize` per byte boundary even for LF-only input. For a 256 MiB expansion
that is about 2 GiB of map. `text.rs:166-173` and `:329` onward
(`PositionIndex`) store two `usize` arrays per character.

**Approach.** Represent CRLF normalization as a sorted `Vec<u32>` of removed
`\r` offsets and translate with binary search. Represent positions as line
starts plus a per-line list of non-ASCII corrections, only for lines that
contain non-ASCII.

**Tests first.** Property-style test: for random mixed CRLF/LF/UTF-16 inputs,
old and new implementations agree on every offset mapping. Keep the old
implementation in the test module until the new one passes.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 `NormalizedSource` for a 100 MiB LF-only input allocates under 1 MiB beyond the text. Existing text/position tests pass.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory).
Root cause (d55c0f6): workspace.rs NormalizedSource::raw_offsets holds one usize per normalized byte (normalize_line_endings_with_offsets); text.rs PositionIndex keeps byte_offsets + utf16_offsets per character on every line.
Steps: 1) Tests first: copy the old implementations into the test modules as oracles; deterministic mixed LF/CR/CRLF/BMP/supplementary inputs compare every offset and position; memory tests: 100 MiB LF-only NormalizedSource map < 1 MiB, 1 MiB ASCII PositionIndex heap < source size. 2) NormalizedSource stores ascending offsets of collapsed CRLF pairs; raw_offset() = offset + count of pairs before it (binary search). 3) PositionIndex lines keep start/end plus Box<[Correction]> (byte end, cumulative UTF-8 minus UTF-16 excess) only for non-ASCII scalars; conversions binary-search the corrections and reject offsets inside scalars via is_char_boundary.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED: lf_only_line_ending_map_allocates_nothing_per_byte, run once with a 4 MiB input for the RED (100 MiB would allocate ~1.6 GB with the per-byte map): '33554432 map bytes for 4194300 source bytes'. ascii_lines_store_no_per_character_offsets: '18088032 index bytes for 1048576 ASCII source bytes'. The oracle agreement tests passed on the original code (they record the old behaviour) and still pass.

GREEN: 100 MiB LF-only map is 0 bytes beyond the text (text capacity = source length); 1 MiB ASCII index < 1 MiB. pascal-lsp lib 711 passed; fmt/clippy clean.

Ruling: collapsed-pair offsets are Vec<usize>, not Vec<u32> — no overflow handling needed and still zero bytes for LF-only input, one word per CRLF line otherwise.

Finding (not changed): DiagnosticLineIndex (workspace.rs) still builds utf16_prefix with one usize per byte, but its only reader is the #[allow(dead_code)] position()/range() used by tests; production diagnostics use byte_range(). Overlaps TASK-59 (LINT-14).

Implemented on branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), awaiting review and merge.

Merged into master as 16139a2 (branch perf/lsp-workspace-memory, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: NormalizedSource kept one usize per byte (≈8x the text; 800 MiB for 100 MiB input) and PositionIndex kept two usize per character.

Change (commit 299b5cf): NormalizedSource stores the normalized offsets of CRLF pairs it collapsed and maps offsets with a binary search (raw_offset()). PositionIndex lines store start/end and, only for non-ASCII scalars, a Correction (byte end, cumulative UTF-8 minus UTF-16 excess); both conversions binary-search corrections and reject offsets inside scalars or surrogate pairs. owned_bytes_upper_bound_with_cancel follows the new layout.

Tests: oracle tests with the old implementations kept in the test modules compare every offset/position on 400 deterministic mixed LF/CR/CRLF/BMP/supplementary inputs each; memory tests (100 MiB LF-only map < 1 MiB: now 0; 1 MiB ASCII index < 1 MiB: was 18 MB). pascal-lsp lib 711 passed; fmt and clippy clean.
<!-- SECTION:FINAL_SUMMARY:END -->
