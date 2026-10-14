# Canonical immutable `ProjectContext` Implementation Plan (l4d-51)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `pascal_project::ProjectContext` one immutable, provenance-only representation of analysis inputs, split observations out of it, and make the context build cache key include the session selections that construction consumes.

**Architecture:** Five commits in the order of spec §8. (1) A shared, deterministic per-project selection lookup feeds both construction and the cache key. (2) Observations (`metadata_files`, `metadata_observations`, `warnings`, `system_pas_searches`) move to a new `ProjectObservations` value returned beside the context. (3) Bare path lists and `defines` stop being stored and become derived accessors. (4) Fields become `pub(crate)` with read accessors; tests build contexts through a test-only `ProjectContextParts`. (5) Contexts are shared as `Arc<ProjectContext>`.

**Tech Stack:** Rust 1.99 (edition 2024) workspace: `pascal-project`, `pascal-core`, `pascal-lsp`, `lint4d`.

**Spec:** `docs/superpowers/specs/2026-10-14-canonical-project-context-design.md`. Read it before any task; the plan argues from it.

## Global Constraints

- Toolchain pinned to Rust `1.99.0`; `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` must pass after every task. Workspace lints deny warnings.
- No new dependencies of any kind.
- TDD: each task's new tests are written and run red before the production change; record the red command and the relevant failing output in the task report. A compile error for a not-yet-existing API counts as red only when the task says so.
- Behavior: only the changes listed in spec §7 are allowed. Any other observable change is a regression.
- No persistent encoding, strong digest, BLAKE3 or SQLite work (phase 2, `l4d-zf1b.2`).
- One local commit per task, message naming `l4d-51`. No push, no merge, no `git stash`, no `bd` (the controller tracks Beads), no edits to `TODOS.md`.
- Failures present at the base commit are not regressions but must not grow: the two `pascal-core` `tests/compile_time_includes.rs` `fix_i3_*` tests; in `protocol_barriers` (`--features test-support`) `document_links_reject_case_variant_higher_priority_candidate_with_restored_directory_stamp` (`l4d-1jfh`/`l4d-l9iv`) and the flaky `cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline` / `shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline`; the `TMPDIR`-sensitive resolver cancellation test (`l4d-6qe5`). The preflight records the exact list at the base commit.
- `crates/pascal-lsp/tests/protocol.rs` is very long (hundreds of server launches). Run it only where a task says so, with a long timeout and `--no-fail-fast`.
- Locate code by symbol name with grep; the line numbers below are for `master` `15c9126` and drift as tasks land.

## Review Focus

1. **Equality whose meaning shifts.** Once observations leave the context, `==` on `ProjectContext` ignores them. LSP `ContextState` equality and `rename.rs` `project_contexts_match_for_explicit_in_path` must still compare the observations they compared before; reuse of parsed documents and resolver session caches may now ignore them (spec §4.2, §7). Task 2 pins `ContextState` and audits every site.
2. **Observations lost on a discovery-cache hit.** A second discovery served from `ProjectDiscoveryCacheScope` must return the same observations as the build that filled the cache. Task 2 test `cached_discovery_returns_the_construction_observations`.
3. **Resolver reports silently losing seeded observations.** Every production `UnitResolver` construction that had a discovered context must call `with_context_observations`, or LSP revalidation misses metadata files. Task 2 test plus a grep gate over the four production sites.
4. **Duplicate paths with different provenance.** The derived `search_paths()`, `include_paths()` and `explicit_units()` must deduplicate by path, first occurrence wins, exactly like the removed stored lists (`assembly.rs` `paths_from_entries`, `add_unit_candidate`). Task 3 test `bare_path_views_are_derived_from_entries`.
5. **Selection keys that name one project with different spellings.** `/w/./App.dproj` and `/w/App.dproj` both match one project; the lookup must pick the exact key, otherwise the smallest key by bytes, independent of `HashMap` order. Task 1 test `session_selections_prefer_the_exact_project_path`.

---

## Preflight (controller, before Task 1)

- [ ] Create the worktree/branch from `master` and confirm `git log --oneline -1` is the intended base.
- [ ] Check overlap with in-progress `l4d-l9iv`: `git branch -a --contains` and `git log master..<l4d-l9iv branch> -- crates/pascal-lsp/src/workspace.rs crates/pascal-lsp/src/workspace/rename.rs` (if a branch exists). Record the result in the ledger; if there are unmerged edits to those files, tell the user before Task 2.
- [ ] Record base failures:

```bash
cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/opencode/l4d-51-base-workspace.log | grep -E ' \.\.\. FAILED$|^test result: FAILED'
cargo test -p pascal-lsp --features test-support --test protocol_barriers --no-fail-fast 2>&1 | tee /tmp/opencode/l4d-51-base-barriers.log | grep -E ' \.\.\. FAILED$|^test result'
```

---

### Task 1: Deterministic session selections and a complete cache key

**Files:**
- Modify: `crates/pascal-project/src/context.rs` (add `SessionSelections`, `ProjectOptions::session_selections_for`, helper, unit test)
- Modify: `crates/pascal-project/src/assembly.rs` (`build_project_context`: the two lookups at ~269 and ~352)
- Modify: `crates/pascal-project/src/discovery_cache.rs` (`ProjectOptionsCacheKey`, ~90-121)
- Modify: `crates/pascal-project/src/selection.rs` (key construction, ~806)
- Test: `crates/pascal-project/tests/build_selection.rs`

**Interfaces:**
- Consumes: `pascal_project::ProjectDiscoveryCacheScope::{new, stats}`, `ProjectDiscoveryCacheStats { project_context_builds, project_context_cache_hits, .. }`; test helpers `write_webquery_fixture`, `discover_webquery_at` in `tests/build_selection.rs`.
- Produces (crate-private, used by later tasks unchanged):
  - `pub(crate) struct SessionSelections { pub(crate) installation: Option<String>, pub(crate) build: Option<BuildChoice> }` deriving `Debug, Clone, Default, PartialEq, Eq, Hash`
  - `impl ProjectOptions { pub(crate) fn session_selections_for(&self, project_file: &Path) -> SessionSelections }`
  - `impl ProjectOptionsCacheKey { pub(super) fn new(options: &ProjectOptions, project_file: &Path) -> Self }` replacing `From<&ProjectOptions>`

- [ ] **Step 1: Write the failing integration tests** (append to `crates/pascal-project/tests/build_selection.rs`; add `ProjectDiscoveryCacheScope` to the existing `use pascal_project::{...}` list)

```rust
#[test]
fn cached_discovery_honours_a_changed_session_build_choice() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let overrides = OverrideSession::new(Some(root.join("config.toml")));
    let choose = |config: &str| ProjectOptions {
        build_selections: [(
            root.join("App.dproj"),
            BuildChoice {
                config: Some(config.into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };
    let scope = ProjectDiscoveryCacheScope::new();

    let debug = discover_webquery_at(root, &choose("Debug"), &overrides);
    let release = discover_webquery_at(root, &choose("Release"), &overrides);

    assert_eq!(debug.config_selection.selected.as_deref(), Some("Debug"));
    assert_eq!(release.config_selection.selected.as_deref(), Some("Release"));
    assert_eq!(release.config_selection.mode, BuildSelectionMode::Session);
    assert_eq!(scope.stats().project_context_builds, 2);
}

#[test]
fn another_projects_selection_change_reuses_this_projects_cached_context() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let overrides = OverrideSession::new(Some(root.join("config.toml")));
    let other = root.join("Other").join("Other.dproj");
    let other_build = ProjectOptions {
        build_selections: [(
            other.clone(),
            BuildChoice {
                config: Some("Debug".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };
    let other_installation = ProjectOptions {
        installation_selections: [(other, "other-installation".to_string())]
            .into_iter()
            .collect(),
        ..ProjectOptions::default()
    };
    let scope = ProjectDiscoveryCacheScope::new();

    let first = discover_webquery_at(root, &ProjectOptions::default(), &overrides);
    let after_build = discover_webquery_at(root, &other_build, &overrides);
    let after_installation = discover_webquery_at(root, &other_installation, &overrides);

    let stats = scope.stats();
    assert_eq!(stats.project_context_builds, 1);
    assert_eq!(stats.project_context_cache_hits, 2);
    assert_eq!(first, after_build);
    assert_eq!(first, after_installation);
}

#[test]
fn session_build_choice_still_overrides_the_configured_choice_with_the_cache() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    write(
        &root.join("config.toml"),
        "[projects.\"App.dproj\"]\nconfig = 'Release'\n",
    );
    let overrides = OverrideSession::new(Some(root.join("config.toml")));
    let options = ProjectOptions {
        build_selections: [(
            root.join("App.dproj"),
            BuildChoice {
                config: Some("Debug".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };
    let _scope = ProjectDiscoveryCacheScope::new();

    let configured = discover_webquery_at(root, &ProjectOptions::default(), &overrides);
    let session = discover_webquery_at(root, &options, &overrides);

    assert_eq!(configured.config_selection.selected.as_deref(), Some("Release"));
    assert_eq!(session.config_selection.selected.as_deref(), Some("Debug"));
    assert_eq!(session.config_selection.mode, BuildSelectionMode::Session);
}
```

- [ ] **Step 2: Run them to verify the expected failures**

Run: `cargo test -p pascal-project --test build_selection -- cached_discovery_honours another_projects_selection_change session_build_choice_still_overrides`
Expected: `cached_discovery_honours_a_changed_session_build_choice` FAILS (second discovery returns the cached Debug context; `project_context_builds` is 1). `another_projects_selection_change_reuses_this_projects_cached_context` FAILS (the other project's installation selection changes the key: builds 2, hits 1). `session_build_choice_still_overrides_the_configured_choice_with_the_cache` FAILS too (the session discovery reuses the configured Release context). Record the output.

- [ ] **Step 3: Write the lookup unit test** (in `crates/pascal-project/src/context.rs` `mod tests`)

```rust
#[test]
fn session_selections_prefer_the_exact_project_path() {
    let exact = PathBuf::from("/work/App.dproj");
    let dotted = PathBuf::from("/work/./App.dproj");
    let doubly_dotted = PathBuf::from("/work/././App.dproj");
    let debug = BuildChoice {
        config: Some("Debug".into()),
        ..BuildChoice::default()
    };
    let release = BuildChoice {
        config: Some("Release".into()),
        ..BuildChoice::default()
    };
    let options = ProjectOptions {
        installation_selections: [
            (dotted.clone(), "dotted".to_string()),
            (exact.clone(), "exact".to_string()),
        ]
        .into_iter()
        .collect(),
        build_selections: [(dotted.clone(), release.clone()), (exact.clone(), debug.clone())]
            .into_iter()
            .collect(),
        ..ProjectOptions::default()
    };
    let selections = options.session_selections_for(&exact);
    assert_eq!(selections.installation.as_deref(), Some("exact"));
    assert_eq!(selections.build, Some(debug));

    let variants_only = ProjectOptions {
        build_selections: [(dotted, BuildChoice::default()), (doubly_dotted, release.clone())]
            .into_iter()
            .collect(),
        ..ProjectOptions::default()
    };
    // Neither key is exact; the smaller key by bytes wins in every run.
    // "/work/././App.dproj" < "/work/./App.dproj" because '.' < 'A'.
    for _ in 0..16 {
        assert_eq!(
            variants_only.session_selections_for(&exact).build,
            Some(release.clone())
        );
    }
    assert_eq!(
        ProjectOptions::default().session_selections_for(&exact),
        SessionSelections::default()
    );
}
```

(The exact key wins in the first map. In `variants_only` the byte-wise smaller key, `doubly_dotted`, is chosen however the `HashMap` iterates.)

- [ ] **Step 4: Implement the helper** (in `context.rs`, after `ProjectOptions`)

```rust
/// The session choices that apply to one project file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct SessionSelections {
    pub(crate) installation: Option<String>,
    pub(crate) build: Option<BuildChoice>,
}

impl ProjectOptions {
    /// Look up this project's session choices. Construction and the context
    /// cache key both use this, so they cannot disagree.
    pub(crate) fn session_selections_for(&self, project_file: &Path) -> SessionSelections {
        SessionSelections {
            installation: selection_for_project(&self.installation_selections, project_file)
                .cloned(),
            build: selection_for_project(&self.build_selections, project_file).cloned(),
        }
    }
}

/// An exact key wins; otherwise the smallest equivalent key by bytes, so the
/// result never depends on `HashMap` iteration order.
fn selection_for_project<'a, T>(
    selections: &'a HashMap<PathBuf, T>,
    project_file: &Path,
) -> Option<&'a T> {
    selections.get(project_file).or_else(|| {
        selections
            .iter()
            .filter(|(path, _)| project_paths_equal(path, project_file))
            .min_by(|left, right| left.0.as_os_str().cmp(right.0.as_os_str()))
            .map(|(_, value)| value)
    })
}
```

- [ ] **Step 5: Use it in construction** (`assembly.rs` `build_project_context`)

Replace the `session_build_choice` lookup (~269):

```rust
let session_selections = options.session_selections_for(&project_file);
let session_build_choice = session_selections.build.as_ref();
```

and the installation argument of `select_installation` (~351-355):

```rust
        session_selections.installation.as_deref(),
```

Before replacing the second site, confirm with `grep -n "evaluation_options.installation_selections" crates/pascal-project/src/assembly.rs` that nothing writes `evaluation_options.installation_selections` between the clone (~268) and the lookup; today nothing does.

- [ ] **Step 6: Key the cache by this project's selections** (`discovery_cache.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ProjectOptionsCacheKey {
    pub(super) project_file: Option<PathBuf>,
    pub(super) selections: SessionSelections,
    pub(super) build_config: Option<String>,
    pub(super) platform: Option<String>,
    pub(super) source_paths: Vec<String>,
    pub(super) conditional_context: ConditionalContext,
}

impl ProjectOptionsCacheKey {
    pub(super) fn new(options: &ProjectOptions, project_file: &Path) -> Self {
        Self {
            project_file: options.project_file.clone(),
            selections: options.session_selections_for(project_file),
            build_config: options.build_config.clone(),
            platform: options.platform.clone(),
            source_paths: options.source_paths.clone(),
            conditional_context: options.conditional_context.clone(),
        }
    }
}
```

Delete `impl From<&ProjectOptions> for ProjectOptionsCacheKey`. In `selection.rs` (~806) use `options: ProjectOptionsCacheKey::new(&options, &project_file),` — the same `project_file` later passed to `build_project_context`. Import `SessionSelections` where needed (`use super::context::SessionSelections;`).

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pascal-project` (all targets)
Expected: PASS, including the four new tests and every existing `build_selection` test.

- [ ] **Step 8: Gates and commit**

```bash
cargo test -p pascal-core -p pascal-lsp --lib --no-fail-fast
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pascal-project
git commit -m "fix(project): key cached contexts by this project's session selections (l4d-51)"
```

Expected: only base failures; fmt and clippy clean.

---

### Task 2: Split observations out of the context

**Files:**
- Create: `crates/pascal-project/src/observations.rs`
- Modify: `crates/pascal-project/src/lib.rs` (module, re-exports)
- Modify: `crates/pascal-project/src/context.rs` (remove four fields; `same_analysis_inputs`; `visit_recovery_payload`; `discover*` return type)
- Modify: `crates/pascal-project/src/assembly.rs` (`build_project_context`, `build_standalone_context*` return `DiscoveredProject`)
- Modify: `crates/pascal-project/src/reads.rs` (`ProjectDiscovery`, `into_discovery`)
- Modify: `crates/pascal-project/src/discovery_cache.rs` (`CachedProjectContext`)
- Modify: `crates/pascal-project/src/selection.rs` (construction call sites, `has_invalid_project_selection`)
- Modify: `crates/pascal-core/src/resolver.rs` (`new_inner` seeding, `with_context_observations`)
- Modify: `crates/pascal-lsp/src/workspace.rs` (`ContextState`, `merge_observations`, revalidation readers, `install_context`)
- Modify: `crates/pascal-lsp/src/workspace/resolver.rs` (`merge_report_into_context`, `resolver_for_context*`)
- Modify: `crates/pascal-lsp/src/workspace/rename.rs`, `workspace/queries.rs`, `workspace/code_lenses.rs`, `workspace/codeactions.rs`, `workspace/compile_time.rs`, `project_cache.rs`
- Modify: `crates/lint4d/src/main.rs` (~535-692)
- Modify (mechanical): every test that calls `ProjectContext::discover*` or reads/writes the four fields (compiler-driven)
- Test: `crates/pascal-project/src/context.rs`, `crates/pascal-project/tests/build_selection.rs`, `crates/pascal-core/tests/unit_resolver.rs`, `crates/pascal-lsp/src/workspace.rs` tests

**Interfaces:**
- Consumes: Task 1 (no API dependency beyond a green tree).
- Produces:
  - `pub struct ProjectObservations { pub metadata_files: Vec<PathBuf>, pub metadata_observations: Vec<MetadataObservation>, pub warnings: Vec<String>, pub system_pas_searches: Vec<SystemPasSearchObservation> }` deriving `Debug, Clone, Default, PartialEq, Eq`, with `pub fn visit_recovery_payload(&self, visit: &mut dyn FnMut(usize) -> Result<(), String>) -> Result<(), String>`
  - `pub struct DiscoveredProject { pub context: ProjectContext, pub observations: ProjectObservations }` deriving `Debug, Clone, Default, PartialEq, Eq` (becomes `Arc<ProjectContext>` in Task 5)
  - `ProjectContext::discover(..) -> Result<DiscoveredProject, String>` and `discover_with_overrides(..) -> Result<DiscoveredProject, String>`
  - `ProjectDiscovery { pub context: ProjectContext, pub context_observations: ProjectObservations, pub observations: Vec<ProjectReadObservation>, .. }` (the existing `observations` field keeps its meaning)
  - `pub fn has_invalid_project_selection(observations: &ProjectObservations) -> bool`
  - `UnitResolver::with_context_observations(self, observations: &ProjectObservations) -> Self`
  - LSP: `ContextState { context, context_observations: ProjectObservations, .. }`; `merge_report_into_context(observations: &mut ProjectObservations, report: &ResolutionReport)`
  - `ProjectContext` keeps derived `PartialEq, Eq`; `same_analysis_inputs` is deleted (callers use `==`).

- [ ] **Step 1: Write the tests**

`crates/pascal-project/src/context.rs` `mod tests` — replace `same_analysis_inputs_ignores_only_recorded_observations` with:

```rust
#[test]
fn equality_is_by_analysis_inputs_and_observations_stay_outside() {
    let base = ProjectContext {
        project_file: Some("A.dproj".into()),
        defines: vec!["DEBUG".to_string()],
        ..ProjectContext::default()
    };
    let observed = DiscoveredProject {
        context: base.clone(),
        observations: ProjectObservations {
            metadata_files: vec!["Unit.pas".into()],
            metadata_observations: vec![MetadataObservation::Stat {
                path: "Unit.pas".into(),
            }],
            warnings: vec!["warning".to_string()],
            system_pas_searches: Vec::new(),
        },
    };
    assert_eq!(base, observed.context);
    assert_ne!(
        observed,
        DiscoveredProject {
            context: base.clone(),
            observations: ProjectObservations::default(),
        }
    );

    let other_defines = ProjectContext {
        defines: vec!["RELEASE".to_string()],
        ..base.clone()
    };
    let other_includes = ProjectContext {
        include_paths: vec!["inc".into()],
        ..base.clone()
    };
    assert_ne!(base, other_defines);
    assert_ne!(base, other_includes);
}
```

(Task 3 rewrites the `defines`/`include_paths` literals when those fields go away.)

`crates/pascal-project/tests/build_selection.rs`:

```rust
#[test]
fn cached_discovery_returns_the_construction_observations() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let overrides = OverrideSession::new(Some(root.join("config.toml")));
    let _scope = ProjectDiscoveryCacheScope::new();

    let built = discover_webquery_project_at(root, &ProjectOptions::default(), &overrides);
    let cached = discover_webquery_project_at(root, &ProjectOptions::default(), &overrides);

    assert!(
        built
            .observations
            .metadata_files
            .iter()
            .any(|path| path.ends_with("App.dproj"))
    );
    assert_eq!(built.observations, cached.observations);
    assert_eq!(built.context, cached.context);
}
```

with the helper (keep `discover_webquery_at` returning the context so existing tests need no edit):

```rust
fn discover_webquery_project_at(
    root: &Path,
    options: &ProjectOptions,
    overrides: &OverrideSession,
) -> pascal_project::DiscoveredProject {
    ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        options,
        overrides,
    )
    .expect("WebQuery discovery")
}

fn discover_webquery_at(
    root: &Path,
    options: &ProjectOptions,
    overrides: &OverrideSession,
) -> ProjectContext {
    discover_webquery_project_at(root, options, overrides).context
}
```

`crates/pascal-core/tests/unit_resolver.rs` (add `MetadataObservation`, `ProjectObservations` to the `pascal_project` import and `ResolutionObservation` to the `pascal_core::resolver` import):

```rust
#[test]
fn context_observations_seed_the_resolution_report() {
    let observations = ProjectObservations {
        metadata_observations: vec![MetadataObservation::Stat {
            path: PathBuf::from("/workspace/App.dproj"),
        }],
        warnings: vec!["project warning".to_string()],
        ..ProjectObservations::default()
    };
    let resolver = UnitResolver::new(
        ProjectContext::default(),
        vec![PathBuf::from("/workspace")],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    )
    .with_context_observations(&observations);

    let report = resolver.report();
    assert!(report.warnings.iter().any(|warning| warning == "project warning"));
    assert!(report.observations.iter().any(|observation| matches!(
        observation,
        ResolutionObservation::Metadata(MetadataObservation::Stat { path })
            if path == Path::new("/workspace/App.dproj")
    )));
}
```

`crates/pascal-lsp/src/workspace.rs` test module (find it with `grep -n "^mod tests" crates/pascal-lsp/src/workspace.rs`):

```rust
#[test]
fn context_states_differing_only_in_observations_are_not_equal() {
    let state = |warnings: Vec<String>| ContextState {
        context: ProjectContext::default(),
        context_observations: pascal_project::ProjectObservations {
            warnings,
            ..Default::default()
        },
        watched_paths: HashMap::new(),
        project_candidate_memberships: HashMap::new(),
        project_read_observations: Vec::new(),
        system_pas_search_stamps: Mutex::new(HashMap::new()),
    };
    assert!(state(Vec::new()) != state(vec!["warning".to_string()]));
    assert!(state(vec!["warning".to_string()]) == state(vec!["warning".to_string()]));
}
```

- [ ] **Step 2: Run them red**

Run: `cargo test -p pascal-project --lib equality_is_by_analysis_inputs 2>&1 | grep -E "^error" | head`
Expected: compile errors — `ProjectObservations`, `DiscoveredProject` do not exist. This is the accepted red for a new API. Record it. (The other three tests fail to compile the same way; record one line each.)

- [ ] **Step 3: Add the observation types** (`crates/pascal-project/src/observations.rs`)

```rust
use super::*;

/// What reading a project produced besides its analysis inputs: the files
/// consulted, how they were authorized, warnings, and System.pas lookups.
/// Owners update it while resolving; it never affects context equality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectObservations {
    /// Project, main-source, imported option-set, and automatic-selection
    /// candidate files consulted. Consumers revalidate these paths without
    /// rediscovering or reparsing unrelated source files.
    pub metadata_files: Vec<PathBuf>,
    /// The authorization provenance for each metadata observation. A path in
    /// `metadata_files` without a payload observation is stat-only.
    pub metadata_observations: Vec<MetadataObservation>,
    pub warnings: Vec<String>,
    /// Ordered System.pas directory lookups used to derive RTL constants.
    pub system_pas_searches: Vec<SystemPasSearchObservation>,
}

impl ProjectObservations {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for path in &self.metadata_files {
            visit(path.as_os_str().len())?;
        }
        for search in &self.system_pas_searches {
            visit(search.directory.as_os_str().len())?;
            if let Some(path) = &search.selected_path {
                visit(path.as_os_str().len())?;
            }
        }
        for observation in &self.metadata_observations {
            observation.visit_recovery_payload(visit)?;
        }
        for warning in &self.warnings {
            visit(warning.len())?;
        }
        Ok(())
    }
}

/// A discovered context and the observations made while constructing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredProject {
    pub context: ProjectContext,
    pub observations: ProjectObservations,
}
```

In `lib.rs` add `mod observations;` and `pub use observations::{DiscoveredProject, ProjectObservations};` next to the `context` re-export.

- [ ] **Step 4: Move the fields in `pascal-project`**

1. `context.rs`: delete `metadata_files`, `system_pas_searches`, `metadata_observations`, `warnings` from `ProjectContext`, with their doc comments (now on `ProjectObservations`). Delete `same_analysis_inputs`. Delete the four observation loops from `ProjectContext::visit_recovery_payload` (they live in `ProjectObservations::visit_recovery_payload`). `discover` and `discover_with_overrides` end with `.map(|discovery| DiscoveredProject { context: discovery.context, observations: discovery.context_observations })`.
2. `assembly.rs`: `build_project_context`, `build_standalone_context_with_overrides` and `build_standalone_context` return `Result<DiscoveredProject, String>`. In each final literal, move the four values out of `ProjectContext { .. }` into `DiscoveredProject { context: ProjectContext { .. }, observations: ProjectObservations { metadata_files, metadata_observations, warnings: builder.warnings /* or the local name used today */, system_pas_searches: builder.system_pas_searches } }`. Keep every computation unchanged; only the destination moves. Callers that mutate the result before returning (`assembly.rs:~55` `let mut context = build_standalone_context(..)`, `selection.rs` `let mut context = build_project_context(..)` / `build_standalone_context_with_overrides(..)`) mutate `project.context.<field>` or `project.observations.<field>` — the same field they touch today.
3. `discovery_cache.rs`: `CachedProjectContext { project: DiscoveredProject, observations, metadata_paths, metadata_observations }` (rename `context` to `project`; the other fields are the tracker's and keep their names).
4. `reads.rs`: `ProjectDiscovery` gains `pub context_observations: ProjectObservations` (doc: "Observations made while constructing `context`"). `into_discovery(self, project: DiscoveredProject, project_selection)` appends `self.metadata_paths` / `self.metadata_observations` into `project.observations` exactly as it appends into `context` today, and returns `ProjectDiscovery { context: project.context, context_observations: project.observations, observations: self.observations, .. }`.
5. `selection.rs`: callers pass the `DiscoveredProject`; `has_invalid_project_selection(observations: &ProjectObservations)` reads `observations.warnings`.

Run `cargo build -p pascal-project --all-targets 2>&1 | grep -E "^(error|warning)" | sort | uniq -c` until clean; then fix `pascal-project` tests (`src/**` test modules and `tests/*.rs`): `ProjectContext::discover*(..).unwrap()` becomes `.unwrap().context` when only the context is used; reads of the four fields go to `.observations`.

- [ ] **Step 5: `pascal-core` resolver**

In `UnitResolver::new_inner` delete the two seeding loops over `context.metadata_observations` and `context.warnings`, and add:

```rust
    /// Seed the resolution report with what constructing the context
    /// observed, so report consumers can revalidate those reads.
    pub fn with_context_observations(mut self, observations: &ProjectObservations) -> Self {
        for observation in &observations.metadata_observations {
            push_observation(
                &mut self.report.observations,
                ResolutionObservation::Metadata(observation.clone()),
            );
        }
        for warning in &observations.warnings {
            push_warning(
                &mut self.report,
                warning.clone(),
                self.limits.max_resolution_warnings,
            );
        }
        self
    }
```

Fix `pascal-core` tests compiler-driven: `ProjectContext::discover*(..).unwrap()` becomes `.unwrap().context`; tests that asserted seeded warnings or observations call `.with_context_observations(&project.observations)`.

- [ ] **Step 6: Retarget production holders**

1. `pascal-lsp/src/workspace.rs`: `ContextState` gains `context_observations: ProjectObservations` (included in `Clone`, in `PartialEq` as `&& self.context_observations == other.context_observations`, and in `visit_recovery_payload` right after `self.context.visit_recovery_payload(visit)?`). `install_context` takes `discovery.context_observations` alongside `discovery.context` (both call sites: ~9780, ~13046) and stores it beside `state.context` in both the update and insert branches. `merge_observations` merges `retained.context_observations.{metadata_files,metadata_observations,system_pas_searches}` into `self.context_observations` with the same dedup predicates it uses today. Every reader of `state.context.{metadata_files,metadata_observations,warnings,system_pas_searches}` (`grep -nE "context\.(metadata_files|metadata_observations|warnings|system_pas_searches)" crates/pascal-lsp/src`) reads `state.context_observations.<field>`.
2. `pascal-lsp/src/workspace/resolver.rs`: `merge_report_into_context(observations: &mut ProjectObservations, report)` with an unchanged body except the target; its callers pass `&mut state.context_observations`. `resolver_for_context`, `resolver_for_context_with_limits` and `resolver_for_context_with_session_cache` gain `observations: &ProjectObservations` and call `.with_context_observations(observations)` on the constructed resolver; callers pass the observations of the `ContextState` they took the context from.
3. `pascal-lsp/src/workspace/compile_time.rs` (~808) and `lint4d/src/main.rs` (~552, from the discovery at ~681): keep the `DiscoveredProject`, construct the resolver from `project.context` and call `.with_context_observations(&project.observations)`. `main.rs:~692` iterates `project.observations.warnings`.
4. `code_lenses.rs`, `queries.rs`, `rename.rs`, `codeactions.rs`: `has_invalid_project_selection(&owner.state.context_observations)`; `codeactions.rs:~5985` clones the observations alongside the context.
5. `project_cache.rs`: replace every `x.same_analysis_inputs(y)` with `x == y`; in `context_differences` delete the `metadata_files`, `metadata_observations` (both blocks), `warnings` and `system_pas_searches` arms.
6. `rename.rs` `evaluated_contexts_equal`: delete the six `.clear()` lines; keep `normalize_standalone_legacy_roots` (Task 4 rewrites it). `project_contexts_match_for_explicit_in_path` (~4563) takes `(left: &ProjectContext, left_observations: &ProjectObservations, right: &ProjectContext, right_observations: &ProjectObservations)`, applies its `.delphilsp.json` filter to clones of the observation records, and returns `left == right && left_files == right_files && left_observations.metadata_observations == right_observations.metadata_observations && left_observations.warnings == right_observations.warnings && left_observations.system_pas_searches == right_observations.system_pas_searches` — the exact set it compared before. Its caller (~4538) passes the consumer and provider states' observations.
7. `rename.rs:~2885` `consumed_context_records` reads `state.context_observations.metadata_files`.

- [ ] **Step 7: Audit every production `==`/`!=` on `ProjectContext`**

Run: `grep -rnE "context (==|!=) |\.context\)? (==|!=)|== \*?context\b|context == " crates/pascal-lsp/src crates/pascal-core/src crates/lint4d/src | grep -v "conditional_context"` and classify each hit in the task report, one line per site:
- State replacement or revalidation (`ContextState` eq, `rename.rs` `retain_context` ~1160, `project_contexts_match_for_explicit_in_path`): must compare observations too — done in Step 6.
- Reuse of parsed documents or resolver session caches (`queries.rs:~1544`, `rename.rs:~6662`, `workspace.rs:~8169`, `resolver.rs:~8131` `ResolverSessionCacheScope`, `compile_time/session.rs:~1911/1931` if its `context` is a `ProjectContext`): context `==` is the intended meaning (spec §7).
Any site that fits neither: stop and report NEEDS_CONTEXT.

- [ ] **Step 8: Gate that every production resolver is seeded**

Run: `grep -rn "UnitResolver::new" crates/pascal-lsp/src crates/lint4d/src | grep -v "mod tests"` and confirm each production construction is followed by `.with_context_observations(`. Paste the output into the report.

- [ ] **Step 9: Fix remaining tests and run green**

Fix the remaining test compile errors compiler-driven (`cargo test --workspace --no-run 2>&1 | grep -E "^error" -A5`). LSP test `ContextState { .. }` literals (`rename.rs` ~10960-11654) add `context_observations: Default::default()` or move their observation fields there.

Run:
```bash
cargo test -p pascal-project
cargo test -p pascal-core --no-fail-fast
cargo test -p pascal-lsp --lib
cargo test -p lint4d --no-fail-fast
cargo test -p pascal-lsp --test protocol --no-fail-fast   # long; timeout >= 3600000 ms
```
Expected: the four new tests pass; only base failures remain.

- [ ] **Step 10: Gates and commit**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
git add -A crates
git commit -m "refactor(project): carry observations beside the project context (l4d-51)"
```

---

### Task 3: Store paths once and derive `defines()`

**Files:**
- Modify: `crates/pascal-project/src/context.rs` (remove `main_source`, `search_paths`, `include_paths`, `explicit_units`, `defines`; add derived accessors; drop fallbacks; `effective_conditional_context`; `source_origin`; `visit_recovery_payload`)
- Modify: `crates/pascal-project/src/authorization.rs` (`ProjectPathEntry::configured`)
- Modify: `crates/pascal-project/src/assembly.rs` (stop computing the stored duplicates: ~544, ~617, ~632, ~634-738, ~857-865, ~1048, ~1080-1085, the two final literals; `paths_from_entries` becomes `pub(super)`; `add_unit_candidate_entry` stops filling the bare map)
- Modify: `crates/pascal-core/src/resolver.rs` (fallback ~3919; admission ~7986), `crates/pascal-lsp/src/navigation/compiled_dcu.rs` (~340), and every other reader (compiler-driven)
- Test: `crates/pascal-project/src/context.rs`, `crates/pascal-project/tests/build_selection.rs`

**Interfaces:**
- Consumes: Task 2's `DiscoveredProject`, `discover_webquery_project_at`.
- Produces (on `ProjectContext`, all `pub`):
  - `fn search_paths(&self) -> Vec<PathBuf>` — paths of `search_path_entries`, unique, first occurrence order
  - `fn include_paths(&self) -> Vec<PathBuf>` — same for `include_path_entries`
  - `fn explicit_units(&self) -> HashMap<String, Vec<PathBuf>>` — per name, unique paths of the entries in order
  - `fn main_source(&self) -> Option<&Path>` — path of `main_source_entry`
  - `fn defines(&self) -> Vec<String>` — names whose fact in `conditional_context` is `ConditionalFact::True`, in `BTreeMap` (sorted) order
  - `ProjectPathEntry::configured(path: PathBuf) -> ProjectPathEntry` (`Configured` provenance)
  - Fields `search_path_entries`, `include_path_entries`, `explicit_unit_entries`, `main_source_entry`, `conditional_context` remain public fields until Task 4.

- [ ] **Step 1: Write the defines test** (append to `crates/pascal-project/tests/build_selection.rs`; add `ConditionalFact` to imports if missing)

```rust
#[test]
fn defines_follow_effective_facts_when_the_caller_sets_a_project_define_false() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let overrides = OverrideSession::new(Some(root.join("config.toml")));
    let mut conditional = ConditionalContext::default();
    conditional.set_define("NOSF", ConditionalFact::False);
    let options = ProjectOptions {
        build_config: Some("Debug".into()),
        conditional_context: conditional,
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &overrides);

    assert_eq!(
        context.conditional_context.define("NOSF"),
        ConditionalFact::False
    );
    let defines = context.defines.clone();
    assert!(defines.iter().any(|define| define == "DEBUG"));
    assert!(!defines.iter().any(|define| define == "NOSF"));
    let mut sorted = defines.clone();
    sorted.sort();
    assert_eq!(defines, sorted);
}
```

- [ ] **Step 2: Run it red**

Run: `cargo test -p pascal-project --test build_selection defines_follow_effective_facts`
Expected: FAIL at `!defines.iter().any(|define| define == "NOSF")` — today `defines` keeps the `DCC_Define` entry `NOSF` while the fact is False. Record it. (If it instead fails first at the sort check, record that; both are the old behavior.)

- [ ] **Step 3: Write the derived-views tests** (`crates/pascal-project/src/context.rs` `mod tests`)

```rust
#[test]
fn bare_path_views_are_derived_from_entries() {
    let a = PathBuf::from("/w/a");
    let b = PathBuf::from("/w/b");
    let unit = PathBuf::from("/w/Unit1.pas");
    let context = ProjectContext {
        search_path_entries: vec![
            ProjectPathEntry::legacy(a.clone()),
            ProjectPathEntry::configured(a.clone()),
            ProjectPathEntry::configured(b.clone()),
        ],
        include_path_entries: vec![ProjectPathEntry::configured(b.clone())],
        main_source_entry: Some(ProjectPathEntry::configured("/w/App.dpr".into())),
        explicit_unit_entries: [(
            "Unit1".to_string(),
            vec![
                ProjectPathEntry::legacy(unit.clone()),
                ProjectPathEntry::configured(unit.clone()),
            ],
        )]
        .into_iter()
        .collect(),
        ..ProjectContext::default()
    };
    assert_eq!(context.search_paths(), vec![a, b.clone()]);
    assert_eq!(context.include_paths(), vec![b]);
    assert_eq!(context.main_source(), Some(Path::new("/w/App.dpr")));
    assert_eq!(context.explicit_units()["Unit1"], vec![unit]);
}

#[test]
fn defines_are_the_true_facts_in_sorted_order() {
    let mut conditional = ConditionalContext::default();
    conditional.set_define("ZED", ConditionalFact::True);
    conditional.set_define("ALPHA", ConditionalFact::True);
    conditional.set_define("OFF", ConditionalFact::False);
    conditional.set_define("MAYBE", ConditionalFact::Unknown);
    let context = ProjectContext {
        conditional_context: conditional,
        ..ProjectContext::default()
    };
    assert_eq!(context.defines(), vec!["ALPHA".to_string(), "ZED".to_string()]);
}
```

- [ ] **Step 4: Run them red**

Run: `cargo test -p pascal-project --lib bare_path_views defines_are_the_true 2>&1 | grep -E "^error" | head`
Expected: compile errors — `configured`, `search_paths()`, `include_paths()`, `main_source()`, `explicit_units()`, `defines()` are fields or missing, not methods. Accepted red for the new API. Record it.

- [ ] **Step 5: Implement storage and accessors**

`authorization.rs`, beside `legacy`:

```rust
    pub fn configured(path: PathBuf) -> Self {
        Self {
            path,
            provenance: ProjectPathProvenance::Configured,
        }
    }
```

`context.rs`: delete the fields `main_source`, `search_paths`, `include_paths`, `explicit_units`, `defines` (move the doc comment of `search_paths` onto `search_path_entries` and that of `include_paths` onto `include_path_entries`), and add:

```rust
impl ProjectContext {
    /// Ordered unit search paths, without provenance.
    pub fn search_paths(&self) -> Vec<PathBuf> {
        paths_from_entries(&self.search_path_entries)
    }

    /// Ordered include paths, without provenance.
    pub fn include_paths(&self) -> Vec<PathBuf> {
        paths_from_entries(&self.include_path_entries)
    }

    /// Explicit unit references by unit name, without provenance.
    pub fn explicit_units(&self) -> HashMap<String, Vec<PathBuf>> {
        self.explicit_unit_entries
            .iter()
            .map(|(name, entries)| (name.clone(), paths_from_entries(entries)))
            .collect()
    }

    pub fn main_source(&self) -> Option<&Path> {
        self.main_source_entry.as_ref().map(|entry| entry.path.as_path())
    }

    /// Defines that are True in the effective conditional context, sorted.
    pub fn defines(&self) -> Vec<String> {
        self.conditional_context
            .defines
            .iter()
            .filter(|(_, fact)| **fact == ConditionalFact::True)
            .map(|(name, _)| name.clone())
            .collect()
    }
}
```

(`use super::assembly::paths_from_entries;` after making it `pub(super)` in `assembly.rs`.)

Also in `context.rs`:
- `include_search_entries`: `let include_paths = self.include_path_entries.clone();` and `let search_paths = self.search_path_entries.clone();` (no fallback).
- `effective_conditional_context`: keep only the clone and `absent_define = ConditionalFact::Unknown`.
- `source_origin`: compare `self.main_source()` and drop the `explicit_units` half of `is_explicit`.
- `visit_recovery_payload`: delete the visits of `main_source`, `search_paths`, `include_paths`, `explicit_units` and `defines` (spec §7).
- Doc comment of `conditional_context`: drop the sentence about the `defines` compatibility projection.

`assembly.rs`: stop computing what is no longer stored — `main_source` (~544), `search_paths` (~617, ~1048), `include_paths` (~632), the bare `explicit_units` map (`add_unit_candidate_entry` and `add_explicit_units_from_source` keep only the `entries` map; keep `add_unit_candidate` only if another caller remains), and `defines` (~857-865, ~1080-1085). Before deleting each local, grep the function for other uses of it (for example `project_source_roots` may be built from `search_paths`); if one exists, compute it from the entries there instead.

`pascal-core/src/resolver.rs` (~3919): `let search_entries = self.context.search_path_entries.clone();`. Admission (~7986): `for define in &context.defines()`.

`pascal-lsp/src/navigation/compiled_dcu.rs` (~340): `context.defines().hash(&mut hasher);`.

- [ ] **Step 6: Migrate readers and test literals (compiler-driven)**

Run `cargo test --workspace --no-run 2>&1 | grep -E "^error" -A6` and fix in order `pascal-project`, `pascal-core`, `pascal-lsp`, `lint4d`:
- Reads: `.search_paths` → `.search_paths()`, `.include_paths` → `.include_paths()`, `.explicit_units` → `.explicit_units()`, `.main_source` → `.main_source()` (now `Option<&Path>`: drop `.as_deref()`, use `.map(Path::to_path_buf)` where an owned value is needed), `.defines` → `.defines()`.
- Test literals setting `search_paths: vec![p, ..]` (with no `search_path_entries`): use `search_path_entries: vec![ProjectPathEntry::configured(p), ..]` when the literal's `read_policy` authorizes `p` as configured or no read policy is set; use the provenance `read_policy.entry_for_path(p)` would return otherwise (the old fallback chose it, and dropped `p` when it returned `None`). Same for `include_paths` and `main_source`. Literals setting both bare and entry lists keep only the entries.
- Test literals setting `defines: vec![..]`: put the names into `conditional_context` with `set_define(name, ConditionalFact::True)` (or `ConditionalContext::from_defines(&[..])` when no other conditional facts are set).
- Test assertions on `defines` order: if an existing test asserted `DCC_Define` order, change it to the sorted order and name it in the report (spec §7).
- `crates/pascal-project/tests/public_api.rs` (~41) builds a context with `defines: vec!["LEGACY".into()]` to exercise the removed `effective_conditional_context` merge. Set `LEGACY` True in that literal's `conditional_context` instead, keep its other assertions, and name it in the report.
- The Task 2 equality test: `defines: vec!["DEBUG".into()]` becomes a `conditional_context` with `DEBUG` True, and `other_defines` uses `RELEASE`; `include_paths: vec!["inc".into()]` becomes `include_path_entries: vec![ProjectPathEntry::configured("inc".into())]`.
- Rewrite `defines_follow_effective_facts_when_the_caller_sets_a_project_define_false` to use `context.defines()`.

- [ ] **Step 7: Run green**

```bash
cargo test -p pascal-project
cargo test -p pascal-core --no-fail-fast
cargo test -p pascal-lsp --lib
cargo test -p lint4d --no-fail-fast
cargo test -p pascal-lsp --test protocol --no-fail-fast   # long
```
Expected: new tests pass; only base failures remain. A changed existing assertion is allowed only for `defines` order/content per spec §7 and must be listed in the report.

- [ ] **Step 8: Gates and commit**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
git add -A crates
git commit -m "refactor(project): store project paths once and derive defines (l4d-51)"
```

---

### Task 4: Encapsulate the context

**Files:**
- Modify: `crates/pascal-project/src/context.rs` (fields `pub(crate)`, accessors, `ProjectContextParts`, `from_parts`/`into_parts`, `eq_ignoring_legacy_search_roots`, compile-fail doctest)
- Modify: `crates/pascal-project/src/lib.rs` (re-export `ProjectContextParts` under cfg)
- Modify: `crates/pascal-project/Cargo.toml` (self dev-dependency enabling `test-support` for its integration tests)
- Modify: `crates/pascal-lsp/src/workspace/rename.rs` (`evaluated_contexts_equal`, delete `normalize_standalone_legacy_roots`)
- Modify (mechanical): every reader in `pascal-core`, `pascal-lsp`, `lint4d` (field → accessor), every test literal and test field write outside `pascal-project/src`
- Test: doctest on `ProjectContext`; `crates/pascal-project/src/context.rs` tests

**Interfaces:**
- Consumes: Tasks 2-3 field set.
- Produces:
  - Read accessors on `ProjectContext` named after each field (below).
  - `#[cfg(any(test, feature = "test-support"))] pub struct ProjectContextParts { pub <every field>: <type> }` deriving `Debug, Clone, Default, PartialEq, Eq`, re-exported as `pascal_project::ProjectContextParts` under the same cfg.
  - `#[cfg(any(test, feature = "test-support"))] pub fn ProjectContext::from_parts(parts: ProjectContextParts) -> ProjectContext` and `pub fn into_parts(self) -> ProjectContextParts`.
  - `pub fn ProjectContext::eq_ignoring_legacy_search_roots(&self, other: &Self) -> bool`.

- [ ] **Step 1: Write the compile-fail doctest** (on `pub struct ProjectContext` in `context.rs`, appended to its doc comment)

```rust
/// Other crates read a context through accessors and cannot change it:
///
/// ```compile_fail,E0616
/// let mut context = pascal_project::ProjectContext::default();
/// context.discovery_complete = true;
/// ```
```

and the legacy-root comparator test in `context.rs` `mod tests`:

```rust
#[test]
fn legacy_search_roots_can_be_ignored_in_comparison() {
    let configured = ProjectPathEntry::configured("/w/src".into());
    let with_legacy = ProjectContext {
        search_path_entries: vec![ProjectPathEntry::legacy("/w".into()), configured.clone()],
        ..ProjectContext::default()
    };
    let without_legacy = ProjectContext {
        search_path_entries: vec![configured],
        ..ProjectContext::default()
    };
    assert_ne!(with_legacy, without_legacy);
    assert!(with_legacy.eq_ignoring_legacy_search_roots(&without_legacy));
    let other = ProjectContext {
        search_path_entries: vec![ProjectPathEntry::configured("/w/other".into())],
        ..ProjectContext::default()
    };
    assert!(!with_legacy.eq_ignoring_legacy_search_roots(&other));
}
```

- [ ] **Step 2: Run red**

Run: `cargo test -p pascal-project --doc ProjectContext` and `cargo test -p pascal-project --lib legacy_search_roots`
Expected: the doctest FAILS with "Test compiled successfully, but it's marked `compile_fail`" (fields are public today); the unit test fails to compile (`eq_ignoring_legacy_search_roots` missing). Record both.

- [ ] **Step 3: Make fields `pub(crate)` and add accessors** (`context.rs`)

Change every `pub` field of `ProjectContext` to `pub(crate)` (keep the doc comments) and add:

```rust
impl ProjectContext {
    pub fn discovery_complete(&self) -> bool {
        self.discovery_complete
    }
    pub fn binding_metadata_complete(&self) -> bool {
        self.binding_metadata_complete
    }
    pub fn path_issues(&self) -> &[ProjectPathIssue] {
        &self.path_issues
    }
    pub fn project_file(&self) -> Option<&Path> {
        self.project_file.as_deref()
    }
    pub fn installation_evidence(&self) -> &InstallationEvidence {
        &self.installation_evidence
    }
    pub fn installation_selection(&self) -> Option<&InstallationSelection> {
        self.installation_selection.as_ref()
    }
    pub fn search_path_entries(&self) -> &[ProjectPathEntry] {
        &self.search_path_entries
    }
    pub fn browsing_path_entries(&self) -> &[ProjectPathEntry] {
        &self.browsing_path_entries
    }
    pub fn debug_dcu_path_entries(&self) -> &[ProjectPathEntry] {
        &self.debug_dcu_path_entries
    }
    pub fn main_source_entry(&self) -> Option<&ProjectPathEntry> {
        self.main_source_entry.as_ref()
    }
    pub fn explicit_unit_entries(&self) -> &HashMap<String, Vec<ProjectPathEntry>> {
        &self.explicit_unit_entries
    }
    pub fn include_path_entries(&self) -> &[ProjectPathEntry] {
        &self.include_path_entries
    }
    pub fn unit_namespaces(&self) -> &[String] {
        &self.unit_namespaces
    }
    pub fn unit_aliases(&self) -> &HashMap<String, String> {
        &self.unit_aliases
    }
    pub fn conditional_context(&self) -> &ConditionalContext {
        &self.conditional_context
    }
    pub fn conditional_closure(&self) -> &ConditionalClosure {
        &self.conditional_closure
    }
    pub fn library_conditional_context(&self) -> &ConditionalContext {
        &self.library_conditional_context
    }
    pub fn project_source_roots(&self) -> &[PathBuf] {
        &self.project_source_roots
    }
    pub fn config(&self) -> Option<&str> {
        self.config.as_deref()
    }
    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }
    pub fn config_selection(&self) -> &BuildSelection {
        &self.config_selection
    }
    pub fn platform_selection(&self) -> &BuildSelection {
        &self.platform_selection
    }
    pub fn overrides(&self) -> &EffectiveOverrides {
        &self.overrides
    }
    pub fn read_policy(&self) -> &ReadPolicy {
        &self.read_policy
    }
    pub fn packages(&self) -> &[String] {
        &self.packages
    }
    pub fn installation_config_files(&self) -> &[PathBuf] {
        &self.installation_config_files
    }
    pub fn override_error(&self) -> Option<&str> {
        self.override_error.as_deref()
    }

    /// Equality that ignores `LegacyNative` search roots, which rename treats
    /// as incidental for contexts without a project file.
    pub fn eq_ignoring_legacy_search_roots(&self, other: &Self) -> bool {
        fn without_legacy_roots(context: &ProjectContext) -> ProjectContext {
            let mut context = context.clone();
            context
                .search_path_entries
                .retain(|entry| !matches!(entry.provenance, ProjectPathProvenance::LegacyNative));
            context
        }
        without_legacy_roots(self) == without_legacy_roots(other)
    }
}
```

If the field list differs from this after Tasks 2-3 (check by reading the struct), add or drop the matching accessor; every remaining field gets exactly one.

- [ ] **Step 4: Add the test-only parts**

In `context.rs`:

```rust
/// The stored fields of a [`ProjectContext`], for tests that build or adjust
/// one directly. Production contexts come from discovery.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContextParts {
    pub discovery_complete: bool,
    pub binding_metadata_complete: bool,
    pub path_issues: Vec<ProjectPathIssue>,
    pub project_file: Option<PathBuf>,
    pub installation_evidence: InstallationEvidence,
    pub installation_selection: Option<InstallationSelection>,
    pub search_path_entries: Vec<ProjectPathEntry>,
    pub browsing_path_entries: Vec<ProjectPathEntry>,
    pub debug_dcu_path_entries: Vec<ProjectPathEntry>,
    pub main_source_entry: Option<ProjectPathEntry>,
    pub explicit_unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    pub include_path_entries: Vec<ProjectPathEntry>,
    pub unit_namespaces: Vec<String>,
    pub unit_aliases: HashMap<String, String>,
    pub conditional_context: ConditionalContext,
    pub conditional_closure: ConditionalClosure,
    pub library_conditional_context: ConditionalContext,
    pub project_source_roots: Vec<PathBuf>,
    pub config: Option<String>,
    pub platform: Option<String>,
    pub config_selection: BuildSelection,
    pub platform_selection: BuildSelection,
    pub overrides: EffectiveOverrides,
    pub read_policy: ReadPolicy,
    pub packages: Vec<String>,
    pub installation_config_files: Vec<PathBuf>,
    pub override_error: Option<String>,
}

#[cfg(any(test, feature = "test-support"))]
impl ProjectContext {
    pub fn from_parts(parts: ProjectContextParts) -> Self {
        let ProjectContextParts {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
            conditional_context,
            conditional_closure,
            library_conditional_context,
            project_source_roots,
            config,
            platform,
            config_selection,
            platform_selection,
            overrides,
            read_policy,
            packages,
            installation_config_files,
            override_error,
        } = parts;
        Self {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
            conditional_context,
            conditional_closure,
            library_conditional_context,
            project_source_roots,
            config,
            platform,
            config_selection,
            platform_selection,
            overrides,
            read_policy,
            packages,
            installation_config_files,
            override_error,
        }
    }

    pub fn into_parts(self) -> ProjectContextParts {
        let Self {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
            conditional_context,
            conditional_closure,
            library_conditional_context,
            project_source_roots,
            config,
            platform,
            config_selection,
            platform_selection,
            overrides,
            read_policy,
            packages,
            installation_config_files,
            override_error,
        } = self;
        ProjectContextParts {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
            conditional_context,
            conditional_closure,
            library_conditional_context,
            project_source_roots,
            config,
            platform,
            config_selection,
            platform_selection,
            overrides,
            read_policy,
            packages,
            installation_config_files,
            override_error,
        }
    }
}
```

The destructuring has no `..`, so adding a field to `ProjectContext` breaks the build until the parts follow. If the struct's field list after Tasks 2-3 differs from this one, follow the struct. In `lib.rs`: `#[cfg(any(test, feature = "test-support"))] pub use context::ProjectContextParts;`.

In `crates/pascal-project/Cargo.toml` `[dev-dependencies]` add `pascal-project = { path = ".", features = ["test-support"] }` so `tests/*.rs` see the parts. If cargo rejects the self dev-dependency, stop and report NEEDS_CONTEXT.

- [ ] **Step 5: Rewrite the rename comparator** (`pascal-lsp/src/workspace/rename.rs`)

```rust
fn evaluated_contexts_equal(left: &ProjectContext, right: &ProjectContext) -> bool {
    if left.project_file().is_none() && right.project_file().is_none() {
        left.eq_ignoring_legacy_search_roots(right)
    } else {
        left == right
    }
}
```

Delete `normalize_standalone_legacy_roots`. (Before Task 3 it also removed bare `search_paths` equal to a legacy root; the bare list is now derived from the entries, so removing the entries is equivalent.)

- [ ] **Step 6: Migrate the other crates (compiler-driven)**

Run `cargo test --workspace --no-run 2>&1 | grep -E "^error\[E0(616|451|063|560)\]" -A6`. Fix by kind:
- Reads (`E0616`): `x.field` → `x.field()`; adapt borrowed types (`project_file()` is `Option<&Path>`, `config()` is `Option<&str>`, booleans are values). Prefer borrowing over cloning; clone only where an owned value is stored.
- Test literals (`E0451`): `ProjectContext { a: x, ..ProjectContext::default() }` → `ProjectContext::from_parts(ProjectContextParts { a: x, ..ProjectContextParts::default() })`; `..base.clone()` → `..base.clone().into_parts()`. Import `pascal_project::ProjectContextParts`.
- Test writes (`context.field = v;`): `let mut parts = context.into_parts(); parts.field = v; let context = ProjectContext::from_parts(parts);`. Inside `pascal-core`'s own `resolver.rs` tests that replace a resolver's context field, use the same through the resolver's field.
- Production code outside `pascal-project` must not need `from_parts`; if it does, stop and report NEEDS_CONTEXT.
- `pascal-project`'s own `src` code and unit tests keep struct literals and field access (`pub(crate)`). Its integration tests (`crates/pascal-project/tests/*.rs`) are a separate crate and migrate like the others.

- [ ] **Step 7: Run green**

```bash
cargo test -p pascal-project            # includes the doctest
cargo test -p pascal-core --no-fail-fast
cargo test -p pascal-lsp --lib
cargo test -p lint4d --no-fail-fast
cargo build --workspace                 # without test features: proves production does not use parts
cargo test -p pascal-lsp --test protocol --no-fail-fast   # long
```
Expected: doctest passes as `compile_fail`; all new tests pass; only base failures remain.

- [ ] **Step 8: Gates and commit**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
git add -A crates
git commit -m "refactor(project): make ProjectContext read-only outside pascal-project (l4d-51)"
```

---

### Task 5: Share contexts as `Arc<ProjectContext>`

**Files:**
- Modify: `crates/pascal-project/src/observations.rs` (`DiscoveredProject::context`), `reads.rs` (`ProjectDiscovery::context`), `discovery_cache.rs` (`CachedProjectContext`), `selection.rs` (wrap after the last in-crate mutation)
- Modify: `crates/pascal-core/src/resolver.rs` (`UnitResolver` field and constructors, `ResolverSessionCacheScope`)
- Modify: `crates/pascal-lsp/src/workspace.rs` (`ContextState::context`) and holders that clone contexts (compiler-driven)
- Test: `crates/pascal-project/tests/build_selection.rs`

**Interfaces:**
- Consumes: Task 4 accessors and parts.
- Produces:
  - `DiscoveredProject { pub context: Arc<ProjectContext>, pub observations: ProjectObservations }`
  - `ProjectDiscovery { pub context: Arc<ProjectContext>, .. }`
  - `UnitResolver::new(context: impl Into<Arc<ProjectContext>>, workspace_roots, store, limits)` and `new_with_session_cache(context: impl Into<Arc<ProjectContext>>, ..)`; the resolver stores `Arc<ProjectContext>`. Callers passing a `ProjectContext` keep compiling through `From<T> for Arc<T>`.
  - LSP `ContextState { context: Arc<ProjectContext>, .. }`.

- [ ] **Step 1: Write the sharing test** (`crates/pascal-project/tests/build_selection.rs`; `use std::sync::Arc;`)

```rust
#[test]
fn cached_discoveries_share_one_context() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let overrides = OverrideSession::new(Some(root.join("config.toml")));
    let _scope = ProjectDiscoveryCacheScope::new();

    let first = discover_webquery_project_at(root, &ProjectOptions::default(), &overrides);
    let second = discover_webquery_project_at(root, &ProjectOptions::default(), &overrides);

    assert!(Arc::ptr_eq(&first.context, &second.context));
}
```

- [ ] **Step 2: Run red**

Run: `cargo test -p pascal-project --test build_selection cached_discoveries_share 2>&1 | grep -E "^error" | head -3`
Expected: compile error — `Arc::ptr_eq` expects `&Arc<_>`, found `&ProjectContext`. Accepted red. Record it.

- [ ] **Step 3: Implement**

1. `pascal-project`: change the three field types. `CachedProjectContext` keeps the `DiscoveredProject` (now holding the `Arc`); a cache hit clones the `Arc`. In `selection.rs`, where a built context is mutated after caching (the override-error branch sets `discovery_complete = false` and `override_error` after inserting a cached copy), keep the cached copy unmutated: insert `Arc::new(context.clone())` into the cache, then mutate the local `context` and wrap it in a new `Arc`. Elsewhere wrap with `Arc::new(context)` once construction is complete. `discover_webquery_at` in the test file returns `ProjectContext` via `Arc::unwrap_or_clone(project.context)`.
2. `pascal-core`: `UnitResolver.context: Arc<ProjectContext>`; constructors take `context: impl Into<Arc<ProjectContext>>` and call `.into()` once. `ResolverSessionCacheScope.context: Arc<ProjectContext>` (`PartialEq` compares values through the `Arc`). Functions that take `&ProjectContext` keep doing so (`&resolver.context` derefs).
3. `pascal-lsp`: `ContextState.context: Arc<ProjectContext>`; `install_context` and other holders store the discovery's `Arc` instead of a clone. Where code clones a context into a long-lived holder, clone the `Arc`. Tests that `into_parts` a shared context use `Arc::unwrap_or_clone(context).into_parts()`.
4. Fix remaining compile errors compiler-driven (`cargo test --workspace --no-run`).

- [ ] **Step 4: Run green**

```bash
cargo test -p pascal-project
cargo test -p pascal-core --no-fail-fast
cargo test -p pascal-lsp --lib
cargo test -p lint4d --no-fail-fast
cargo test -p pascal-lsp --test protocol --no-fail-fast   # long
```
Expected: the sharing test passes; only base failures remain.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
git add -A crates
git commit -m "perf(project): share project contexts through Arc (l4d-51)"
```

---

## Final verification (controller, after Task 5)

- [ ] `cargo test --workspace --no-fail-fast` and `cargo test -p pascal-lsp --features test-support --test protocol_barriers --no-fail-fast`; compare failing test names with the preflight logs. Any new failure blocks completion.
- [ ] `cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `grep -rn "pub search_paths\|pub defines\|pub metadata_files\|same_analysis_inputs" crates/pascal-project/src/context.rs` returns nothing; `grep -n "^\s*pub [a-z_]*:" crates/pascal-project/src/context.rs` lists only `ProjectOptions`, `ProjectCandidates`, `ProjectCandidateMembership` and the cfg-gated `ProjectContextParts` fields.
- [ ] Whole-branch review against the spec, including the Review Focus list.
- [ ] Beads: record red/green evidence for tests 9.1, 9.2, 9.4, 9.5, 9.6 on `l4d-51`, check its two acceptance criteria, and close it only after the user's integration choice.
