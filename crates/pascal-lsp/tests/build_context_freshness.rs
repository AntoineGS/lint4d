use lsp_types::{Position, Url};
use pascal_lsp::NavigationTarget;
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use pascal_project::CompilerVersion;
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::Path;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture");
}

fn fixture(root: &Path, search_path: &str) -> (Url, Url, String) {
    for directory in search_path.split(';') {
        fs::create_dir_all(root.join(directory)).expect("create unit search directory");
    }
    let project = root.join("App.dproj");
    let main = root.join("App.dpr");
    let unit = root.join("U.pas");
    write(
        &project,
        &format!(
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource>\
             <Config Condition=\"'$(Config)'==''\">Release</Config>\
             <Platform Condition=\"'$(Platform)'==''\">Win32</Platform>\
             <DCC_DCCCompiler>DCC32</DCC_DCCCompiler>\
             <DCC_UnitSearchPath>{search_path}</DCC_UnitSearchPath></PropertyGroup>\
             <ItemGroup><BuildConfiguration Include=\"Release\"/></ItemGroup></Project>"
        ),
    );
    write(
        &main,
        "program App; uses U in 'U.pas', H in 'H.pas'; begin end.",
    );
    write(
        &root.join("H.pas"),
        "unit H; interface procedure Help; implementation procedure Help; begin end; end.",
    );
    let source = "unit U;\ninterface\nuses H;\nimplementation\n{$IF Declared(RTLVersion113)}\nprocedure Run; begin Help; end;\n{$IFEND}\nend.\n";
    write(&unit, source);
    (
        Url::from_file_path(project).expect("project URI"),
        Url::from_file_path(unit).expect("unit URI"),
        source.to_owned(),
    )
}

fn workspace(root: &Path) -> Workspace {
    let mut options = WorkspaceOptions::default();
    options.conditional_context.compiler_version = Some(CompilerVersion::new(35, 0));
    Workspace::with_override_session(
        vec![root.to_path_buf()],
        options,
        OverrideSession::new(None),
    )
}

#[test]
fn notification_free_system_pas_creation_invalidates_cached_navigation() {
    let temp = tempfile::tempdir().expect("temporary project");
    let root = temp.path();
    let (project, unit, source) = fixture(root, "rtl");
    let mut workspace = workspace(root);
    let position = Position::new(
        5,
        source.lines().nth(5).unwrap().find("Help").unwrap() as u32,
    );

    assert_eq!(
        workspace
            .navigate(&unit, position, NavigationTarget::Definition)
            .len(),
        1,
        "table fallback should initially activate RTLVersion113"
    );
    write(
        &root.join("rtl/System.pas"),
        "unit System; interface implementation end.",
    );
    let updated = workspace
        .build_context(&project)
        .expect("fresh build context");
    assert_eq!(
        updated.conditionals.rtl_version_constants.source,
        "systemPas"
    );
    assert!(updated.conditionals.rtl_version_constants.names.is_empty());

    assert!(
        workspace
            .navigate(&unit, position, NavigationTarget::Definition)
            .is_empty(),
        "cached navigation must not retain the table-fallback branch"
    );
}

#[test]
fn notification_free_higher_priority_system_pas_shadow_invalidates_cached_navigation() {
    let temp = tempfile::tempdir().expect("temporary project");
    let root = temp.path();
    let (project, unit, source) = fixture(root, "high;low");
    write(
        &root.join("low/System.pas"),
        "unit System; interface const RTLVersion113 = True; implementation end.",
    );
    let mut workspace = workspace(root);
    let position = Position::new(
        5,
        source.lines().nth(5).unwrap().find("Help").unwrap() as u32,
    );

    assert_eq!(
        workspace
            .navigate(&unit, position, NavigationTarget::Definition)
            .len(),
        1,
        "the lower-priority System.pas should initially activate RTLVersion113"
    );
    write(
        &root.join("high/System.pas"),
        "unit System; interface implementation end.",
    );
    let updated = workspace
        .build_context(&project)
        .expect("fresh build context");
    assert_eq!(
        updated.conditionals.rtl_version_constants.source,
        "systemPas"
    );
    assert!(updated.conditionals.rtl_version_constants.names.is_empty());

    assert!(
        workspace
            .navigate(&unit, position, NavigationTarget::Definition)
            .is_empty(),
        "cached navigation must honor the newly appearing higher-priority System.pas"
    );
}
