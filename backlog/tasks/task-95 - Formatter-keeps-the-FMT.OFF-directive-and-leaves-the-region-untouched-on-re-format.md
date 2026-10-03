---
id: TASK-95
title: >-
  Formatter keeps the {$FMT.OFF} directive and leaves the region untouched on
  re-format
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 01:35'
updated_date: '2026-10-03 01:49'
labels:
  - fmt
  - correctness
milestone: m-3
dependencies: []
references:
  - >-
    crates/fmt4d/tests/fmt_bugs_test.rs
    (format_off_region_with_multiline_string_survives_formatting)
priority: high
type: bug
ordinal: 97000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-25 (FMT-1). When a declaration (for example a whole procedure) sits inside a `{$FMT.OFF}` ... `{$FMT.ON}` region, formatting drops the `{$FMT.OFF}` line from the output. The region body itself is kept verbatim. Because the opening directive is gone, the next format run no longer treats the code as format-off and rewrites it (for example `S  :=  S;` becomes `S := S;`). That defeats the purpose of the directive and makes the formatter non-idempotent on such files. Users lose a source directive silently, and formatting on save gradually reformats code they explicitly protected.

Repro (default config): `unit T;\ninterface\nimplementation\n{$FMT.OFF}\nprocedure P;\nbegin\n  S  :=  S;\nend;\n{$FMT.ON}\nend.\n`: the output has no `{$FMT.OFF}` line.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Formatting that output a second time produces identical output (idempotent)
- [x] #2 Comments that precede a format-off node (between {$FMT.OFF} and the node) are also preserved
- [x] #3 A regression test in crates/fmt4d/tests/fmt_bugs_test.rs fails on the original code and passes after the fix; the fmt4d suite and clippy stay green
- [x] #4 Formatting a unit whose procedure is wrapped in {$FMT.OFF}/{$FMT.ON} keeps the {$FMT.OFF} line, and everything from it through the protected code is byte-identical to the source (the formatter's normal spacing before the {$FMT.ON} line is acceptable)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Work in worktree .worktrees/fmt-off-directive, branch fix/fmt-off-directive.

Root cause: CommentMap/DirectiveMap attach trivia to leaf node IDs (leading on the next leaf, trailing on the previous leaf on the same row). DocBuilder::doc_for_node (and the sans_leading/sans_trailing variants) returns Doc::Raw(node_text) for a node inside a format-off region without descending, so the first leaf's leading trivia (the {$FMT.OFF} directive, comments) and the last leaf's trailing trivia (end; // tail) lie outside node_text and are never emitted. node_text also starts at the node, so a statement-level region loses its line indentation.

Fix:
1. Tests first in fmt_bugs_test.rs: declaration-level region with a comment and a trailing comment; statement-level region (directive + indentation); restore idempotency_check on the TASK-25 format-off fixture.
2. Add start_byte/end_byte to AttachedComment and AttachedDirective (real and patch-derived virtual directives; patches preserve byte offsets).
3. Replace the three early returns with one format_off_doc(node, include_leading, include_trailing): Raw of the source slice from the earliest leading trivia of the first leaf (only when that trivia would otherwise be emitted by descent, i.e. not when the caller is sans_leading on the leaf itself) to the latest trailing trivia of the last leaf (same rule for sans_trailing), with the start widened to the line start when only blanks precede it. Decoded and '\r'-stripped like node_text.
4. Run the fmt4d suite, clippy and fmt; check the CLI repros.

Added after testing: (a) the verbatim text starts its own output line when it starts a line in the source, via a new Doc::LineStart(indent) (newline unless already at line start, then the source indentation; measured like a Hardline). Otherwise a region that starts inside an argument list put {$FMT.OFF} after "Foo(" and moved the region on the next run. (b) build_aligned_section treats a format-off declaration as non-alignable and emits it through doc_for_node. On master the aligned path split such declarations into cells and dropped {$FMT.OFF}; with the widened Raw it would also have emitted the leading comment twice.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Tests (fmt_bugs_test.rs), all failing on fa12a80: fmt_off_directive_and_surrounding_comments_survive_formatting, fmt_off_statement_region_keeps_directive_and_indentation, format_off_region_with_multiline_string_survives_formatting (now checks the {$FMT.OFF} line and idempotency), fmt_off_declaration_in_aligned_section_keeps_trivia_once (also confirmed failing with only the alignment guard reverted).

CLI checks, all verbatim and idempotent: declaration-level region with a comment and a trailing comment, statement-level region with odd indentation, region inside a call argument list, region in a var section, comment above {$FMT.OFF}, whole-unit region.

Verification: cargo test -p fmt4d 437 passed / 0 failed; clippy -p fmt4d --all-targets -D warnings clean; fmt --check clean; cargo test -p pascal-lsp format 28 passed.

AC #1 nuance: everything from {$FMT.OFF} through the last protected node is byte-identical. At declaration level the formatter still inserts its usual blank line between the protected procedure and the {$FMT.ON} line (that gap lies outside the protected nodes). Statement-level regions keep it adjacent. AC #1 is left unchecked pending a decision.

Pre-existing, out of scope (reproduced on master fa12a80): with alignment enabled, any standalone directive before a declaration (e.g. {$R+}) gets a blank line inserted before it, which splits the alignment group on the next run, so aligned output is not idempotent. Also pre-existing: a format-off argument followed by a comma renders as "A, " with a trailing space.

AC #1 reworded with the user's agreement: the formatter's normal spacing before the {$FMT.ON} line is acceptable. The new criterion (#4) is verified by fmt_off_directive_and_surrounding_comments_survive_formatting. Merged into master as 00f5223 (fast-forward); master suite green (fmt4d 437 passed, clippy clean).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
## Keep comments and directives around format-off nodes

**Problem.** A node inside a {$FMT.OFF} region was emitted as Raw(node_text), but CommentMap/DirectiveMap attach trivia to leaves. The trivia leading the node's first leaf (the {$FMT.OFF} directive itself, comments) and trailing its last leaf (`end; // tail`) lay outside the node's text and was silently dropped. The next run no longer saw the region and reformatted it. Statement-level regions also lost their indentation, and aligned sections split format-off declarations into cells.

**Change (commit 00f5223).**
- `AttachedComment`/`AttachedDirective` carry their source byte `span`.
- `DocBuilder::format_off_doc` replaces the three Raw early returns. It widens the verbatim slice to the trivia that descending would have emitted, mirroring the `sans_leading`/`sans_trailing` variants so nothing is emitted twice.
- New `Doc::LineStart(indent)`: verbatim text that starts a source line starts an output line with its source indentation (a newline only if not already at line start). It is measured like a Hardline.
- `build_aligned_section` emits a format-off declaration through `doc_for_node` instead of decomposing it.

**Tests.** Four regression tests in fmt_bugs_test.rs, all failing on fa12a80: declaration-level region with comments, statement-level region, multiline-literal region (idempotency restored), aligned section. Six CLI cases are verbatim and idempotent. fmt4d suite 437/0, clippy -D warnings and fmt clean, pascal-lsp formatting tests green.

**Known/out of scope.** The formatter's normal blank line before {$FMT.ON} at declaration level stays (accepted). With alignment enabled, a standalone directive before a declaration gets a blank line inserted, making aligned output non-idempotent; it is tracked separately.
<!-- SECTION:FINAL_SUMMARY:END -->
