---
id: TASK-102
title: >-
  fmt4d: uses clause terminated inside each {$IFDEF} branch gets a dangling
  comma
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:14'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - correctness
dependencies:
  - TASK-63
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
priority: high
type: bug
ordinal: 104000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-63 round-trip oracle (fixture sweep in crates/fmt4d/tests/fmt_roundtrip_test.rs, listed in KNOWN_FAILING_FIXTURES).

Fixture: crates/fmt4d/tests/fixtures/ppFragment/bucket_c_uses_semi.pas

Minimal repro (default config):
```pascal
unit T;
interface
uses
{$IFDEF VER270}
  WinAPI.Windows, SysUtils;
{$ELSE}
  Windows, SysUtils;
{$ENDIF}
implementation
end.
```
Output:
```pascal
uses
  {$IFDEF VER270}
  WinAPI.Windows,
  SysUtils,
  {$ELSE}
  Windows,
  SysUtils,
  {$ENDIF};
```
With either branch active the clause reads `uses A, SysUtils, ;`, which does not compile. Each branch's own `;` is dropped and every unit inside a block gets a comma; one `;` is appended after `{$ENDIF}` (ppUsesBlockWithSemi parsed by parse_pp_uses_block in crates/fmt4d/src/uses.rs; emit_ifdef_block always passes is_last=false to the branch items). The existing test pp_fragment.rs bucket_c_uses_semi_parses_and_is_idempotent only checks directive count and idempotency, so it passes.

Expected: when the block is the last item and its branches end with `;` in the source (ppUsesBlockWithSemi), the last unit of each branch keeps the `;` and no `;` is added after `{$ENDIF}`.

Done when: the fixture passes the round-trip oracle and is removed from KNOWN_FAILING_FIXTURES.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-uses-fixes, branch fix/fmt-uses-fixes (from master d55c0f6). One commit per task, order TASK-102, TASK-104, TASK-108.

Root cause: extract_uses_items (crates/fmt4d/src/uses.rs) parses a ppUsesBlockWithSemi like a plain ppUsesBlock and drops the fact that each branch carries the clause's ';'. emit_ifdef_block emits every unit inside a block with ',' and puts the clause ';' after {$ENDIF}.

Steps:
1. RED: fmt_bugs_test regression formatting the minimal repro (each branch must end with 'SysUtils;', no '{$ENDIF};', output parses without diagnostics, idempotent). Existing fixture bucket_c_uses_semi.pas covers it in the round-trip oracle sweep.
2. IfDefBlock gets a 'terminated' flag set for ppUsesBlockWithSemi. Emission takes the punctuation the item must end with (',', ';' or nothing): a terminated block given ';' ends each branch with ';' (on the branch's last unit, or the last nested block's {$ENDIF}) and adds nothing after {$ENDIF}.
3. Remove bucket_c_uses_semi.pas from KNOWN_FAILING_FIXTURES; full fmt4d tests, fmt, clippy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED: `CARGO_BUILD_JOBS=2 cargo test -p fmt4d --test fmt_bugs_test -- uses_clause_terminated` -> uses_clause_terminated_in_each_branch_keeps_its_semicolons FAILED; output was `{$IFDEF VER270}` / `WinAPI.Windows,` / `SysUtils,` / `{$ELSE}` / `Windows,` / `SysUtils,` / `{$ENDIF};` (the bug as described).

Change: IfDefBlock.terminated (set by extract_uses_items for ppUsesBlockWithSemi). Emission now passes the punctuation an item ends with (ItemEnd: Comma | Semicolon) instead of is_last; emit_branch_items gives a branch's last unit/block that punctuation. A terminated block ending the clause ends each branch with `;` and writes nothing after {$ENDIF}; a branch with no unit gets the `;` alone on a line (grammar requires one per branch).

GREEN: fmt_bugs_test 144 passed, fmt_roundtrip_test 26 passed (every_fixture_round_trips with KNOWN_FAILING_FIXTURES now empty, so bucket_c_uses_semi.pas passes the oracle and idempotency), pp_fragment 14 passed; full `cargo test -p fmt4d` green; clippy -p fmt4d --all-targets -D warnings and fmt --check clean. Commit c91a21a.

Implemented on branch fix/fmt-uses-fixes (worktree .worktrees/fmt-uses-fixes), awaiting review and merge.

Merged into master as c492d6a (branch fix/fmt-uses-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a uses clause written as one {$IFDEF} block whose branches each end with the clause's `;` (ppUsesBlockWithSemi) was formatted with a `,` after every unit and one `;` after {$ENDIF}, so every configuration read `uses A, SysUtils, ;` and did not compile.

Change (commit c91a21a, branch fix/fmt-uses-fixes): crates/fmt4d/src/uses.rs. IfDefBlock gains `terminated`, set for ppUsesBlockWithSemi. Emission takes the punctuation an item ends with (ItemEnd) instead of a bool; emit_branch_items gives each branch's last unit or block that punctuation. A terminated block that ends the clause ends each branch with `;` and adds nothing after {$ENDIF}.

Tests: new fmt_bugs_test::uses_clause_terminated_in_each_branch_keeps_its_semicolons (exact output, parses without diagnostics, idempotent) failed before and passes now. bucket_c_uses_semi.pas removed from KNOWN_FAILING_FIXTURES; every_fixture_round_trips passes over all fixtures. Full cargo test -p fmt4d green, clippy -D warnings and fmt --check clean.

Known limits: blocks that are not terminated but end the clause are handled by TASK-108; a directive ending the clause by TASK-104.
<!-- SECTION:FINAL_SUMMARY:END -->
