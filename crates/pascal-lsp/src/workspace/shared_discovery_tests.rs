//! Request workspaces reuse the project contexts discovered for the
//! workspace they were built from, while the project files are unchanged.

use super::queries::navigation_from_input;
use super::{Workspace, WorkspaceOptions};
use crate::NavigationTarget;
use lsp_types::{Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

const PROJECT: &str = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>";
const MAIN: &str = "unit Main;\ninterface\nuses Dep;\nimplementation\nprocedure Run;\nbegin\n  DepValue;\nend;\nend.\n";

fn fixture(root: &Path) {
    fs::write(root.join("App.dproj"), PROJECT).expect("project");
    fs::write(root.join("Main.pas"), MAIN).expect("main");
    fs::write(
        root.join("Dep.pas"),
        "unit Dep;\ninterface\nprocedure DepValue;\nimplementation\nprocedure DepValue; begin end;\nend.\n",
    )
    .expect("dependency");
    settle(root);
}

/// Stamps written in the current clock tick are unsettled and cannot prove
/// a retained context unchanged; age the fixture as an editor session would.
fn settle(root: &Path) {
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for entry in fs::read_dir(root).expect("fixture directory") {
        let path = entry.expect("fixture entry").path();
        let file = if path.is_dir() {
            fs::File::open(&path)
        } else {
            fs::File::options().write(true).open(&path)
        };
        file.and_then(|file| file.set_modified(past))
            .expect("age fixture entry");
    }
    fs::File::open(root)
        .and_then(|directory| directory.set_modified(past))
        .expect("age fixture directory");
}

fn discoveries_during(run: impl FnOnce()) -> usize {
    pascal_project::test_reset_project_discovery_counts();
    run();
    pascal_project::test_project_discovery_counts().0
}

fn definition(workspace: &Workspace, main: &Url) -> usize {
    navigation_from_input(
        workspace.analysis_input(),
        main,
        Position::new(6, 4),
        NavigationTarget::Definition,
        &AtomicBool::new(false),
    )
    .value
    .expect("definition")
    .locations
    .len()
}

fn workspace(root: &Path) -> Workspace {
    Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    )
}

#[test]
fn a_second_request_reuses_the_discovered_project_contexts() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fixture(root);
    let workspace = workspace(root);
    let main = Url::from_file_path(root.join("Main.pas")).expect("main URI");

    let first = discoveries_during(|| assert_eq!(definition(&workspace, &main), 1));
    let second = discoveries_during(|| assert_eq!(definition(&workspace, &main), 1));

    assert!(first > 0, "the first request discovers the project");
    assert_eq!(second, 0, "the second request must reuse the discovery");
}

#[test]
fn an_edited_project_file_is_rediscovered() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fixture(root);
    let workspace = workspace(root);
    let main = Url::from_file_path(root.join("Main.pas")).expect("main URI");
    assert_eq!(definition(&workspace, &main), 1);

    fs::write(
        root.join("App.dproj"),
        PROJECT.replace("FEATURE", "OTHER_FEATURE_WITH_ANOTHER_SIZE"),
    )
    .expect("edited project");
    let after_edit = discoveries_during(|| assert_eq!(definition(&workspace, &main), 1));

    assert!(
        after_edit > 0,
        "a changed project file must be rediscovered"
    );
}

#[test]
fn a_project_warning_is_logged_once_per_configuration_generation() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fixture(root);
    let workspace = workspace(root);
    let generation = workspace.configuration_generation;
    assert!(
        workspace
            .shared_discoveries
            .first_report(generation, "ignored Target body")
    );
    assert!(
        !workspace
            .analysis_input()
            .shared_discoveries
            .first_report(generation, "ignored Target body"),
        "a request workspace shares what was logged"
    );
    assert!(
        workspace
            .shared_discoveries
            .first_report(generation + 1, "ignored Target body"),
        "a new configuration generation logs again"
    );
}

#[test]
fn jobs_of_an_older_generation_do_not_log_a_warning_again() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let shared = workspace(temp.path()).shared_discoveries.clone();
    assert!(shared.first_report(1, "ignored Target body"));
    assert!(shared.first_report(2, "ignored Target body"));
    assert!(
        !shared.first_report(1, "ignored Target body"),
        "a job still running under the older generation must not log again"
    );
}

#[test]
fn a_project_warning_is_logged_once_across_requests() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fixture(root);
    fs::write(
        root.join("App.dproj"),
        PROJECT.replace(
            "</Project>",
            "<Target Name=\"Custom\"><Exec Command=\"x\"/></Target></Project>",
        ),
    )
    .expect("project with a target");
    settle(root);
    let workspace = workspace(root);
    let main = Url::from_file_path(root.join("Main.pas")).expect("main URI");
    super::TEST_LOGGED_WARNINGS.with(|logged| logged.borrow_mut().clear());

    assert_eq!(definition(&workspace, &main), 1);
    assert_eq!(definition(&workspace, &main), 1);

    let logged = super::TEST_LOGGED_WARNINGS.with(|logged| logged.borrow().clone());
    let targets = logged
        .iter()
        .filter(|warning| warning.contains("ignored Target body"))
        .count();
    assert_eq!(targets, 1, "{logged:#?}");
}

/// A request that already installed the project context for one document
/// and adopts the shared discovery of another records that document's
/// directories, so a new project candidate there stales the context.
#[test]
fn adopting_a_shared_discovery_into_an_installed_context_records_its_directories() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fixture(root);
    // The project names `b` as a unit directory, so it owns `b/Other.pas`.
    fs::write(
        root.join("App.dproj"),
        PROJECT.replace(
            "</PropertyGroup>",
            "<DCC_UnitSearchPath>b</DCC_UnitSearchPath></PropertyGroup>",
        ),
    )
    .expect("project with a unit directory");
    fs::create_dir_all(root.join("b")).expect("second source directory");
    fs::write(
        root.join("b/Other.pas"),
        "unit Other;\ninterface\nimplementation\nend.\n",
    )
    .expect("second source");
    settle(&root.join("b"));
    settle(root);
    let main_workspace = workspace(root);
    let main = Url::from_file_path(root.join("Main.pas")).expect("main URI");
    let other = Url::from_file_path(root.join("b/Other.pas")).expect("other URI");
    {
        let mut first = Workspace::from_analysis_input(&main_workspace.analysis_input());
        first.context_for_uri(&other).expect("context of Other");
    }

    let mut second = Workspace::from_analysis_input(&main_workspace.analysis_input());
    let main_key = second.context_for_uri(&main).expect("context of Main");
    let other_key = second.context_for_uri(&other).expect("context of Other");
    assert_eq!(main_key, other_key, "one project owns both documents");

    let memberships = &second.contexts[&other_key].project_candidate_memberships;
    assert!(
        memberships.keys().any(|directory| directory.ends_with("b")),
        "{:?}",
        memberships.keys().collect::<Vec<_>>()
    );
}
