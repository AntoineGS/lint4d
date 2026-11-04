//! D45: `IFOPT` on a switch the project leaves unset answers with dcc's
//! default, in project code and in library code alike.

use std::{fs, path::Path};

use pascal_core::conditional::{self, ConditionalContext, Truth};
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{ProjectContext, ProjectOptions, SourceOrigin};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn activity(source: &str, marker: &str, context: &ConditionalContext) -> Truth {
    conditional::analyze_with_context(source, context)
        .directives
        .iter()
        .find(|directive| directive.body.contains(marker))
        .expect("marker directive")
        .activity
}

fn discover(range_checking: &str) -> (tempfile::TempDir, ProjectContext) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(&root.join("project/App.dpr"), "program App; begin end.");
    write(&root.join("sdk/bin/CodeGear.Delphi.Targets"), "<Project/>");
    write(
        &root.join("project/App.dproj"),
        &format!(
            r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource><Config Condition="'$(Config)'==''">Debug</Config></PropertyGroup>
  <PropertyGroup>{range_checking}</PropertyGroup>
  <Import Project="$(BDS)\Bin\CodeGear.Delphi.Targets" Condition="Exists('$(BDS)\Bin\CodeGear.Delphi.Targets')"/>
</Project>"#
        ),
    );
    write(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[installations.\"7.0\".properties]\nBDS = '{}'\nPlatform = 'Win32'\n\
             [projects.'project/App.dproj']\ninstallation = '7.0'\n",
            root.join("sdk").display()
        ),
    );
    let context = ProjectContext::discover_with_overrides(
        &root.join("project/App.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions {
            project_file: Some(root.join("project/App.dproj")),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(None),
    )
    .unwrap()
    .context;
    (temp, std::sync::Arc::unwrap_or_clone(context))
}

const SOURCE: &str = "{$IFOPT R+}{$DEFINE R_PLUS}{$ENDIF}{$IFDEF R_PLUS}{$DEFINE CHECKED}{$ENDIF}\
                      {$IFOPT C+}{$DEFINE ASSERTING}{$ENDIF}{$IFOPT O-}{$DEFINE UNOPTIMIZED}{$ENDIF}";

#[test]
fn ifopt_on_an_unset_switch_answers_with_the_dcc_default() {
    let (temp, context) = discover("");
    let project = temp.path().join("project/App.dpr");
    let library = temp.path().join("library/GIFImg.pas");
    assert_eq!(context.source_origin(&library), SourceOrigin::Library);
    for path in [&project, &library] {
        let analysed = context.conditional_context_for(path);
        assert_eq!(activity(SOURCE, "DEFINE R_PLUS", &analysed), Truth::False);
        assert_eq!(activity(SOURCE, "DEFINE ASSERTING", &analysed), Truth::True);
        assert_eq!(
            activity(SOURCE, "DEFINE UNOPTIMIZED", &analysed),
            Truth::False
        );
    }
    // Library code may be compiled with defines the project does not name,
    // so `R_PLUS` itself stays open there (D45 settles switches only).
    let analysed = context.conditional_context_for(&library);
    assert_eq!(
        activity(SOURCE, "DEFINE CHECKED", &analysed),
        Truth::Unknown
    );
}

#[test]
fn ifopt_follows_the_project_setting_in_project_code() {
    let (temp, context) = discover("<DCC_RangeChecking>true</DCC_RangeChecking>");
    let project = context.conditional_context_for(&temp.path().join("project/App.dpr"));
    assert_eq!(activity(SOURCE, "DEFINE CHECKED", &project), Truth::True);
}
