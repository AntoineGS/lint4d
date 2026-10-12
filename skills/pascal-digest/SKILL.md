---
name: pascal-digest
description: Use before reading a long Delphi/Object Pascal routine (over ~150 lines), or when asked what a Pascal routine does, whether its objects are freed, or where it starts, commits or rolls back transactions. Produces a compact, line-anchored digest with `lint4d digest`.
---

# Pascal routine digests

`lint4d digest` summarises Pascal routines: control flow with conditions,
objects created and whether they are freed on every path, transactions,
raises, exits and calls, each with its source line.

## When to use it

- Before reading a Pascal routine longer than about 150 lines.
- When you have a `file:line` (grep hit, compiler error, stack trace) inside a long routine.
- To choose which routine of a large unit to read.

## Commands

- `lint4d digest <file>`: routine index with line spans, sizes and markers
  (`tx`, `raiseN`, `exitN`, `!escape` = an object is not freed on every path,
  `?` = an undecided verdict).
- `lint4d digest <file> --line N`: the innermost routine containing line N.
- `lint4d digest <file> --routine NAME`: qualified (`TFoo.Bar`) or last
  component (`Bar`); every overload is shown.
- `--all` digests every routine; `--format json` prints the data model;
  `--max-lines N` (default 300) collapses the deepest blocks to
  `Lstart-end … (N lines, calls …)`.

## Reading a digest

- Read only the source lines the digest points to.
- `NOT freed on all paths (escapes via L… exception)` and `unknown (…)` are
  leads: verify them in the source before calling them bugs.
- `unknown (hidden directive block …)` means a `{$IF}` region could not be parsed.
- `not freed in this routine` describes ownership (returned as `Result`,
  stored in a field); it is not a leak claim.
- Facts are file-local: a call is a name, not the callee's behaviour. Digest
  or read the callee when its effect matters.
- `!` lines warn about parse errors, conditional code (every `{$IF}` branch is
  shown) and anonymous methods (not analysed).

Exit codes: 0 success; 2 error (unreadable file, unknown routine or line; the
closest routines are listed on stderr).
