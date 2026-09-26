# Task 2 implementation report

## Changed files

- `crates/pascal-project/src/path_issues.rs` — typed issue kind/data model.
- `crates/pascal-project/src/lib.rs` — context completeness APIs, missing DCCReference capture and stat observation, recovery accounting, and conservative handling of non-NotFound path inspection errors.
- `crates/pascal-core/src/resolver.rs` — use binding-specific completeness for positive unit lookup and reserve missing explicit unit names after alias application.
- `crates/pascal-project/tests/public_api.rs` — missing-reference usability/recovery and inaccessible-parent regressions.
- `crates/pascal-core/tests/unit_resolver.rs` — explicit-missing reservation, absent-provider path, and unreadable-priority-path regressions.
- `crates/pascal-lsp/tests/navigation.rs` — positive definition URI and same-name missing-unit fallback navigation regression.

`crates/pascal-lsp/src/workspace/resolver.rs` was not changed: the end-to-end navigation regression confirmed that this layer already preserves a positive `Found` result when the aggregate project report is incomplete. No files outside the task's allowed paths were edited, except this required report.

## Red/green evidence

- **Public missing-reference regression, API RED:**
  `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-project --test public_api missing_reference_keeps_independent_metadata_usable -- --exact`
  failed to compile because `ProjectContext::can_resolve_units` and `missing_explicit_unit` were absent (exit 101).
- **Public missing-reference regression, behavioral RED:** after adding the data shape and API with conservative defaults, the same command compiled and failed at `assert!(context.can_resolve_units())` (exit 101).
- **Public missing-reference regression, GREEN:** the same command passed (1 passed, 0 failed). After extending this test to check the missing-path `Stat` observation and rediscovery after file creation, it passed again.
- **Resolver regression, RED:**
  `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-core --test unit_resolver missing_explicit_reference_reserves_name_without_blocking_other_units -- --exact`
  failed because the unrelated `Good` lookup was not `Resolution::Found` with only `discovery_complete=false` (1 failed, exit 101).
- **Resolver regression, GREEN:** the same command passed after switching the lookup gate and adding the aliased missing-unit reservation (1 passed, 0 failed); the test also verifies the aggregate report remains incomplete.
- **Unreadable path classification, RED:**
  `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-project --test public_api inaccessible_reference_parent_is_not_a_proven_missing_unit -- --exact`
  failed because a non-directory parent was incorrectly recorded as a proven missing unit (1 failed, exit 101).
- **Unreadable path classification, GREEN:** rerunning the command after classifying only `NotFound` as absence passed (1 passed, 0 failed). Missing-reference public regression passed alongside it.
- **LSP navigation test:**
  `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-lsp --test navigation project_navigation_resolves_independent_unit_when_reference_is_missing -- --exact`
  passed (1 passed, 0 failed), including the actual Provider URI and empty navigation result for a missing explicit unit with `later/Missing.pas` present. One intermediate run failed because the fixture edit accidentally placed its `MissingRoutine` assertion in the neighboring test; the fixture was corrected and the final run passed.

## Broader verification

- `CARGO_TARGET_DIR=/home/antoinegs/lint4d/target cargo test --locked -p pascal-project --test public_api` — 26 passed, 0 failed.
- `CARGO_TARGET_DIR=/home/antoinegs/lint4d/target cargo test --locked -p pascal-core --test unit_resolver` — 50 passed, 0 failed.
- `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-lsp --test navigation` — 300 passed, 0 failed.
- `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-project` — 30 unit tests + 26 public API tests passed; doc tests passed. The known FIFO-substitution race test passed in this run.
- `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-core` — 100 unit tests, 52 conditional tests, 29 override tests, and 50 resolver tests passed; doc tests passed.
- `CARGO_TARGET_DIR=/home/antoinegs/gits/lint4d/target cargo test --locked -p pascal-lsp` — 671 navigation/workspace tests and 94 rename tests passed; doc tests passed. Prior protocol timeouts did not reproduce.
- `rustfmt --check --edition 2024` over all task-owned Rust paths — passed.
- `git diff --check` — passed.
- `cargo fmt --all -- --check` could not run: Cargo metadata resolved the editable `tree-sitter-pascal` dependency under `.worktrees/tree-sitter-pascal`, which reports it belongs to the root workspace and is not listed as a member. Direct `rustfmt --check` succeeded instead; this environment/workspace metadata issue was not modified.

## Unresolved issues / deviations

- No behavioral deviation from the task brief or design's missing-path policy identified.
- The first test failure for the main regression was the expected absent API compile failure; a subsequent test run confirmed the intended assertion failure before changing missing-path policy.
- The cargo-wide formatter command remains blocked by the sibling worktree metadata issue described above. Focused formatting and all target crate tests passed.
- Implementation checkpoint committed as `fix(resolver): scope missing project paths to affected lookups`; the normal pre-commit hook ran formatting and Clippy checks successfully.
