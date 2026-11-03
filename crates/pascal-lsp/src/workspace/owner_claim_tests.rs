//! D43 (l4d-z7ti): a unit below an unrelated ancestor `.dproj` keeps the
//! context of the selected or open project whose search path reaches it.

use super::indexing::TargetPurpose;
use super::{ContextKey, Workspace, WorkspaceOptions};
use crate::NavigationTarget;
use lsp_types::{Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const USERS: &str = "unit Users;\ninterface\ntype\n  TUser = class\n  end;\nimplementation\nend.\n";
const APP: &str = "program App;\nuses Users;\nvar U: TUser;\nbegin\nend.\n";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    ancestor: PathBuf,
    app_project: PathBuf,
    app: PathBuf,
    users: PathBuf,
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// `Common/AES.dproj` above `Common/Rules/Users.pas`, and
/// `Projects/App/App.dproj` whose unit search path reaches `Common/Rules`.
/// `ancestor_items` goes in the ancestor project's ItemGroup.
fn fixture(ancestor_items: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let ancestor = root.join("Common").join("AES.dproj");
    write(
        &ancestor,
        &format!(
            "<Project><PropertyGroup><MainSource>AES.dpr</MainSource></PropertyGroup><ItemGroup>{ancestor_items}</ItemGroup></Project>"
        ),
    );
    write(
        &root.join("Common").join("AES.dpr"),
        "program AES; begin end.",
    );
    let users = root.join("Common").join("Rules").join("Users.pas");
    write(&users, USERS);
    let app_project = root.join("Projects").join("App").join("App.dproj");
    write(
        &app_project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>..\\..\\Common\\Rules</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    let app = root.join("Projects").join("App").join("App.dpr");
    write(&app, APP);
    pascal_project::backdate_tree_for_test(&root);
    Fixture {
        _temp: temp,
        root,
        ancestor,
        app_project,
        app,
        users,
    }
}

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).unwrap()
}

fn workspace(fixture: &Fixture) -> Workspace {
    Workspace::with_override_session(
        vec![fixture.root.clone()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    )
}

fn open(workspace: &mut Workspace, path: &Path) -> Url {
    let uri = uri(path);
    workspace
        .open_document(uri.clone(), fs::read_to_string(path).unwrap(), 1)
        .unwrap();
    uri
}

fn select(workspace: &mut Workspace, document: &Path, project: &Path) {
    workspace
        .select_project(&uri(document), Some(&uri(project)))
        .unwrap();
}

fn project_of(workspace: &mut Workspace, path: &Path) -> Option<Url> {
    workspace
        .project_context(&uri(path))
        .unwrap()
        .selected_project_uri
}

fn key_of(workspace: &Workspace, path: &Path) -> ContextKey {
    workspace
        .document_contexts
        .get(&uri(path))
        .cloned()
        .expect("document context")
}

fn app_with_users_open(fixture: &Fixture, selected: bool) -> Workspace {
    let mut workspace = workspace(fixture);
    open(&mut workspace, &fixture.app);
    if selected {
        select(&mut workspace, &fixture.app, &fixture.app_project);
    }
    open(&mut workspace, &fixture.users);
    workspace
}

#[test]
fn without_a_claiming_project_the_unrelated_ancestor_does_not_own_the_unit() {
    let fixture = fixture("");
    let mut workspace = workspace(&fixture);
    open(&mut workspace, &fixture.users);

    assert_eq!(project_of(&mut workspace, &fixture.users), None);
}

#[test]
fn a_unit_on_the_selected_projects_search_path_keeps_its_context() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);

    assert_eq!(
        project_of(&mut workspace, &fixture.users),
        Some(uri(&fixture.app_project))
    );
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app),
        "the unit must share the selected project's context, not a duplicate"
    );
}

#[test]
fn a_unit_on_an_open_documents_project_search_path_keeps_its_context() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, false);

    assert_eq!(
        project_of(&mut workspace, &fixture.users),
        Some(uri(&fixture.app_project))
    );
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app)
    );
}

#[test]
fn a_dcc_reference_of_the_ancestor_project_keeps_the_ancestor_as_owner() {
    let fixture = fixture(r#"<DCCReference Include="Rules\Users.pas"/>"#);
    let mut workspace = app_with_users_open(&fixture, true);

    assert_eq!(
        project_of(&mut workspace, &fixture.users),
        Some(uri(&fixture.ancestor))
    );
}

#[test]
fn an_explicit_selection_of_the_ancestor_wins() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);
    select(&mut workspace, &fixture.users, &fixture.ancestor);

    assert_eq!(
        project_of(&mut workspace, &fixture.users),
        Some(uri(&fixture.ancestor))
    );
}

#[test]
fn a_navigation_target_opened_from_the_selected_project_keeps_its_context() {
    let fixture = fixture("");
    let mut workspace = workspace(&fixture);
    open(&mut workspace, &fixture.app);
    select(&mut workspace, &fixture.app, &fixture.app_project);
    let locations = workspace
        .navigate_with_cancel(
            &uri(&fixture.app),
            Position::new(2, 8),
            NavigationTarget::Definition,
            &AtomicBool::new(false),
        )
        .expect("definition");
    assert_eq!(
        locations
            .iter()
            .map(|location| location.uri.clone())
            .collect::<Vec<_>>(),
        vec![uri(&fixture.users)]
    );

    open(&mut workspace, &fixture.users);

    assert_eq!(
        project_of(&mut workspace, &fixture.users),
        Some(uri(&fixture.app_project))
    );
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app)
    );
}

#[test]
fn the_index_targets_the_selected_project_for_the_unit() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);
    let app_key = key_of(&workspace, &fixture.app);
    let input = workspace.analysis_input();

    let targets = workspace
        .index_targets(
            &input,
            Some(&uri(&fixture.users)),
            TargetPurpose::References,
            &AtomicBool::new(false),
        )
        .expect("targets");

    assert_eq!(targets.requesting.as_ref(), Some(&app_key));
    assert!(
        targets
            .contexts
            .iter()
            .all(|(key, _)| key.project_file.as_deref() != Some(fixture.ancestor.as_path())),
        "the unrelated ancestor must not be an index target: {:?}",
        targets.contexts
    );
    let membership = workspace
        .index_membership(&app_key, &AtomicBool::new(false))
        .expect("membership");
    assert!(membership.paths.contains(&fixture.users));
}

#[test]
fn selecting_the_project_after_opening_the_unit_moves_it_to_the_selected_context() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, false);
    select(&mut workspace, &fixture.app, &fixture.app_project);

    assert_eq!(
        project_of(&mut workspace, &fixture.users),
        Some(uri(&fixture.app_project))
    );
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app),
        "the unit must follow the selection, not keep the automatic context"
    );
    assert!(
        key_of(&workspace, &fixture.users)
            .selection_project
            .is_some()
    );
}

/// The server answers from a workspace rebuilt from its analysis input; the
/// unit's imports must resolve in the selected project there too, not in a
/// projectless context where `Base` is ambiguous.
#[test]
fn a_request_from_the_analysis_input_resolves_the_unit_in_the_selected_project() {
    let fixture = fixture("");
    let users_source = "unit Users;\ninterface\nuses Base;\ntype\n  TUser = class(TBase)\n  end;\nimplementation\nend.\n";
    write(&fixture.users, users_source);
    let base = fixture.root.join("Common").join("Base").join("Base.pas");
    let base_source =
        "unit Base;\ninterface\ntype\n  TBase = class\n  end;\nimplementation\nend.\n";
    write(&base, base_source);
    write(
        &fixture.root.join("Projects").join("Other").join("Base.pas"),
        base_source,
    );
    write(
        &fixture.app_project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>..\\..\\Common\\Rules;..\\..\\Common\\Base</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    pascal_project::backdate_tree_for_test(&fixture.root);
    let workspace = app_with_users_open(&fixture, true);

    let result = super::queries::navigation_from_input(
        workspace.analysis_input(),
        &uri(&fixture.users),
        Position::new(4, 18),
        NavigationTarget::Definition,
        &AtomicBool::new(false),
    )
    .value
    .expect("navigation");

    assert_eq!(
        result
            .locations
            .iter()
            .map(|location| location.uri.clone())
            .collect::<Vec<_>>(),
        vec![uri(&base)]
    );
}
