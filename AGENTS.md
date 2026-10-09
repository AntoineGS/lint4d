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

## Work tracking — Beads (required)

Tracked work lives in Beads (`bd`, issue prefix `l4d`); run `bd prime` for
the command reference. The database lives on the shared Dolt server at
`~/.beads/shared-server/`, not in the repo, so issue updates never create
commits. Issue history syncs to `refs/dolt/data` on `origin`: writes
auto-push (`dolt.auto-push`), and `bd dolt pull` fetches changes made
elsewhere.

- **History:** Backlog.md is historical. Its last state is commit `55026f9`
  (`git show 55026f9:backlog/...`). `TASK-N` in older commits and records is
  issue `l4d-N` (`TASK-1.1` is `l4d-1.1`), and each imported issue keeps its
  Backlog ID as its external reference; `DRAFT-1` is `l4d-0jh`. Do not run
  `backlog` or recreate `backlog/`.
- **Find work:** `bd ready`, `bd list` (filter by `--status`, `--label`),
  `bd search`, `bd show`. Respect dependencies. Milestones M1..M7 are the
  epics `l4d-m1`..`l4d-m7`; their children (`bd children l4d-m1`) give the
  intended order. Shared context for `arch-review` issues is in
  `docs/architecture-review-context.md`.
- **Start:** `bd update <id> --claim` (sets `in_progress`) and record the plan
  with `--design` before writing code.
- **Complete:** only after the acceptance criteria are verified (tests, fmt,
  clippy). Check each one (`[x]`) with `bd update <id> --acceptance`, then
  `bd close <id> --reason` with the final summary (what changed, commits, how
  it was verified).
- **Defer or put off:** `bd update <id> --status deferred --append-notes`
  with the reason, what was done so far, and what would unblock it. A
  deferred issue must never be left `in_progress`.
- **Partial work or new findings:** record progress with `--append-notes`;
  create new issues (`bd create`) for follow-ups or bugs discovered, linked
  with `--deps discovered-from:<id>` or `bd dep add`. Fix stale `file:line`
  citations you notice.
- Name the issue in the commit message of code done for it. Do not commit
  merely to synchronize tracking.
- Do not keep separate TODO files. `TODOS.md` is the user's own list: do
  not edit it.

## Bug fixes

TDD required: failing test first, minimal fix, refactor. No fix without a regression test.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:46cd31e7 -->
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
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
