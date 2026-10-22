use pascal_cache::{
    ArtifactRef, ContextKey, Digest, ExpandedArtifactKey, IncludeOccurrence, IncludeOutcome,
    LocalArtifactKey, MANIFEST_DIGEST_CONTEXT, MemberInputs, MemberKey, Namespace,
    SOURCE_DIGEST_CONTEXT, SourceContent, SourceIdentity,
};
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{
    ConditionalContext, ConditionalFact, ProjectContext, ProjectContextParts, ProjectOptions,
    ProjectPathEntry, SourceOrigin,
};
use std::fs;
use std::path::Path;
use std::process::Command;

const CHILD: &str = "PASCAL_CACHE_VECTOR_CHILD";
const FIXTURE: &str = "PASCAL_CACHE_VECTOR_FIXTURE";

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

fn write_fixture(root: &Path) {
    for (name, contents) in [
        ("App.dproj", format!("\u{feff}{WEBQUERY_DPROJ}")),
        ("App.dpr", MAIN.to_string()),
        ("SvcMain.pas", UNIT.to_string()),
    ] {
        fs::write(root.join(name), contents).unwrap();
    }
}

fn discovered_context(root: &Path) -> ContextKey {
    let options = ProjectOptions {
        build_config: Some("Debug".into()),
        ..ProjectOptions::default()
    };
    let discovered = ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &options,
        &OverrideSession::new(Some(root.join("config.toml"))),
    )
    .expect("discovery");
    ContextKey::new(&discovered.context)
}

fn vectors(fixture: &Path) -> Vec<(String, String)> {
    let mut parts = ProjectContextParts {
        project_file: Some("/w/App.dproj".into()),
        search_path_entries: vec![
            ProjectPathEntry::legacy("/w".into()),
            ProjectPathEntry::configured("/w/src".into()),
        ],
        ..ProjectContextParts::default()
    };
    for index in 0..64 {
        parts.explicit_unit_entries.insert(
            format!("unit{index}"),
            vec![ProjectPathEntry::configured(
                format!("/w/Unit{index}.pas").into(),
            )],
        );
        parts
            .unit_aliases
            .insert(format!("Alias{index}"), format!("Unit{index}"));
        parts
            .overrides
            .properties
            .insert(format!("P{index}"), format!("{index}"));
    }
    parts
        .conditional_context
        .set_define("DEBUG", ConditionalFact::True);
    parts
        .conditional_context
        .set_define("RELEASE", ConditionalFact::False);
    let context = ContextKey::new(&ProjectContext::from_parts(parts));

    let content = SourceContent::from_bytes(b"unit Vector; interface implementation end.");
    let mut conditional = ConditionalContext::default();
    conditional.set_define("DEBUG", ConditionalFact::True);
    let local = LocalArtifactKey::new(&content, &conditional, SourceOrigin::ProjectCompiled);
    let expanded = ExpandedArtifactKey::new(
        &local,
        &[IncludeOccurrence {
            parent: None,
            directive_range: 30..45,
            directive: "Vector.inc".into(),
            inherited: conditional.clone(),
            outcome: IncludeOutcome::Resolved {
                source: SourceIdentity {
                    canonical: "/w/Vector.inc".into(),
                    logical: "/w/Vector.inc".into(),
                    spelling: Some("Vector.inc".into()),
                    uri: None,
                },
                entry: Some(ProjectPathEntry::configured("/w".into())),
                content: SourceContent::from_bytes(b"const V = 1;"),
            },
        }],
    )
    .unwrap();
    let source = SourceIdentity {
        canonical: "/w/Vector.pas".into(),
        logical: "/w/Vector.pas".into(),
        spelling: None,
        uri: Some("file:///w/Vector.pas".into()),
    };
    let member = MemberKey::new(&MemberInputs {
        context: &context,
        source: &source,
        entry: Some(&ProjectPathEntry::configured("/w".into())),
        origin: SourceOrigin::ProjectCompiled,
        fallback_unit_name: Some("Vector"),
        artifact: Some(ArtifactRef::Expanded(&expanded)),
    });
    [
        (
            "namespace",
            Namespace::new(Digest::from_bytes([7; 32])).id(),
        ),
        ("source", content.digest()),
        ("context", context.digest()),
        ("discovered", discovered_context(fixture).digest()),
        ("local", local.digest()),
        ("expanded", expanded.digest()),
        ("member", member.digest()),
    ]
    .into_iter()
    .map(|(name, digest)| (name.to_string(), digest.to_hex()))
    .collect()
}

#[test]
#[ignore = "run by vectors_are_identical_in_a_separate_process"]
fn print_vectors_for_child_process() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let fixture = std::env::var_os(FIXTURE).expect("fixture root");
    for (name, hex) in vectors(Path::new(&fixture)) {
        println!("vector {name} {hex}");
    }
}

#[test]
fn vectors_are_identical_in_a_separate_process() {
    let fixture = tempfile::tempdir().unwrap();
    write_fixture(fixture.path());
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "print_vectors_for_child_process",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env(FIXTURE, fixture.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let child = stdout
        .lines()
        // The harness prints "test <name> ... " before the first captured line.
        .filter_map(|line| line.split_once("vector ").map(|(_, vector)| vector))
        .map(|line| {
            let (name, hex) = line.split_once(' ').unwrap();
            (name.to_string(), hex.to_string())
        })
        .collect::<Vec<_>>();
    assert_eq!(child.len(), 7);
    assert_eq!(child, vectors(fixture.path()));
}

const DEFAULT_CONDITIONAL_CONTEXT: &[u8] = &[
    1, 0, 0, 2, 0, 1, 0, 0, 2, 0, 1, 0, 0, 2, 0, 0, 3, 0, 3, 4, 0, 3, 5, 0, 3, 3, 0, 0, 4, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 0, 3, 8, 0, 0,
];

#[test]
fn local_key_manifest_has_its_canonical_bytes() {
    let key = LocalArtifactKey::new(
        &SourceContent::from_bytes(b""),
        &ConditionalContext::default(),
        SourceOrigin::Library,
    );
    let mut expected = vec![b'L', b'4', b'D', b'C', 1, 0, 0, 0, 2]; // header, LocalArtifact
    expected.extend([1, 0]); // content
    expected.extend([1, 0]); //   digest
    expected.extend(blake3::derive_key(SOURCE_DIGEST_CONTEXT, b""));
    expected.extend([2, 0, 0, 0, 0, 0, 0, 0, 0, 0]); //   length 0
    expected.extend([3, 0, 2]); //   encoding Utf8
    expected.extend([2, 0]); // conditional
    expected.extend_from_slice(DEFAULT_CONDITIONAL_CONTEXT);
    expected.extend([3, 0, 2]); // origin Library
    assert_eq!(key.key().manifest(), expected);
    assert_eq!(
        *key.digest().as_bytes(),
        blake3::derive_key(MANIFEST_DIGEST_CONTEXT, &expected)
    );
}
