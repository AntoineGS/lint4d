//! l4d-z473, l4d-qk5t: a Delphi build while the editor is open. It
//! rewrites the installation's `environment.proj` (outside the workspace
//! roots) and writes compiled units and other build products beside the
//! sources. Requests are not refused for it: only a change that can change
//! resolution (a new or removed source) refuses a request in flight.
#![cfg(unix)]

use super::persistent_cache::request;
use super::persistent_index::wait_index;
use super::persistent_queries::{FixtureSpec, query_fixture, settle_background};
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

const SPEC: FixtureSpec = FixtureSpec {
    units: 30,
    importers: 5,
    with_include_every: 10,
    unresolved_includes: 0,
    shared_name: "TShared",
};
const OPENS: [&str; 2] = ["App.dpr", "lib/Shared.pas"];

/// A held request's barrier, between its computation and its final
/// revalidation (`PASCAL_LSP_TEST_REFERENCES_VALIDATION_BARRIER`).
fn validation_barrier(environment: &Path) -> (TestBarrier, String) {
    let directory = environment.join("validation-barrier");
    fs::create_dir_all(&directory).unwrap();
    let barrier = TestBarrier {
        entered: directory.join("entered"),
        release: directory.join("release"),
    };
    let spec = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    // Requests pass until a test holds one.
    barrier.release();
    (barrier, spec)
}

/// A warm server over `query_fixture(SPEC)` (its store persisted by a first
/// server), holding references and workspace/symbol results at `spec`.
fn held_server(environment: &Path, root: &Path, spec: &str) -> TestServer {
    let mut first = launch(environment, "held-first", &[]);
    first.initialize(root, Value::Null);
    for relative in OPENS {
        open(&mut first, &root.join(relative));
    }
    wait_index(&mut first, super::persistent_index::persisted);
    first.shutdown();
    let mut server = launch(
        environment,
        "held",
        &[("PASCAL_LSP_TEST_REFERENCES_VALIDATION_BARRIER", spec)],
    );
    server.initialize(root, Value::Null);
    for relative in OPENS {
        open(&mut server, &root.join(relative));
    }
    wait_index(&mut server, |context| context["complete"] == true);
    settle_background(&mut server);
    server
}

fn references_to_shared(root: &Path) -> Value {
    json!({
        "textDocument": {"uri": uri(&root.join("lib/Shared.pas"))},
        "position": {"line": 2, "character": 6},
        "context": {"includeDeclaration": true},
    })
}

/// Sends `method`, waits until its computed result is held at `barrier`,
/// runs `change`, then releases it and returns the answer.
fn held_answer(
    server: &mut TestServer,
    barrier: &TestBarrier,
    id: &str,
    method: &str,
    params: Value,
    change: impl FnOnce(),
) -> Result<Value, String> {
    let _ = fs::remove_file(&barrier.entered);
    fs::remove_file(&barrier.release).unwrap();
    let request_id = RequestId::from(id.to_string());
    server.send_request(request_id.clone(), method, params);
    barrier.wait_until_entered();
    change();
    barrier.release();
    let response = server.response(&request_id);
    match response.error {
        Some(error) => Err(format!("{} {}", error.code, error.message)),
        None => Ok(response.result.unwrap_or(Value::Null)),
    }
}

/// What a Delphi build writes beside the sources: compiled units (in both
/// cases), the executable, a package, resources, a map.
fn write_build_products(directory: &Path, round: usize) {
    for name in [
        format!("Other{round}.dcu"),
        format!("Shared{round}.DCU"),
        format!("App{round}.exe"),
        format!("Pkg{round}.dcp"),
        format!("App{round}.res"),
        format!("App{round}.map"),
        format!("App{round}.drc"),
    ] {
        fs::write(directory.join(name), b"built").unwrap();
    }
}

/// l4d-qk5t: a build writes compiled units and other products into the
/// search directory while a references request and a workspace/symbol
/// request are computed. Neither can change what resolves, so both answer
/// as before. Before, the directory's new stamp refused each (-32803
/// "workspace metadata or membership changed while resolving …/lib").
#[test]
fn build_products_written_during_a_request_refuse_nothing() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let (barrier, spec) = validation_barrier(environment.path());
    let mut server = held_server(environment.path(), &root, &spec);
    let before = answer(
        &mut server,
        "references-before",
        "textDocument/references",
        references_to_shared(&root),
    )
    .expect("references before the build");
    assert!(locations(&before).len() > SPEC.importers, "{before}");

    let lib = root.join("lib");
    let references = held_answer(
        &mut server,
        &barrier,
        "references-during-build",
        "textDocument/references",
        references_to_shared(&root),
        || {
            write_build_products(&lib, 0);
            write_build_products(&root, 0);
        },
    )
    .expect("references while the build wrote its products");
    assert_eq!(locations(&references), locations(&before), "{references}");

    let symbols = held_answer(
        &mut server,
        &barrier,
        "symbols-during-build",
        "workspace/symbol",
        json!({"query": "TShared"}),
        || write_build_products(&lib, 1),
    )
    .expect("workspace/symbol while the build wrote its products");
    assert!(
        symbol_units(&symbols, "TShared").contains(&"Shared.pas".to_string()),
        "{symbols}"
    );
    server.shutdown();
}

/// l4d-qk5t: a new unit (a source the resolver can select; its extension
/// in another case) created in the search directory while references are
/// computed can change the answer: the request is refused, and the retry
/// finds the new reference.
#[test]
fn a_source_created_during_a_request_still_refuses_it() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let (barrier, spec) = validation_barrier(environment.path());
    let mut server = held_server(environment.path(), &root, &spec);
    let lib = root.join("lib");
    let created = held_answer(
        &mut server,
        &barrier,
        "references-new-unit",
        "textDocument/references",
        references_to_shared(&root),
        || {
            fs::write(
                lib.join("Late.Pas"),
                "unit Late;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  TShared.Create;\nend;\nend.\n",
            )
            .unwrap()
        },
    );
    let error = created.expect_err("a new unit refuses the request in flight");
    assert!(
        error.starts_with("-32801 ") || error.starts_with("-32803 "),
        "{error}"
    );
    assert!(error.contains("retry"), "{error}");
    server.shutdown();
}

/// l4d-qk5t: a build between two sessions writes compiled units beside the
/// persisted members. No source changed, so the next session reuses every
/// member. Before, a member's stored listing of a directory its include
/// search listed named every entry, so a new `.dcu` there made it stale and
/// it was extracted again.
#[test]
fn build_products_written_between_sessions_keep_persisted_members() {
    let done = index_after_a_build(|root| {
        write_build_products(&root.join("lib"), 0);
        write_build_products(root, 0);
    });
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], done["members"], "{done}");
}

/// l4d-qk5t: the include the persisted units looked for appears between
/// the sessions, its name in another case: their stored listing no longer
/// holds, and they are extracted again.
#[test]
fn an_include_written_between_sessions_stales_the_members_that_missed_it() {
    let done = index_after_a_build(|root| {
        write_build_products(&root.join("lib"), 0);
        fs::write(root.join("lib/MISSING.INC"), "{ found }\n").unwrap();
    });
    assert!(
        done["extracted"].as_u64().unwrap() >= 3,
        "the units including it are read again: {done}"
    );
}

/// The index status of a second session over `query_fixture(SPEC)` with
/// three units including the missing `Missing.inc` (they hold the listing
/// of each directory their include search consulted), once a first session
/// persisted every member and `build` changed the tree.
fn index_after_a_build(build: impl FnOnce(&Path)) -> Value {
    let spec = FixtureSpec {
        unresolved_includes: 3,
        ..SPEC
    };
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &spec);
    let mut first = launch(environment.path(), "persisted-first", &[]);
    first.initialize(&root, Value::Null);
    for relative in OPENS {
        open(&mut first, &root.join(relative));
    }
    wait_index(&mut first, super::persistent_index::persisted);
    first.shutdown();

    build(&root);
    let mut second = launch(environment.path(), "persisted", &[]);
    second.initialize(&root, Value::Null);
    for relative in OPENS {
        open(&mut second, &root.join(relative));
    }
    let done = wait_index(&mut second, |context| context["complete"] == true);
    second.shutdown();
    done
}
