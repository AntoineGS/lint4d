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


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:1105d646 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->
