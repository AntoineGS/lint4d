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

Tracked work lives in `backlog/`, managed with Backlog.md. Access it **only
through the Backlog.md MCP server** (`backlog mcp start`): read and change
tasks, milestones and docs with its tools. Do not read, grep, glob or
hand-edit files under `backlog/`, and do not use the `backlog` CLI. If the
MCP tools are not available in the session, stop and ask the user to
connect the server instead of falling back. Call
`get_backlog_instructions` for the full workflow.

- **Find work:** `task_list` (filter by `status`, `milestone`, `labels`;
  `ready` for unblocked tasks), `task_search`, `task_view`.
  Respect `dependencies`; milestones `M1`..`M7` give the intended order.
- **Start:** `task_edit` with `status: "In Progress"` and `planSet`
  before writing code.
- **Complete:** only after the acceptance criteria are verified (tests, fmt,
  clippy). Check each one with `acceptanceCriteriaCheck`, add
  `finalSummary` (what changed, commits, how it was verified), then set
  `status: "Done"`.
- **Defer or put off:** set `status: "Deferred"` and `notesAppend` with the
  reason, what was done so far, and what would unblock it. A deferred task
  must never be left `In Progress`.
- **Partial work or new findings:** record progress with `notesAppend`;
  create new tasks (`task_create`) for follow-ups or bugs discovered, and
  link them with `dependencies`. Fix stale `file:line` citations you notice.
- Commit backlog changes together with the code change that caused them.
- Do not keep separate TODO files. The former `AGENT_TODOS.md` was
  converted into `agent-todos` tasks; its original text is archived as a
  Backlog.md document (`document_view` with id `doc-1`). `TODOS.md` is the
  user's own list: do not edit it.

`arch-review` tasks come from a 2026-10-02 architecture review that is no
longer in the repo. Their titles keep the review IDs (`LSP-3`, `CORE-7`,
...).

## Bug fixes

TDD required: failing test first, minimal fix, refactor. No fix without a regression test.
