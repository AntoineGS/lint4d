# lint4d

## Editable dependencies

Local sibling checkouts — edit upstream instead of working around bugs:

- `../tree-sitter-pascal` — git dep (`github.com/AntoineGS/tree-sitter-pascal`; grammar, parser, queries); edit, push, bump the rev in `[workspace.dependencies]`. cfg-pascal parses with the same grammar, so it must pin the **same** rev: bump it in cfg-pascal first, push, then bump both pins here
- `../cfg-core` — git dep (`github.com/AntoineGS/cfg-core`); edit, push, bump rev
- `../cfg-pascal` — git dep (`github.com/AntoineGS/cfg-pascal`); same flow

A gitignored `.cargo/config.toml` may build against these checkouts via `paths`
overrides, which leave `Cargo.lock` on the pinned revs; CI always uses the pins.
Overrides cannot change a crate's dependency list: when a sibling adds or
removes a dependency, push it and bump the pin first.

## Bug fixes

TDD required: failing test first, minimal fix, refactor. No fix without a regression test.
