//! l4d-z473, l4d-qk5t: a Delphi build while the editor is open. It
//! rewrites the installation's `environment.proj` (outside the workspace
//! roots) and writes compiled units and other build products beside the
//! sources. Requests are not refused for it: only a change that can change
//! resolution (a new or removed source) refuses a request in flight.
#![cfg(unix)]

use super::persistent_cache::request;
use super::persistent_index::wait_index;
use super::persistent_queries::settle_background;
use super::*;
use std::path::PathBuf;

/// A server over the test's store (`environment/cache`), with analysis
/// metrics, its trace in `$PASCAL_LSP_TRACE.<label>`, and `extra` variables.
fn launch(environment: &Path, label: &str, extra: &[(&str, &str)]) -> TestServer {
    let cache_home = environment.join("cache");
    let trace = std::env::var("PASCAL_LSP_TRACE")
        .ok()
        .map(|path| format!("{path}.{label}"));
    let mut variables = vec![
        ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
        ("PASCAL_LSP_ANALYSIS_METRICS", "1"),
    ];
    if let Some(trace) = &trace {
        variables.push(("PASCAL_LSP_TRACE", trace.as_str()));
    }
    variables.extend_from_slice(extra);
    TestServer::launch_test_server_with_environment_path_and_variables(environment, variables)
}

fn open(server: &mut TestServer, path: &Path) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(path),
            "languageId": "pascal",
            "version": 1,
            "text": fs::read_to_string(path).unwrap(),
        }}),
    );
}

/// The files declaring a symbol named exactly `name`.
fn symbol_units(symbols: &Value, name: &str) -> Vec<String> {
    let mut units: Vec<String> = symbols
        .as_array()
        .expect("symbols")
        .iter()
        .filter(|symbol| symbol["name"] == name)
        .filter_map(|symbol| symbol["location"]["uri"].as_str())
        .map(|uri| uri.rsplit('/').next().unwrap().to_string())
        .collect();
    units.sort();
    units.dedup();
    units
}

/// The response to `method`, or its error message.
fn answer(server: &mut TestServer, id: &str, method: &str, params: Value) -> Result<Value, String> {
    let id = RequestId::from(id.to_string());
    server.send_request(id.clone(), method, params);
    let response = server.response(&id);
    match response.error {
        Some(error) => Err(format!("{} {}", error.code, error.message)),
        None => Ok(response.result.unwrap_or(Value::Null)),
    }
}

/// `environment/workspace` with `App.dproj` (installation 7.0, unit search
/// path `lib`), `App.dpr`, `Main.pas` declaring `Run` and `lib/Helper.pas`
/// using it; the installation's IDE directory is `environment/ide`, outside
/// the workspace (Headless-Delphi keeps it under `~/.local/share`), with an
/// `environment.proj`. Returns the root, `Main.pas` and `environment.proj`.
fn installation_fixture(environment: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let root = environment.join("workspace");
    let sdk = environment.join("sdk");
    let ide = environment.join("ide");
    write_file(&sdk.join("bin/rsvars.bat"), "@SET BDS=C:\\SDK\n");
    write_file(
        &sdk.join("source/SdkUnit.pas"),
        "unit SdkUnit; interface implementation end.",
    );
    write_file(
        &ide.join("EnvOptions.proj"),
        "<Project><PropertyGroup><Win32LibraryPath>$(BDS)\\source</Win32LibraryPath></PropertyGroup></Project>",
    );
    write_file(
        &ide.join("environment.proj"),
        "<Project><PropertyGroup><EnvValue>1</EnvValue></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[installations.\"7.0\".properties]\nBDS='{}'\nAPPDATA='{}'\nPlatform='Win32'\n[projects.\"App.dproj\"]\ninstallation='7.0'\n",
            sdk.display(),
            ide.display(),
        ),
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write_file(
        &root.join("App.dpr"),
        "program App; uses Main, Helper; begin end.",
    );
    let main = root.join("Main.pas");
    write_file(
        &main,
        "unit Main;\ninterface\nuses SdkUnit;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n",
    );
    write_file(
        &root.join("lib/Helper.pas"),
        "unit Helper;\ninterface\nuses Main;\nimplementation\nprocedure Go;\nbegin\n  Run;\nend;\nend.\n",
    );
    pascal_project::backdate_tree_for_test(environment);
    (root, main, ide.join("environment.proj"))
}

/// Rewrites `path` as Headless-Delphi does on every build: the same
/// content in a new file renamed over it (a new inode and change time).
fn rewrite_in_place(path: &Path) {
    let content = fs::read(path).unwrap();
    let replacement = path.with_extension("proj.tmp");
    fs::write(&replacement, &content).unwrap();
    fs::rename(&replacement, path).unwrap();
}

/// Waits until the server's watcher applied `path` and nothing is pending.
fn wait_for_watcher(server: &mut TestServer, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut applied = false;
    loop {
        let state = request(
            server,
            "pascal/testBackgroundIdle",
            json!({"watchPath": path.display().to_string()}),
        );
        applied |= state["watchPathApplied"] == true;
        if applied && state["watchPending"] == false {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the watcher did not apply {path:?}"
        );
    }
}

fn references_to_run(main: &Path) -> Value {
    json!({
        "textDocument": {"uri": uri(main)},
        "position": {"line": 3, "character": 10},
        "context": {"includeDeclaration": true},
    })
}

/// The sorted `(file name, line)` of each location in `result`.
fn locations(result: &Value) -> Vec<(String, u64)> {
    let mut found: Vec<_> = result
        .as_array()
        .expect("locations")
        .iter()
        .map(|location| {
            (
                location["uri"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string(),
                location["range"]["start"]["line"].as_u64().unwrap(),
            )
        })
        .collect();
    found.sort();
    found
}

/// l4d-z473 (w2-gap): with the project selected explicitly, as an editor
/// does, a build rewrites the installation's `environment.proj` once (same
/// content, new inode). Every later request answers. Before, the context
/// rediscovered for the explicit owner was installed over its stale state
/// and kept the stale read of `environment.proj` beside the new watched
/// stamp: every workspace/symbol and references request was refused
/// (-32803 "workspace metadata or membership changed while resolving
/// …/environment.proj") for the rest of the session.
#[test]
fn a_rewritten_environment_file_of_a_selected_project_refuses_no_later_request() {
    let environment = tempfile::tempdir().unwrap();
    let (root, main, environment_proj) = installation_fixture(environment.path());
    let mut server = launch(environment.path(), "environment", &[]);
    server.initialize(&root, Value::Null);
    open(&mut server, &main);
    let selected = request(
        &mut server,
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&main)}, "projectUri": uri(&root.join("App.dproj"))}),
    );
    assert!(
        selected["installationConfigUris"]
            .as_array()
            .is_some_and(|uris| uris.iter().any(|uri| uri
                .as_str()
                .is_some_and(|uri| uri.ends_with("/ide/environment.proj")))),
        "{selected}"
    );
    wait_index(&mut server, |context| context["complete"] == true);
    settle_background(&mut server);
    let before = answer(
        &mut server,
        "references-before",
        "textDocument/references",
        references_to_run(&main),
    )
    .expect("references before the build");

    // The context's documents are closed: requests resolve them from their
    // known owners.
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    rewrite_in_place(&environment_proj);
    // Requests come more than `STAMP_SETTLE_WINDOW` after the build wrote
    // it: its new stamp is settled.
    pascal_project::backdate_tree_for_test(&environment_proj);
    // The build also rewrites a closed source the session loaded as a
    // dependency: its index member is proven again from a snapshot.
    let helper = root.join("lib/Helper.pas");
    let text = fs::read_to_string(&helper).unwrap();
    fs::write(&helper, format!("{text}{{ built }}\n")).unwrap();
    wait_for_watcher(&mut server, &helper);
    let mut refused = Vec::new();
    for attempt in 0..3 {
        match answer(
            &mut server,
            &format!("symbols-{attempt}"),
            "workspace/symbol",
            json!({"query": "Run"}),
        ) {
            Ok(symbols) => assert_eq!(symbol_units(&symbols, "Run"), ["Main.pas"], "{symbols}"),
            Err(error) => refused.push(format!("workspace/symbol {attempt}: {error}")),
        }
        match answer(
            &mut server,
            &format!("references-{attempt}"),
            "textDocument/references",
            references_to_run(&main),
        ) {
            Ok(found) => assert_eq!(locations(&found), locations(&before), "{found}"),
            Err(error) => refused.push(format!("references {attempt}: {error}")),
        }
        if let Err(error) = answer(
            &mut server,
            &format!("document-symbols-{attempt}"),
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri(&main)}}),
        ) {
            refused.push(format!("documentSymbol {attempt}: {error}"));
        }
    }
    assert!(refused.is_empty(), "{refused:#?}");
    server.shutdown();
}
