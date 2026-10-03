---
id: TASK-102
title: >-
  fmt4d: uses clause terminated inside each {$IFDEF} branch gets a dangling
  comma
status: To Do
assignee: []
created_date: '2026-10-03 03:14'
labels:
  - fmt
  - correctness
dependencies:
  - TASK-63
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
