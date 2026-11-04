//! D43 (l4d-z7ti): a unit below an unrelated ancestor `.dproj` keeps the
//! context of the selected or open project whose search path reaches it.
//!
//! Every ownership check asks both the main workspace and a request
//! workspace rebuilt from its analysis input (the server answers from the
//! latter), and requires them to agree. `Users` imports `Base`, which only
//! the App projects resolve; projectless, `Base` is ambiguous (a decoy
//! exists), and the ancestor `AES` project does not reach it, so a
//! definition on `TBase` lands on `Common/Base/Base.pas` exactly when the
//! unit is analysed in an App context.

use super::indexing::TargetPurpose;
use super::{ContextKey, FileChange, Workspace, WorkspaceOptions};
use crate::NavigationTarget;
use lsp_types::{Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const USERS: &str = "unit Users;\ninterface\nuses Base;\ntype\n  TUser = class(TBase)\n  end;\nimplementation\nend.\n";
const BASE: &str = "unit Base;\ninterface\ntype\n  TBase = class\n  end;\nimplementation\nend.\n";
const APP_PROJECT: &str = "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource><DCC_UnitSearchPath>..\\..\\Common\\Rules;..\\..\\Common\\Base</DCC_UnitSearchPath></PropertyGroup></Project>";
/// `TBase` in `TUser = class(TBase)`.
const TBASE: Position = Position {
    line: 4,
    character: 18,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    ancestor: PathBuf,
    app_project: PathBuf,
    app: PathBuf,
    users: PathBuf,
    base: PathBuf,
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn ancestor_project(items: &str) -> String {
    format!(
        "<Project><PropertyGroup><MainSource>AES.dpr</MainSource></PropertyGroup><ItemGroup>{items}</ItemGroup></Project>"
    )
}

/// Write `Projects/{name}/{name}.dproj` (search path reaching
/// `Common/Rules` and `Common/Base`) and its program; returns both paths.
fn app_project(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let project = root
        .join("Projects")
        .join(name)
        .join(format!("{name}.dproj"));
    write(&project, &APP_PROJECT.replace("{name}", name));
    let program = root.join("Projects").join(name).join(format!("{name}.dpr"));
    write(
        &program,
        &format!("program {name};\nuses Users;\nvar U: TUser;\nbegin\nend.\n"),
    );
    (project, program)
}

/// `Common/AES.dproj` above `Common/Rules/Users.pas`, and
/// `Projects/App/App.dproj` whose unit search path reaches `Common/Rules`.
/// `ancestor_items` goes in the ancestor project's ItemGroup.
fn fixture(ancestor_items: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let ancestor = root.join("Common").join("AES.dproj");
    write(&ancestor, &ancestor_project(ancestor_items));
    write(
        &root.join("Common").join("AES.dpr"),
        "program AES; begin end.",
    );
    let users = root.join("Common").join("Rules").join("Users.pas");
    write(&users, USERS);
    let base = root.join("Common").join("Base").join("Base.pas");
    write(&base, BASE);
    write(&root.join("Projects").join("Other").join("Base.pas"), BASE);
    let (app_project, app) = app_project(&root, "App");
    pascal_project::backdate_tree_for_test(&root);
    Fixture {
        _temp: temp,
        root,
        ancestor,
        app_project,
        app,
        users,
        base,
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

fn definition_targets(locations: Vec<lsp_types::Location>) -> Vec<Url> {
    locations.into_iter().map(|location| location.uri).collect()
}

/// The definition of `TBase` in `Users`, from the main workspace and from a
/// request workspace built from its analysis input.
fn definitions(workspace: &mut Workspace, users: &Path) -> (Vec<Url>, Vec<Url>) {
    let request = super::queries::navigation_from_input(
        workspace.analysis_input(),
        &uri(users),
        TBASE,
        NavigationTarget::Definition,
        &AtomicBool::new(false),
    )
    .value
    .expect("request navigation")
    .locations;
    let main = workspace
        .navigate_with_cancel(
            &uri(users),
            TBASE,
            NavigationTarget::Definition,
            &AtomicBool::new(false),
        )
        .unwrap_or_default();
    (definition_targets(main), definition_targets(request))
}

/// The owning project of `Users` agrees between the main workspace's
/// projectContext and both workspaces' navigation.
fn assert_owner(workspace: &mut Workspace, fixture: &Fixture, project: Option<&Path>) {
    assert_eq!(
        project_of(workspace, &fixture.users),
        project.map(uri),
        "main workspace projectContext"
    );
    let expected = if project.is_some_and(|project| project != fixture.ancestor) {
        vec![uri(&fixture.base)]
    } else {
        Vec::new()
    };
    let (main, request) = definitions(workspace, &fixture.users);
    assert_eq!(main, expected, "main workspace navigation");
    assert_eq!(request, expected, "request workspace navigation");
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

    assert_owner(&mut workspace, &fixture, None);
}

#[test]
fn a_unit_on_the_selected_projects_search_path_keeps_its_context() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
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

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app)
    );
}

#[test]
fn a_dcc_reference_of_the_ancestor_project_keeps_the_ancestor_as_owner() {
    let fixture = fixture(r#"<DCCReference Include="Rules\Users.pas"/>"#);
    let mut workspace = app_with_users_open(&fixture, true);

    assert_owner(&mut workspace, &fixture, Some(&fixture.ancestor));
}

#[test]
fn an_explicit_selection_of_the_ancestor_wins() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);
    select(&mut workspace, &fixture.users, &fixture.ancestor);

    assert_owner(&mut workspace, &fixture, Some(&fixture.ancestor));
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
            Position::new(1, 6),
            NavigationTarget::Definition,
            &AtomicBool::new(false),
        )
        .expect("definition");
    assert_eq!(definition_targets(locations), vec![uri(&fixture.users)]);

    open(&mut workspace, &fixture.users);

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app)
    );
}

#[test]
fn selecting_the_project_after_opening_the_unit_moves_it_to_the_selected_context() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, false);
    select(&mut workspace, &fixture.app, &fixture.app_project);

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
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

#[test]
fn a_unit_opened_before_its_project_is_claimed_once_the_project_opens() {
    let fixture = fixture("");
    let mut workspace = workspace(&fixture);
    open(&mut workspace, &fixture.users);
    assert_owner(&mut workspace, &fixture, None);

    open(&mut workspace, &fixture.app);

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app)
    );
}

#[test]
fn a_unit_opened_before_the_selection_is_owned_by_the_selected_project() {
    let fixture = fixture("");
    let mut workspace = workspace(&fixture);
    open(&mut workspace, &fixture.users);
    open(&mut workspace, &fixture.app);
    select(&mut workspace, &fixture.app, &fixture.app_project);

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
    assert!(
        key_of(&workspace, &fixture.users)
            .selection_project
            .is_some()
    );
}

#[test]
fn closing_the_claimant_makes_the_unit_projectless_again() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, false);
    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));

    assert!(workspace.close_document(&uri(&fixture.app)));

    assert_owner(&mut workspace, &fixture, None);
}

#[test]
fn closing_the_winning_claimant_hands_the_unit_to_the_next_one() {
    let fixture = fixture("");
    let (second_project, second) = app_project(&fixture.root, "Second");
    pascal_project::backdate_tree_for_test(&fixture.root);
    let mut workspace = workspace(&fixture);
    open(&mut workspace, &fixture.app);
    open(&mut workspace, &second);
    open(&mut workspace, &fixture.users);
    let app_key = key_of(&workspace, &fixture.app);
    let second_key = key_of(&workspace, &second);
    let (winner, winner_program, loser_project) =
        if super::context_key_digest(&app_key) <= super::context_key_digest(&second_key) {
            (app_key, fixture.app.clone(), second_project.clone())
        } else {
            (second_key, second.clone(), fixture.app_project.clone())
        };
    assert_eq!(
        key_of(&workspace, &fixture.users),
        winner,
        "open claimants are ordered by key digest"
    );

    assert!(workspace.close_document(&uri(&winner_program)));

    assert_owner(&mut workspace, &fixture, Some(&loser_project));
}

#[test]
fn deselecting_the_project_keeps_the_unit_with_the_open_project() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);
    workspace.select_project(&uri(&fixture.app), None).unwrap();

    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));
    assert_eq!(
        key_of(&workspace, &fixture.users),
        key_of(&workspace, &fixture.app)
    );
    assert!(
        key_of(&workspace, &fixture.users)
            .selection_project
            .is_none()
    );
}

#[test]
fn naming_the_unit_in_the_skipped_ancestor_moves_it_to_the_ancestor() {
    let fixture = fixture("");
    let mut workspace = app_with_users_open(&fixture, true);
    assert_owner(&mut workspace, &fixture, Some(&fixture.app_project));

    write(
        &fixture.ancestor,
        &ancestor_project(r#"<DCCReference Include="Rules\Users.pas"/>"#),
    );
    workspace.file_event(&uri(&fixture.ancestor), FileChange::Changed);

    assert_owner(&mut workspace, &fixture, Some(&fixture.ancestor));
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
