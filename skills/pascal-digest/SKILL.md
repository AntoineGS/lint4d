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
- `--all` digests every routine; `--format json` prints the data model.
- `--max-lines N` (default 300) applies to `--routine` and `--line` text output
  only; `--all`, the index and `--format json` are never collapsed. A collapsed
  block reads `L4430-4441 while not aQry.Eof … (12 lines, calls …)`: the head
  text, then the line count, then `calls …` only when the subtree has calls.

## Reading a digest

- Read only the source lines the digest points to. A verdict with
  `— verify L…-…` names the lines to read; the digest does not prove it.
- The digest compacts output: a `frequent calls:` line after `objects:` lists
  calls found in three or more separate runs, with total counts, and removes
  them from those runs; runs and controls with no fact are omitted; a chain of
  single-child controls with one fact reads on one line joined by ` → `
  (`L463 if … → L465 calls …`); long labels and raise text end in `…`.
- Verdict lines carry their own interpretation (`— verify L…` ranges,
  ownership notes such as `caller owns`); read them literally.
- A final `note:` line says calls are names only. Digest or read the callee
  when its effect matters.
- `unknown (hidden directive block at L…)`: the routine contains a
  `{$IF}…{$IFEND}` block the parser had to blank out; read that block yourself.
  `unknown (named inside with at L…)`: read that statement yourself.
- `!` lines warn about parse errors, `! hidden directive block at L…`,
  conditional code (every `{$IF}` branch is shown) and anonymous methods (not
  analysed).

Exit codes: 0 success; 2 error (unreadable file, unknown routine or line; the
closest routines are listed on stderr).
