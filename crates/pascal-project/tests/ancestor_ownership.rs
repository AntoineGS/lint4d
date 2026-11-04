//! D43 (l4d-z7ti): a `.dproj` in an ancestor directory of a unit owns it only
//! when the unit is one of that project's own units.

use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{ProjectContext, ProjectOptions, ProjectSelections, discover_with_selections};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    fs::write(path, contents).expect("fixture file");
}

fn unit(path: &Path) {
    let name = path.file_stem().unwrap().to_string_lossy().into_owned();
    write(path, &format!("unit {name}; interface implementation end."));
}

/// `root/a.dproj` (+ `a.dpr`) with `property_group` in its PropertyGroup.
fn ancestor_project(root: &Path, item_group: &str, property_group: &str) -> PathBuf {
    let project = root.join("a.dproj");
    write(
        &project,
        &format!(
            "<Project><PropertyGroup><MainSource>a.dpr</MainSource>{property_group}</PropertyGroup><ItemGroup>{item_group}</ItemGroup></Project>"
        ),
    );
    write(&root.join("a.dpr"), "program a; begin end.");
    project
}

fn discover(file: &Path, root: &Path, options: &ProjectOptions) -> Arc<ProjectContext> {
    ProjectContext::discover_with_overrides(
        file,
        &[root.to_path_buf()],
        options,
        &OverrideSession::new(None),
    )
    .expect("discovery")
    .context
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("canonical path")
}

#[test]
fn an_ancestor_project_does_not_own_an_unrelated_unit_below_it() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    ancestor_project(&root, "", "");
    let source = root.join("rules").join("Unrelated.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), None);
    assert!(context.discovery_complete());
}

#[test]
fn an_ancestor_project_owns_its_dcc_reference() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, r#"<DCCReference Include="rules\Member.pas"/>"#, "");
    let source = root.join("rules").join("Member.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn an_ancestor_project_owns_a_unit_of_its_explicit_in_path() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, "", "");
    write(
        &root.join("a.dpr"),
        "program a; uses Member in 'rules\\Member.pas'; begin end.",
    );
    let source = root.join("rules").join("Member.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn an_ancestor_project_owns_a_unit_of_its_unit_search_path() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, "", "<DCC_UnitSearchPath>rules</DCC_UnitSearchPath>");
    let source = root.join("rules").join("Searched.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn caller_source_paths_do_not_make_an_ancestor_project_own_a_unit() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    ancestor_project(&root, "", "");
    let source = root.join("rules").join("Library.pas");
    unit(&source);
    let options = ProjectOptions {
        source_paths: vec![root.join("rules").to_string_lossy().into_owned()],
        ..ProjectOptions::default()
    };

    let context = discover(&source, &root, &options);

    assert_eq!(context.project_file(), None);
}

#[test]
fn a_project_in_the_units_own_directory_still_owns_it() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, "", "");
    let source = root.join("Local.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn a_further_ancestor_that_owns_the_unit_is_chosen_over_a_nearer_unrelated_one() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(
        &root,
        "",
        "<DCC_UnitSearchPath>common\\rules</DCC_UnitSearchPath>",
    );
    write(
        &root.join("common").join("Codec.dproj"),
        "<Project><PropertyGroup><MainSource>Codec.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        &root.join("common").join("Codec.dpr"),
        "program Codec; begin end.",
    );
    let source = root.join("common").join("rules").join("Users.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn the_project_file_option_wins_over_membership() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, "", "");
    let source = root.join("rules").join("Unrelated.pas");
    unit(&source);
    let options = ProjectOptions {
        project_file: Some(project.clone()),
        ..ProjectOptions::default()
    };

    let context = discover(&source, &root, &options);

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn a_runtime_selection_wins_over_membership() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, "", "");
    let source = root.join("rules").join("Unrelated.pas");
    unit(&source);
    let mut selections = ProjectSelections::new();
    selections.insert(root.clone(), project.clone());

    let context = discover_with_selections(
        &source,
        std::slice::from_ref(&root),
        &ProjectOptions::default(),
        &selections,
        &OverrideSession::new(None),
        &[],
    )
    .expect("discovery")
    .context;

    assert_eq!(context.project_file(), Some(project.as_path()));
}

#[test]
fn project_membership_names_own_units_but_resolves_search_path_units() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(
        &root,
        r#"<DCCReference Include="refs\Member.pas"/>"#,
        "<DCC_UnitSearchPath>rules</DCC_UnitSearchPath>",
    );
    let member = root.join("refs").join("Member.pas");
    let searched = root.join("rules").join("Searched.pas");
    let library = root.join("library").join("Library.pas");
    let unrelated = root.join("other").join("Other.pas");
    for path in [&member, &searched, &library, &unrelated] {
        unit(path);
    }
    let options = ProjectOptions {
        project_file: Some(project),
        source_paths: vec![root.join("library").to_string_lossy().into_owned()],
        ..ProjectOptions::default()
    };
    let context = discover(&member, &root, &options);

    assert!(context.owns_unit_source(&member));
    assert!(context.owns_unit_source(&searched));
    assert!(context.owns_unit_source(&root.join("a.dpr")));
    assert!(!context.owns_unit_source(&library));
    assert!(!context.owns_unit_source(&unrelated));
    assert!(context.resolves_unit_source(&library));
    assert!(context.resolves_unit_source(&searched));
    assert!(!context.resolves_unit_source(&unrelated));
}

/// C1: an ancestor whose search path cannot be evaluated (no installation
/// defines `$(BDS)`) is not proof that it does not name the unit; it stays
/// selected, incomplete, as before D43.
#[test]
fn an_incomplete_ancestor_context_keeps_the_ancestor() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(
        &root,
        "",
        "<DCC_UnitSearchPath>$(BDS)\\rules</DCC_UnitSearchPath>",
    );
    let source = root.join("rules").join("Unrelated.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
    assert!(!context.discovery_complete());
}

/// C1: `$(Undefined)\rules` expands to `\rules` (MSBuild's empty default),
/// a path the project's author did not mean; membership through such an
/// entry is unknown, so the ancestor stays selected.
#[test]
fn a_search_path_entry_with_an_undefined_property_keeps_the_ancestor() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(
        &root,
        "",
        "<DCC_UnitSearchPath>$(Undefined)\\rules</DCC_UnitSearchPath>",
    );
    let source = root.join("rules").join("Unrelated.pas");
    unit(&source);

    let context = discover(&source, &root, &ProjectOptions::default());

    assert_eq!(context.project_file(), Some(project.as_path()));
}

/// An undefined property in one entry leaves the other entries' verdicts
/// alone: a unit no clean entry names is still unknown (the ancestor
/// stays), and a unit a clean entry names is owned.
#[test]
fn an_undefined_property_taints_only_its_own_search_path_entry() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(
        &root,
        "",
        "<DCC_UnitSearchPath>$(Undefined)\\elsewhere;rules</DCC_UnitSearchPath>",
    );
    let source = root.join("rules").join("Searched.pas");
    unit(&source);
    let context = discover(&source, &root, &ProjectOptions::default());
    assert_eq!(context.project_file(), Some(project.as_path()));
    assert!(context.owns_unit_source(&source));
    let elsewhere = root.join("elsewhere").join("Other.pas");
    unit(&elsewhere);
    assert!(!context.owns_unit_source(&elsewhere));
    assert!(context.unit_directories_unknown());
}

/// I5: the membership probe's build is the selected context's build.
#[test]
fn a_member_unit_builds_its_ancestor_project_once() {
    let temp = tempdir().unwrap();
    let root = canonical(temp.path());
    let project = ancestor_project(&root, r#"<DCCReference Include="rules\Member.pas"/>"#, "");
    let source = root.join("rules").join("Member.pas");
    unit(&source);

    pascal_project::test_reset_project_discovery_counts();
    let context = discover(&source, &root, &ProjectOptions::default());
    let (_, builds) = pascal_project::test_project_discovery_counts();

    assert_eq!(context.project_file(), Some(project.as_path()));
    assert_eq!(builds, 1, "the probe's build must be reused");
}
