use pascal_cache::{
    ArtifactRef, ContextKey, LocalArtifactKey, MemberInputs, MemberKey, SourceContent,
    SourceIdentity,
};
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{ProjectContext, ProjectOptions, SourceOrigin};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const WEBQUERY_DPROJ: &str = r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
	<PropertyGroup>
		<ProjectVersion>12.0</ProjectVersion>
		<MainSource>App.dpr</MainSource>
		<Config Condition="'$(Config)'==''">Release</Config>
		<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>
	</PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''"><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_1)'!=''"><Cfg_1>true</Cfg_1><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_2)'!=''"><Cfg_2>true</Cfg_2><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Base)'!=''"><DCC_Define>NOSF;$(DCC_Define)</DCC_Define><DCC_Platform>x86</DCC_Platform></PropertyGroup>
	<PropertyGroup Condition="'$(Cfg_1)'!=''"><DCC_Define>RELEASE;$(DCC_Define)</DCC_Define></PropertyGroup>
	<PropertyGroup Condition="'$(Cfg_2)'!=''"><DCC_Define>DEBUG;$(DCC_Define)</DCC_Define></PropertyGroup>
	<ItemGroup>
		<DelphiCompile Include="App.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
		<DCCReference Include="SvcMain.pas"/>
		<BuildConfiguration Include="Base"><Key>Base</Key></BuildConfiguration>
		<BuildConfiguration Include="Debug"><Key>Cfg_2</Key><CfgParent>Base</CfgParent></BuildConfiguration>
		<BuildConfiguration Include="Release"><Key>Cfg_1</Key><CfgParent>Base</CfgParent></BuildConfiguration>
	</ItemGroup>
</Project>"#;

const MAIN: &str = "program App; uses SvcMain in 'SvcMain.pas'; begin end.";
const UNIT: &str = "unit SvcMain; interface implementation end.";
const LIBRARY: &[u8] = b"unit Lib; interface implementation end.";

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// `root/A` and `root/B` are identical projects; `root/lib` is outside both.
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for project in ["A", "B"] {
        let directory = root.path().join(project);
        write(
            &directory.join("App.dproj"),
            &format!("\u{feff}{WEBQUERY_DPROJ}"),
        );
        write(&directory.join("App.dpr"), MAIN);
        write(&directory.join("SvcMain.pas"), UNIT);
    }
    root
}

fn discover(root: &Path, project: &str, config: &str) -> Arc<ProjectContext> {
    let options = ProjectOptions {
        build_config: Some(config.into()),
        ..ProjectOptions::default()
    };
    ProjectContext::discover_with_overrides(
        &root.join(project).join("App.dpr"),
        &[root.to_path_buf()],
        &options,
        &OverrideSession::new(Some(root.join("config.toml"))),
    )
    .expect("discovery")
    .context
}

#[test]
fn configurations_and_projects_have_distinct_context_keys() {
    let root = fixture();
    let a_debug = discover(root.path(), "A", "Debug");
    let a_release = discover(root.path(), "A", "Release");
    let b_debug = discover(root.path(), "B", "Debug");
    assert_eq!(a_debug.config(), Some("Debug"));
    assert_eq!(a_release.config(), Some("Release"));

    let keys = [
        ContextKey::new(&a_debug),
        ContextKey::new(&a_release),
        ContextKey::new(&b_debug),
    ];
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[0], keys[2]);
    assert_ne!(keys[1], keys[2]);

    // A -> B -> A: switching back reproduces the first key exactly.
    assert_eq!(
        ContextKey::new(&discover(root.path(), "A", "Debug")),
        keys[0]
    );
}

#[test]
fn identical_local_facts_are_shared_across_projects_but_not_configurations() {
    let root = fixture();
    let a_debug = discover(root.path(), "A", "Debug");
    let a_release = discover(root.path(), "A", "Release");
    let b_debug = discover(root.path(), "B", "Debug");
    let unit = SourceContent::from_bytes(UNIT.as_bytes());
    let a_unit = root.path().join("A/SvcMain.pas");
    let b_unit = root.path().join("B/SvcMain.pas");
    assert_eq!(
        a_debug.source_origin(&a_unit),
        SourceOrigin::ProjectCompiled
    );

    let a_debug_key = LocalArtifactKey::for_source(&a_debug, &a_unit, &unit);
    assert_eq!(
        a_debug_key,
        LocalArtifactKey::for_source(&b_debug, &b_unit, &unit)
    );
    assert_ne!(
        a_debug_key,
        LocalArtifactKey::for_source(&a_release, &a_unit, &unit)
    );
}

#[test]
fn library_sources_share_across_projects_and_differ_from_project_sources() {
    let root = fixture();
    let a_debug = discover(root.path(), "A", "Debug");
    let b_debug = discover(root.path(), "B", "Debug");
    let library_path = root.path().join("lib/Lib.pas");
    let project_path = root.path().join("A/Lib.pas");
    assert_eq!(a_debug.source_origin(&library_path), SourceOrigin::Library);
    assert_eq!(
        a_debug.source_origin(&project_path),
        SourceOrigin::ProjectCompiled
    );

    let content = SourceContent::from_bytes(LIBRARY);
    let library = LocalArtifactKey::for_source(&a_debug, &library_path, &content);
    assert_eq!(
        library,
        LocalArtifactKey::for_source(&b_debug, &library_path, &content)
    );
    assert_ne!(
        library,
        LocalArtifactKey::for_source(&a_debug, &project_path, &content)
    );
}

#[test]
fn members_keep_shared_artifacts_apart_per_context() {
    let root = fixture();
    let a_debug = discover(root.path(), "A", "Debug");
    let b_debug = discover(root.path(), "B", "Debug");
    let unit = SourceContent::from_bytes(UNIT.as_bytes());
    let member = |context: &ProjectContext, project: &str| {
        let path: PathBuf = root.path().join(project).join("SvcMain.pas");
        let artifact = LocalArtifactKey::for_source(context, &path, &unit);
        let source = SourceIdentity {
            canonical: path.clone(),
            logical: path.clone(),
            spelling: Some("SvcMain.pas".into()),
            uri: None,
        };
        let context_key = ContextKey::new(context);
        let key = MemberKey::new(&MemberInputs {
            context: &context_key,
            source: &source,
            entry: None,
            origin: context.source_origin(&path),
            fallback_unit_name: None,
            artifact: Some(ArtifactRef::Local(&artifact)),
        });
        (artifact, key)
    };
    let (a_artifact, a_member) = member(&a_debug, "A");
    let (b_artifact, b_member) = member(&b_debug, "B");
    assert_eq!(a_artifact, b_artifact);
    assert_ne!(a_member, b_member);
}
