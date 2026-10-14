use super::common::{TestServer, uri, write_file};
use lsp_server::RequestId;
use serde_json::{Value, json};
pub(super) fn ambiguous_projects() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let unit = root.join("src/Unit.pas");
    let project_a = root.join("src/A.dproj");
    let project_b = root.join("src/B.dproj");
    write_file(&unit, "unit Unit; interface implementation end.");
    let contents =
        "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>";
    write_file(&project_a, contents);
    write_file(&project_b, contents);
    (directory, unit, project_a, project_b)
}

pub(super) fn open_automatic_unit(server: &mut TestServer, unit: &std::path::Path) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(unit), "languageId": "pascal", "version": 1,
            "text": "unit Unit; interface implementation end."
        }}),
    );
}

pub(super) fn project_context(
    server: &mut TestServer,
    unit: &std::path::Path,
    suffix: &str,
) -> Value {
    let id = RequestId::from(format!("project-prompt-context-{suffix}"));
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(unit)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap()
}

pub(super) fn project_context_retry_stale(
    server: &mut TestServer,
    unit: &std::path::Path,
    suffix: &str,
) -> Value {
    for attempt in 0..5 {
        let id = RequestId::from(format!("project-prompt-context-{suffix}-{attempt}"));
        server.send_request(
            id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(unit)}}),
        );
        let response = server.response(&id);
        if response
            .error
            .as_ref()
            .is_some_and(|error| error.code == -32803)
        {
            continue;
        }
        assert!(response.error.is_none(), "{response:?}");
        return response.result.unwrap();
    }
    panic!("project context stayed stale after bounded retries for {suffix}");
}

pub(super) struct SelectionFixture {
    pub(super) directory: tempfile::TempDir,
    pub(super) project_a: std::path::PathBuf,
    pub(super) project_b: std::path::PathBuf,
    pub(super) main_a: std::path::PathBuf,
}

pub(super) fn selection_fixture() -> SelectionFixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut config = String::new();
    for version in ["7.0", "37.0"] {
        let sdk = root.join("sdk").join(version);
        let ide = root.join("ide").join(version);
        write_file(
            &sdk.join("bin/rsvars.bat"),
            &format!("@SET BDS=C:\\SDK\\{version}\n"),
        );
        let property = if version == "7.0" {
            "Win32LibraryPath"
        } else {
            "DelphiLibraryPath"
        };
        write_file(
            &ide.join("EnvOptions.proj"),
            &format!(
                "<Project><PropertyGroup><{property}>$(BDS)\\source</{property}></PropertyGroup></Project>"
            ),
        );
        write_file(
            &sdk.join("source/SdkUnit.pas"),
            &format!(
                "unit SdkUnit; interface const SdkValue = {}; implementation end.",
                if version == "7.0" { 7 } else { 37 },
            ),
        );
        config.push_str(&format!(
            "[installations.\"{version}\".properties]\nBDS='{}'\nAPPDATA='{}'\nEnvironmentSettings='{}/EnvOptions.proj'\n",
            sdk.display(),
            ide.display(),
            ide.display(),
        ));
    }
    for (name, version) in [("a", "7.0"), ("b", "37.0")] {
        write_file(
            &root.join(name).join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform></PropertyGroup></Project>",
        );
        write_file(
            &root.join(name).join("App.dpr"),
            "program App; uses Main; begin end.",
        );
        write_file(
            &root.join(name).join("Main.pas"),
            "unit Main; interface uses SdkUnit; implementation procedure Run; var Value: Integer; begin Value := SdkValue; end; end.",
        );
        config.push_str(&format!(
            "[projects.\"{name}/App.dproj\"]\ninstallation='{version}'\n"
        ));
    }
    write_file(&root.join(".delphi-tools.local.toml"), &config);
    SelectionFixture {
        project_a: root.join("a/App.dproj"),
        project_b: root.join("b/App.dproj"),
        main_a: root.join("a/Main.pas"),
        directory,
    }
}
