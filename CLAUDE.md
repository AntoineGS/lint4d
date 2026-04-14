# lint4d

## Editable dependencies

Local sibling checkouts — edit upstream instead of working around bugs:

- `../tree-sitter-pascal` — path dep (grammar, parser, queries)
- `../cfg-core` — git dep (`github.com/AntoineGS/cfg-core`); edit, push, bump rev
- `../cfg-pascal` — git dep (`github.com/AntoineGS/cfg-pascal`); same flow

## Bug fixes

TDD required: failing test first, minimal fix, refactor. No fix without a regression test.
