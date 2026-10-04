# lint4d

## Editable dependencies

The grammar and CFG crates live in this workspace (merged with `git subtree`,
see `docs/decisions/0001-monorepo.md`); fix bugs in them directly instead of
working around them, in the same commit as the code that needs the fix:

- `crates/tree-sitter-pascal` — grammar (`grammar.js`), generated parser
  (`src/parser.c`, `src/grammar.json`, `src/node-types.json`), external scanner
  (`src/scanner.c`) and queries. After editing `grammar.js`, run
  `tree-sitter generate` and `tree-sitter test` in that directory with
  tree-sitter CLI 0.24 (ABI 14; `npm install` there provides it as
  `npx tree-sitter`), and commit the regenerated `src/` files with the
  grammar change
- `crates/cfg-core` — language-agnostic CFG and interprocedural analysis
- `crates/cfg-pascal` — Pascal CFG builder; lint4d uses cfg-core only through
  its `cfg_pascal::cfg_core` re-export

All three are path dependencies, so there are no revs to bump and no
`.cargo/config.toml` overrides. The old `github.com/AntoineGS/{tree-sitter-pascal,cfg-core,cfg-pascal}`
repositories are no longer used by the build; do not add git dependencies on them.

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
- Do not keep separate TODO files. `TODOS.md` is the user's own list: do
  not edit it.

## Bug fixes

TDD required: failing test first, minimal fix, refactor. No fix without a regression test.
