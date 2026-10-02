# lint4d

## Editable dependencies

Local sibling checkouts — edit upstream instead of working around bugs:

- `../tree-sitter-pascal` — git dep (`github.com/AntoineGS/tree-sitter-pascal`; grammar, parser, queries); edit, push, bump the rev in `crates/lint4d` and `crates/pascal-core` (kept per crate: cargo-deny cannot resolve git workspace dependencies). cfg-pascal parses with the same grammar, so it must pin the **same** rev: bump it in cfg-pascal first, push, then bump the pins here
- `../cfg-core` — git dep (`github.com/AntoineGS/cfg-core`); edit, push, bump rev
- `../cfg-pascal` — git dep (`github.com/AntoineGS/cfg-pascal`); same flow

A gitignored `.cargo/config.toml` may build against these checkouts via `paths`
overrides, which leave `Cargo.lock` on the pinned revs; CI always uses the pins.
Overrides cannot change a crate's dependency list: when a sibling adds or
removes a dependency, push it and bump the pin first.

## Backlog (required)

Tracked work lives in `backlog/`, managed with the Backlog.md CLI (`backlog`).
Change tasks through the CLI only; never hand-edit files under `backlog/`.
Run `backlog instructions` for the full workflow.

- **Find work:** `backlog task list --plain`, `backlog task <id> --plain`.
  Respect `dependencies`; milestones `M1`..`M7` give the intended order.
- **Start:** `backlog task edit <id> -s "In Progress" --plan "<steps>"`
  before writing code.
- **Complete:** only after the acceptance criteria are verified (tests, fmt,
  clippy). Check each one with `--check-ac <n>`, add `--final-summary`
  (what changed, commits, how it was verified), then `-s Done`.
- **Defer or put off:** set `-s Deferred` and `--append-notes` with the
  reason, what was done so far, and what would unblock it. A deferred task
  must never be left `In Progress`.
- **Partial work or new findings:** record progress with `--append-notes`;
  create new tasks (`backlog task create`) for follow-ups or bugs discovered,
  and link them with `--dep`. Fix stale `file:line` citations you notice.
- Commit backlog changes together with the code change that caused them.
- Do not keep separate TODO files. The former `AGENT_TODOS.md` was
  converted into `agent-todos` tasks; its original text is archived in
  `backlog/docs/` (`backlog doc view doc-1`). `TODOS.md` is the user's own
  list: do not edit it.

`arch-review` tasks come from `2026-10-02-architecture-review-backlog.md`.
Their titles keep the review IDs (`LSP-3`, `CORE-7`, ...); read that
file's Global constraints section before starting one.

## Bug fixes

TDD required: failing test first, minimal fix, refactor. No fix without a regression test.
