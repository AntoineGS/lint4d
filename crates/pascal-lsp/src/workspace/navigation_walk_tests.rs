//! Navigation over a dependency closure answers best effort (D16) and stops
//! a walk for a name that binds nowhere.

use super::{
    MAX_NAVIGATION_WALK_WORK, TEST_FAIL_IMPORTS_OF, TEST_NAVIGATION_WALK_BYTES, Workspace,
    WorkspaceOptions,
};
use crate::NavigationTarget;
use lsp_types::{Location, Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn workspace(root: &Path) -> Workspace {
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
    let _fail = FailImports::of("/Bad.pas");

    let locations = definition(&mut workspace, &root.join("Main.pas"), Position::new(7, 5));

    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0].uri, uri(&root.join("Root.pas")));
    assert!(
        workspace.warnings.iter().any(|warning| warning
            .contains("navigation skipped the imports of")
            && warning.contains("Bad.pas")),
        "the skipped unit must be reported: {:?}",
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
