//! Navigation over a dependency closure answers best effort (D16) and stops
//! a walk for a name that binds nowhere.

use super::{
    MAX_NAVIGATION_WALK_WORK, TEST_FAIL_EXPANSION_OF, TEST_FAIL_IMPORTS_OF,
    TEST_NAVIGATION_WALK_BYTES, Workspace, WorkspaceOptions,
};
use crate::NavigationTarget;
use lsp_types::{Location, Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn workspace(root: &Path) -> Workspace {
    let mut workspace = strict_workspace(root);
    workspace.set_best_effort_navigation(true);
    workspace
}

fn strict_workspace(root: &Path) -> Workspace {
    Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    )
}

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("source URI")
}

fn definition(workspace: &mut Workspace, main: &Path, position: Position) -> Vec<Location> {
    workspace
        .navigate_with_cancel(
            &uri(main),
            position,
            NavigationTarget::Definition,
            &AtomicBool::new(false),
        )
        .expect("navigation answers")
}

struct FailImports;

impl FailImports {
    fn of(file_name: &str) -> Self {
        TEST_FAIL_IMPORTS_OF.with(|name| *name.borrow_mut() = Some(file_name.to_string()));
        Self
    }
}

impl Drop for FailImports {
    fn drop(&mut self) {
        TEST_FAIL_IMPORTS_OF.with(|name| *name.borrow_mut() = None);
    }
}

/// `X.RootMember` binds through `TDep` (Dep) -> `TBase` (Base) -> `TRoot`
/// (Root). The walk loads the imports of `Bad` before those of `Base`.
fn inherited_member_fixture(root: &Path) {
    let files = [
        (
            "Main.pas",
            "unit Main;\ninterface\nuses Dep;\nimplementation\nprocedure Run;\nvar X: TDep;\nbegin\n  X.RootMember;\nend;\nend.\n",
        ),
        (
            "Dep.pas",
            "unit Dep;\ninterface\nuses Bad, Base;\ntype TDep = class(TBase) end;\nimplementation\nend.\n",
        ),
        (
            "Bad.pas",
            "unit Bad;\ninterface\nuses Root;\nconst BadValue = 1;\nimplementation\nend.\n",
        ),
        (
            "Base.pas",
            "unit Base;\ninterface\nuses Root;\ntype TBase = class(TRoot) end;\nimplementation\nend.\n",
        ),
        (
            "Root.pas",
            "unit Root;\ninterface\ntype TRoot = class\n  procedure RootMember;\nend;\nimplementation\nprocedure TRoot.RootMember; begin end;\nend.\n",
        ),
    ];
    for (name, source) in files {
        fs::write(root.join(name), source).expect("fixture source");
    }
}

#[test]
fn a_dependency_whose_imports_fail_to_load_is_skipped_and_marked() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    let mut workspace = workspace(root);
    let _fail = FailImports::of("/Base.pas");

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(7, 5));

    assert!(
        locations.is_empty(),
        "TRoot binds through Base: {locations:?}"
    );
    assert!(
        workspace.warnings.iter().any(|warning| warning
            .contains("navigation skipped the imports of")
            && warning.contains("Base.pas")),
        "the skipped unit must be reported: {:?}",
        workspace.warnings
    );
}

/// l4d-4nbs D44: best-effort navigation loads the imports its query looks
/// through: those of Bad, beside the ancestor chain, are not loaded, so
/// their failure does not touch the answer.
#[test]
fn best_effort_navigation_does_not_load_imports_off_the_ancestor_chain() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    let mut workspace = workspace(root);
    let _fail = FailImports::of("/Bad.pas");

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(7, 5));

    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0].uri, uri(&root.join("Root.pas")));
    assert!(
        !workspace
            .warnings
            .iter()
            .any(|warning| warning.contains("navigation skipped the imports of")),
        "{:?}",
        workspace.warnings
    );
}

#[test]
fn the_requested_documents_own_import_failure_still_fails_navigation() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    let mut workspace = workspace(root);
    let _fail = FailImports::of("/Main.pas");

    let result = workspace.navigate_with_cancel(
        &uri(&root.join("Main.pas")),
        Position::new(7, 5),
        NavigationTarget::Definition,
        &AtomicBool::new(false),
    );

    assert!(result.is_err(), "{result:?}");
}

/// `Unit0` uses `Unit1`, and so on; `Main` uses `Unit0`.
fn chain_fixture(root: &Path, units: usize, padding: usize) {
    let padding = format!("{{{}}}\n", "x".repeat(padding));
    for index in 0..units {
        let uses = if index + 1 < units {
            format!("uses Unit{};\n", index + 1)
        } else {
            String::new()
        };
        fs::write(
            root.join(format!("Unit{index}.pas")),
            format!(
                "unit Unit{index};\ninterface\n{uses}{padding}const Value{index} = {index};\nimplementation\nend.\n"
            ),
        )
        .expect("chain unit");
    }
    fs::write(
        root.join("Main.pas"),
        "unit Main;\ninterface\nuses Unit0;\nimplementation\nprocedure Run;\nbegin\n  Nowhere;\nend;\nend.\n",
    )
    .expect("main");
}

fn walk_stopped(workspace: &Workspace) -> bool {
    workspace
        .warnings
        .iter()
        .any(|warning| warning.contains("navigation walk limit"))
}

#[test]
fn a_negative_definition_stops_at_the_walk_unit_limit() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let units = MAX_NAVIGATION_WALK_WORK + 8;
    chain_fixture(root, units, 0);
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(6, 4));

    assert!(locations.is_empty(), "{locations:?}");
    assert!(walk_stopped(&workspace), "{:?}", workspace.warnings);
    let last = uri(&root.join(format!("Unit{}.pas", units - 1)));
    assert!(
        !workspace.index.contains(&last),
        "the walk must stop before the end of the chain"
    );
}

#[test]
fn a_negative_definition_stops_at_the_walk_byte_limit() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    chain_fixture(root, 8, 4096);
    let mut workspace = workspace(root);
    TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(Some(10_000)));

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(6, 4));
    TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(None));

    assert!(locations.is_empty(), "{locations:?}");
    assert!(walk_stopped(&workspace), "{:?}", workspace.warnings);
    assert!(workspace.index.contains(&uri(&root.join("Unit1.pas"))));
    assert!(
        !workspace.index.contains(&uri(&root.join("Unit5.pas"))),
        "the walk must stop once it indexed the byte limit"
    );
}

#[test]
fn a_definition_within_the_walk_limits_still_binds() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    chain_fixture(root, 4, 0);
    let main = root.join("Main.pas");
    fs::write(
        &main,
        "unit Main;\ninterface\nuses Unit0;\nimplementation\nprocedure Run;\nbegin\n  Value0;\nend;\nend.\n",
    )
    .expect("main");
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &main, Position::new(6, 4));

    assert_eq!(locations.len(), 1, "{locations:?}");
    assert!(!walk_stopped(&workspace), "{:?}", workspace.warnings);
}

#[test]
fn strict_navigation_does_not_skip_a_failed_dependency() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    let mut workspace = strict_workspace(root);
    let _fail = FailImports::of("/Bad.pas");

    let result = workspace.navigate_with_cancel(
        &uri(&root.join("Main.pas")),
        Position::new(7, 5),
        NavigationTarget::Definition,
        &AtomicBool::new(false),
    );

    assert!(
        result.is_err(),
        "the implementation lens stays strict: {result:?}"
    );
}

#[test]
fn a_skipped_dependency_is_logged_once_per_configuration_generation() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    let main_workspace = strict_workspace(root);
    let main = uri(&root.join("Main.pas"));
    let _fail = FailImports::of("/Base.pas");
    super::TEST_LOGGED_WARNINGS.with(|logged| logged.borrow_mut().clear());

    for _ in 0..2 {
        let locations = super::queries::navigation_from_input(
            main_workspace.analysis_input(),
            &main,
            Position::new(7, 5),
            NavigationTarget::Definition,
            &AtomicBool::new(false),
        )
        .value
        .expect("definition")
        .locations;
        assert!(
            locations.is_empty(),
            "TRoot binds through Base: {locations:?}"
        );
    }

    let logged = super::TEST_LOGGED_WARNINGS.with(|logged| logged.borrow().clone());
    let skipped = logged
        .iter()
        .filter(|warning| warning.contains("navigation skipped the imports of"))
        .count();
    assert_eq!(skipped, 1, "{logged:#?}");
}

#[test]
fn a_relaxed_global_binding_is_logged_by_definition() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fs::write(
        root.join("Dep.pas"),
        "unit Dep;\ninterface\nprocedure DepValue;\nimplementation\nprocedure DepValue; begin end;\nend.\n",
    )
    .expect("dependency");
    fs::write(
        root.join("Main.pas"),
        concat!(
            "unit Main;\ninterface\nuses Dep;\ntype\n  TThing = class(TBase)\n    procedure Run;\n  end;\n",
            "implementation\nprocedure TThing.Run;\nbegin\n  DepValue;\nend;\nend.\n",
        ),
    )
    .expect("main");
    let main_workspace = strict_workspace(root);
    super::TEST_LOGGED_WARNINGS.with(|logged| logged.borrow_mut().clear());

    let locations = super::queries::navigation_from_input(
        main_workspace.analysis_input(),
        &uri(&root.join("Main.pas")),
        Position::new(10, 4),
        NavigationTarget::Definition,
        &AtomicBool::new(false),
    )
    .value
    .expect("definition")
    .locations;

    assert_eq!(locations.len(), 1, "{locations:?}");
    let logged = super::TEST_LOGGED_WARNINGS.with(|logged| logged.borrow().clone());
    assert!(
        logged
            .iter()
            .any(|warning| warning.contains("Members inherited by TThing are unknown")),
        "{logged:#?}"
    );
}

/// l4d-4nbs review (D16 intent): a dependency off the ancestor chain that
/// the walk loads and that fails to load is skipped with a note, and the
/// answer still binds through the chain.
#[test]
fn an_off_chain_dependency_that_fails_to_load_is_noted_and_the_answer_binds() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    // Bad is loaded with Dep's imports, as it spells TBase, the name Dep's
    // lookup needs (l4d-ucux), and needs a compile-time session, which fails.
    fs::write(
        root.join("Bad.pas"),
        "unit Bad;\ninterface\nuses Root;\n{$IFDEF X}{$ENDIF}\nconst BadValue = 1; { not TBase }\nimplementation\nend.\n",
    )
    .expect("failing dependency");
    let mut workspace = workspace(root);
    TEST_FAIL_EXPANSION_OF.with(|name| *name.borrow_mut() = Some("/Bad.pas".into()));

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(7, 5));
    TEST_FAIL_EXPANSION_OF.with(|name| *name.borrow_mut() = None);

    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0].uri, uri(&root.join("Root.pas")));
    assert!(
        workspace
            .warnings
            .iter()
            .any(|warning| warning.contains("Bad.pas") && warning.contains("incomplete")),
        "the failed dependency must be noted: {:?}",
        workspace.warnings
    );
}

fn write_sources(root: &Path, files: &[(&str, &str)]) {
    for (name, source) in files {
        fs::write(root.join(name), source).expect("fixture source");
    }
}

fn indexed(workspace: &Workspace, root: &Path, name: &str) -> bool {
    workspace.index.contains(&uri(&root.join(name)))
}

/// `TTarget` in `Main`'s interface binds through `Target`; `Noise` and
/// `Bulky` cannot declare it.
fn lazy_import_fixture(root: &Path) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Noise, Target, Bulky;\ntype TUser = class\n  F: TTarget;\nend;\nimplementation\nuses Later;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\ntype TNoise = class end;\nimplementation\nend.\n",
            ),
            (
                "Target.pas",
                "unit Target;\ninterface\ntype TTarget = class end;\nimplementation\nend.\n",
            ),
            (
                "Bulky.pas",
                "unit Bulky;\ninterface\n{$I Bulky.inc}\nimplementation\nend.\n",
            ),
            ("Bulky.inc", "const BulkyValue = 1;\n"),
            (
                "Later.pas",
                "unit Later;\ninterface\nimplementation\nend.\n",
            ),
        ],
    );
}

/// l4d-ucux: a definition parses only the imports that can declare the name
/// it looks up. `Noise` never spells `TTarget`; `Bulky` spells it neither in
/// its text nor in the include it resolves; `Later` is an implementation
/// import an interface name never looks through.
#[test]
fn a_definition_parses_only_the_imports_that_can_declare_the_name() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    lazy_import_fixture(root);
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(4, 6));

    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0].uri, uri(&root.join("Target.pas")));
    for unit in ["Noise.pas", "Bulky.pas", "Later.pas"] {
        assert!(
            !indexed(&workspace, root, unit),
            "{unit} cannot declare TTarget and must not be parsed"
        );
    }
}

/// An include that spells the name keeps its unit a candidate: the unit is
/// parsed, and its declaration answers beside `Target`'s, as when every
/// import is parsed.
#[test]
fn an_import_whose_include_spells_the_name_is_parsed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    lazy_import_fixture(root);
    fs::write(root.join("Bulky.inc"), "type TTarget = class end;\n").expect("include");
    let main = root.join("Main.pas");
    let expected = definition(&mut strict_workspace(root), &main, Position::new(4, 6));
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &main, Position::new(4, 6));

    assert!(indexed(&workspace, root, "Bulky.pas"));
    assert_eq!(locations, expected);
    assert!(
        locations
            .iter()
            .any(|location| location.uri == uri(&root.join("Bulky.inc"))
                || location.uri == uri(&root.join("Bulky.pas"))),
        "{locations:?}"
    );
}

/// A member of an imported class: the walk parses the ancestor chain and
/// what can declare its names, never the unrelated imports along it.
#[test]
fn a_member_definition_parses_no_unrelated_import_along_the_chain() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    inherited_member_fixture(root);
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Noise, Dep;\nimplementation\nprocedure Run;\nvar X: TDep;\nbegin\n  X.RootMember;\nend;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\ntype TNoise = class end;\nimplementation\nend.\n",
            ),
        ],
    );
    let main = root.join("Main.pas");
    let expected = definition(&mut strict_workspace(root), &main, Position::new(7, 5));
    let mut workspace = workspace(root);

    let first = definition(&mut workspace, &main, Position::new(7, 5));
    let second = definition(&mut workspace, &main, Position::new(7, 5));

    assert_eq!(first, expected);
    assert_eq!(second, expected);
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].uri, uri(&root.join("Root.pas")));
    for unit in ["Noise.pas", "Bad.pas"] {
        assert!(
            !indexed(&workspace, root, unit),
            "{unit} is off the chain and must not be parsed"
        );
    }
}

/// A helper an import declares for an ancestor applies to the member: the
/// unit holding it is parsed before the member answers, so the answer is
/// the one a walk parsing every import gives.
#[test]
fn a_helper_in_an_import_that_spells_no_looked_up_type_still_answers() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Base, Helps;\nimplementation\nprocedure Run;\nvar X: TThing;\nbegin\n  X.Member;\nend;\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TThing = class\n  procedure Member;\nend;\nimplementation\nprocedure TThing.Member; begin end;\nend.\n",
            ),
            (
                "Helps.pas",
                "unit Helps;\ninterface\ntype TObjectHelps = class helper for TObject\n  procedure Member;\nend;\nimplementation\nprocedure TObjectHelps.Member; begin end;\nend.\n",
            ),
        ],
    );
    let main = root.join("Main.pas");
    let expected = definition(&mut strict_workspace(root), &main, Position::new(7, 5));
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &main, Position::new(7, 5));

    assert_eq!(locations, expected);
}

/// Review C1: a unit the index holds under older text is parsed again, never
/// skipped: the lookup would read the stale copy.
#[test]
fn a_unit_indexed_under_older_text_is_parsed_again_not_skipped() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    lazy_import_fixture(root);
    let main = root.join("Main.pas");
    let rewrite = || {
        fs::write(
            root.join("Target.pas"),
            "unit Target;\ninterface\ntype TOther = class end;\nimplementation\nend.\n",
        )
        .expect("rewrite")
    };
    let mut strict = strict_workspace(root);
    definition(&mut strict, &main, Position::new(4, 6));
    let mut workspace = workspace(root);
    let first = definition(&mut workspace, &main, Position::new(4, 6));
    assert_eq!(first.len(), 1, "{first:?}");
    rewrite();
    let expected = definition(&mut strict, &main, Position::new(4, 6));

    let locations = definition(&mut workspace, &main, Position::new(4, 6));

    assert_eq!(locations, expected);
    assert!(locations.is_empty(), "{locations:?}");
}

/// `Global` binds through `Dep`; the helper `Helps` declares for `TDep` adds
/// `Extra`, so the skipped `Helps` changes the answer once `Extra` is looked
/// up (review M4).
fn helper_answer_fixture(root: &Path, member: &str) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                &format!(
                    "unit Main;\ninterface\nuses Dep, Helps;\nimplementation\nprocedure Run;\nbegin\n  Global.{member};\nend;\nend.\n"
                ),
            ),
            (
                "Dep.pas",
                "unit Dep;\ninterface\nuses Base;\ntype TDep = class(TBase)\n  procedure Own;\nend;\nvar Global: TDep;\nimplementation\nprocedure TDep.Own; begin end;\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TBase = class\n  procedure BaseOwn;\nend;\nimplementation\nprocedure TBase.BaseOwn; begin end;\nend.\n",
            ),
            (
                "Helps.pas",
                "unit Helps;\ninterface\nuses Dep;\ntype TDepHelps = class helper for TDep\n  procedure Extra;\nend;\nimplementation\nprocedure TDepHelps.Extra; begin end;\nend.\n",
            ),
        ],
    );
}

#[test]
fn a_helper_in_a_skipped_import_changes_the_answer_as_with_every_import_parsed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    helper_answer_fixture(root, "Extra");
    let main = root.join("Main.pas");
    let expected = definition(&mut strict_workspace(root), &main, Position::new(6, 10));
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &main, Position::new(6, 10));

    assert_eq!(locations, expected);
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0].uri, uri(&root.join("Helps.pas")));
}

/// Review I2: at the walk limit, a lazily loaded document a lookup still
/// needs is loaded again (it costs no new unit of work).
#[test]
fn a_lazy_reload_is_not_dropped_at_the_walk_limit() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    helper_answer_fixture(root, "Extra");
    // At the limit once `Main`'s imports are loaded: `Dep`'s imports (for
    // `TBase`) sort before `Main`'s reload (for the helper).
    let main = root.join("Main.pas");
    TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(Some(1)));
    let expected = definition(&mut strict_workspace(root), &main, Position::new(6, 10));
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &main, Position::new(6, 10));
    TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(None));

    let mut strict = strict_workspace(root);
    TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(Some(1)));
    definition(&mut strict, &main, Position::new(6, 10));
    TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(None));
    assert_eq!(locations, expected);
    assert!(indexed(&strict, root, "Helps.pas"));
    assert!(
        indexed(&workspace, root, "Helps.pas"),
        "the helper lookup through Main's skipped Helps must be served"
    );
}

/// Review I3: an answer waiting on a lazily skipped import loads that import
/// only, not the other documents its lookups went through.
#[test]
fn an_answer_waiting_on_a_lazy_import_loads_only_that_import() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    helper_answer_fixture(root, "Own");
    // `Own` overloads one `TBase` declares: its lookup goes through `Dep`'s
    // imports for `TBase` too.
    write_sources(
        root,
        &[
            (
                "Dep.pas",
                "unit Dep;\ninterface\nuses Base;\ntype TDep = class(TBase)\n  procedure Own; overload;\nend;\nvar Global: TDep;\nimplementation\nprocedure TDep.Own; begin end;\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TBase = class\n  procedure Own(A: Integer); overload;\nend;\nimplementation\nprocedure TBase.Own(A: Integer); begin end;\nend.\n",
            ),
        ],
    );
    let main = root.join("Main.pas");
    let mut strict = strict_workspace(root);
    let expected = definition(&mut strict, &main, Position::new(6, 10));
    let mut workspace = workspace(root);

    let locations = definition(&mut workspace, &main, Position::new(6, 10));

    assert_eq!(locations, expected);
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(
        indexed(&workspace, root, "Base.pas"),
        indexed(&strict, root, "Base.pas"),
        "laziness must not load more of the closure than the full walk"
    );
}

/// Under `{$X+}` the implicit `Result` of a function shadows every
/// unit-level and imported name: an imported global `Result` is never its
/// definition (l4d-fpyu, review F4).
#[test]
fn a_functions_result_never_binds_to_an_imported_global() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\n{$X+}\ninterface\nuses Globals;\nfunction Get: Integer;\nimplementation\nfunction Get: Integer;\nbegin\n  Result := 1;\nend;\nend.\n",
            ),
            (
                "Globals.pas",
                "unit Globals;\ninterface\nvar Result: Integer;\nimplementation\nend.\n",
            ),
        ],
    );
    let main = root.join("Main.pas");

    let locations = definition(&mut workspace(root), &main, Position::new(8, 3));

    assert!(
        !locations
            .iter()
            .any(|location| location.uri == uri(&root.join("Globals.pas"))),
        "{locations:?}"
    );
}

/// Review I3 (red on 2997f26: `Base.pas`): `Foo` binds best effort to the
/// global in `Glob` (D16 fallback over `TMy`'s unknown members); waiting on
/// a lazily skipped import must not load `Base`, whose member `Foo` the
/// eager best-effort walk never sees.
#[test]
fn an_answer_waiting_on_a_lazy_import_equals_the_eager_best_effort_answer() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Dep, Glob, Args;\ntype TMy = class(TDep)\n  procedure Run;\nend;\nimplementation\nprocedure TMy.Run;\nbegin\n  Foo(ArgValue);\nend;\nend.\n",
            ),
            (
                "Dep.pas",
                "unit Dep;\ninterface\nuses Base;\ntype TDep = class(TBase) end;\nimplementation\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TBase = class\n  procedure Foo(A: Integer);\nend;\nimplementation\nprocedure TBase.Foo(A: Integer); begin end;\nend.\n",
            ),
            (
                "Glob.pas",
                "unit Glob;\ninterface\nprocedure Foo(A: Integer);\nimplementation\nprocedure Foo(A: Integer); begin end;\nend.\n",
            ),
            (
                "Args.pas",
                "unit Args;\ninterface\nconst ArgValue = 1;\nimplementation\nend.\n",
            ),
        ],
    );
    let main = root.join("Main.pas");
    super::TEST_EAGER_SNAPSHOT_IMPORTS.with(|eager| eager.set(true));
    let mut eager = workspace(root);
    let expected = definition(&mut eager, &main, Position::new(9, 3));
    super::TEST_EAGER_SNAPSHOT_IMPORTS.with(|eager| eager.set(false));
    let mut lazy = workspace(root);

    let locations = definition(&mut lazy, &main, Position::new(9, 3));

    assert_eq!(locations, expected);
    assert_eq!(
        indexed(&lazy, root, "Base.pas"),
        indexed(&eager, root, "Base.pas")
    );
}
