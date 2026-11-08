//! Spec §12.1 (`l4d-zf1b.12`): the integrated cached-versus-uncached
//! matrix. Each test compares a cold persisting server, a warm second
//! server over the same store and a persistence-disabled server.
#![cfg(unix)]

use super::persistent_cache::request;
use super::persistent_index::{complete, persisted, shutdown, wait_index};
use super::persistent_queries::*;
use super::*;
use std::path::PathBuf;

/// The unmutated matrix fixture: the persisting first server and the warm
/// second server both answer like an uncached server, and the warm one
/// reused every member.
#[test]
fn an_unchanged_tree_answers_alike_cold_warm_and_uncached() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |_| {},
        &[SHARED_REFERENCES, Query::Symbols("TShared")],
    );
    assert_agree(&run.cold, &run.uncached);
    assert_agree(&run.warm, &run.uncached);
    assert_truthful(&run.warm, &run.uncached);
    assert_eq!(run.warm_status["extracted"], 0, "{}", run.warm_status);
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
}

/// Two projects, `A.dproj`/`A.dpr` and `B.dproj`/`B.dpr`, with Debug
/// (`DEBUG`, `debug/UA.pas`) and Release (`RELEASE`, `release/UB.pas`)
/// configurations. `A.dpr` uses `UA` under `DEBUG` and `UB` otherwise;
/// `B.dpr` uses `UA` always. Each `.dpr` calls `Shared` at line 3, column 2.
fn two_project_fixture(environment: &Path) -> PathBuf {
    let root = environment.join("workspace");
    for (project, main) in [("A", "A"), ("B", "B")] {
        write_file(
            &root.join(format!("{project}.dproj")),
            &format!(
                r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup>
    <MainSource>{main}.dpr</MainSource>
    <Config Condition="'$(Config)'==''">Debug</Config>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''"><Base>true</Base></PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_1)'!=''"><Cfg_1>true</Cfg_1><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_2)'!=''"><Cfg_2>true</Cfg_2><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
  <PropertyGroup Condition="'$(Cfg_1)'!=''"><DCC_Define>RELEASE;$(DCC_Define)</DCC_Define><DCC_UnitSearchPath>release</DCC_UnitSearchPath></PropertyGroup>
  <PropertyGroup Condition="'$(Cfg_2)'!=''"><DCC_Define>DEBUG;$(DCC_Define)</DCC_Define><DCC_UnitSearchPath>debug</DCC_UnitSearchPath></PropertyGroup>
  <ItemGroup>
    <DelphiCompile Include="{main}.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
    <BuildConfiguration Include="Base"><Key>Base</Key></BuildConfiguration>
    <BuildConfiguration Include="Debug"><Key>Cfg_2</Key><CfgParent>Base</CfgParent></BuildConfiguration>
    <BuildConfiguration Include="Release"><Key>Cfg_1</Key><CfgParent>Base</CfgParent></BuildConfiguration>
  </ItemGroup>
</Project>"#
            ),
        );
    }
    write_file(
        &root.join("A.dpr"),
        "program A;\n{$IFDEF DEBUG}uses UA;{$ELSE}uses UB;{$ENDIF}\nbegin\n  Shared;\nend.\n",
    );
    write_file(
        &root.join("B.dpr"),
        "program B;\nuses UA;\nbegin\n  Shared;\nend.\n",
    );
    for (directory, unit) in [("debug", "UA"), ("release", "UB")] {
        write_file(
            &root.join(format!("{directory}/{unit}.pas")),
            &format!(
                "unit {unit};\ninterface\nprocedure Shared;\nimplementation\nprocedure Shared;\nbegin\nend;\nend.\n"
            ),
        );
    }
    pascal_project::backdate_tree_for_test(&root);
    root
}

fn selection(root: &Path, project: &str, config: &str, undefines: &[&str]) -> Value {
    json!({
        "projectFile": root.join(format!("{project}.dproj")).display().to_string(),
        "buildConfig": config,
        "conditionalUndefines": undefines,
    })
}

/// A and B in one root over one store: (A, Debug), (A, Release) and
/// (B, Debug) each persist, the configuration's own unit gains a second
/// call while stopped, and the warm answers equal the uncached ones.
/// (A, Debug) and (B, Debug) differ in their `.dpr` but share `debug/UA.pas`.
#[test]
fn two_projects_a_debug_a_release_b_debug() {
    let environment = tempfile::tempdir().unwrap();
    let root = two_project_fixture(environment.path());
    let mut seen = Vec::new();
    for (project, config, unit, undefines) in [
        ("A", "Debug", "debug/UA.pas", &[][..]),
        ("A", "Release", "release/UB.pas", &["DEBUG"][..]),
        ("B", "Debug", "debug/UA.pas", &[][..]),
    ] {
        let dpr: &'static str = if project == "A" { "A.dpr" } else { "B.dpr" };
        let options = selection(&root, project, config, undefines);
        let run = differential_full(
            environment.path(),
            &root,
            &options,
            &[dpr],
            |root| {
                let path = root.join(unit);
                let text = fs::read_to_string(&path).unwrap().replace(
                    "implementation\n",
                    &format!(
                        "implementation\nprocedure Again{project}{config};\nbegin\n  Shared;\nend;\n"
                    ),
                );
                write_file(&path, &text);
                pascal_project::backdate_tree_for_test(root);
            },
            &[Query::References(dpr, 3, 2), Query::Symbols("Shared")],
        );
        assert_agree(&run.warm, &run.uncached);
        assert!(
            run.warm_status["reused"].as_u64() > Some(0),
            "{project}/{config}: {}",
            run.warm_status
        );
        let file = unit.rsplit('/').next().unwrap();
        assert!(locates(&run.warm[0], dpr, 3, 2), "{project}/{config}");
        assert!(locates(&run.warm[0], file, 6, 2), "{project}/{config}");
        seen.push(run.warm);
    }
    assert_ne!(seen[0][0], seen[2][0], "A/Debug and B/Debug differ by .dpr");
    for warm in [&seen[0], &seen[2]] {
        assert!(locates(&warm[0], "UA.pas", 2, 10), "{warm:?}");
    }
    assert_ne!(seen[0][0], seen[1][0], "Debug and Release differ");
}

/// Waits for the context anchored on `project`'s files to be `complete`.
fn wait_selected(server: &mut TestServer, project: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let status = request(server, "pascal/indexStatus", json!({}));
        let found = status["contexts"].as_array().and_then(|contexts| {
            contexts.iter().find(|context| {
                context["anchor"].to_string().contains(project) && complete(context)
            })
        });
        if let Some(context) = found {
            return context.clone();
        }
        assert!(
            Instant::now() < deadline,
            "context of {project} did not complete: {status}"
        );
    }
}

fn select_at_runtime(server: &mut TestServer, selection: &Value) {
    server.send_notification(
        "workspace/didChangeConfiguration",
        json!({"settings": {"pascalLsp": selection}}),
    );
}

/// A first server selects A/Debug then B/Debug at runtime and persists
/// both; a second starts on A/Debug, switches to B/Debug and back. After
/// each switch the selected context completes and its answers equal an
/// uncached server's; the final A context extracts nothing.
#[test]
fn a_runtime_a_b_a_switch_reuses_a() {
    let environment = tempfile::tempdir().unwrap();
    let root = two_project_fixture(environment.path());
    let a = selection(&root, "A", "Debug", &[]);
    let b = selection(&root, "B", "Debug", &[]);
    let a_queries = [Query::References("A.dpr", 3, 2), Query::Symbols("Shared")];
    let b_queries = [Query::References("B.dpr", 3, 2), Query::Symbols("Shared")];
    let mut first = launch_labelled(environment.path(), "cache", "switch-first");
    first.initialize(&root, a.clone());
    open_disk(&mut first, &root, &["A.dpr"]);
    wait_index(&mut first, persisted);
    select_at_runtime(&mut first, &b);
    open_disk(&mut first, &root, &["B.dpr"]);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let status = request(&mut first, "pascal/indexStatus", json!({}));
        let stored = status["contexts"].as_array().is_some_and(|contexts| {
            contexts.iter().any(|context| {
                context["anchor"].to_string().contains("B.dproj") && persisted(context)
            })
        });
        if stored {
            break;
        }
        assert!(Instant::now() < deadline, "B did not persist: {status}");
    }
    shutdown(first);
    let expected_a = uncached_answers(environment.path(), &root, &a, &["A.dpr"], &a_queries);
    // The second server keeps A.dpr open throughout and never opens B.dpr,
    // so the uncached reference for B opens the same document.
    let expected_b = uncached_answers(environment.path(), &root, &b, &["A.dpr"], &b_queries);

    let mut second = launch_labelled(environment.path(), "cache", "switch-second");
    second.initialize(&root, a.clone());
    open_disk(&mut second, &root, &["A.dpr"]);
    let first_a = wait_selected(&mut second, "A.dproj");
    assert_eq!(first_a["extracted"], 0, "{first_a}");
    settle_background(&mut second);
    assert_agree(&answers(&mut second, &root, &a_queries), &expected_a);

    select_at_runtime(&mut second, &b);
    wait_selected(&mut second, "B.dproj");
    settle_background(&mut second);
    assert_agree(&answers(&mut second, &root, &b_queries), &expected_b);

    select_at_runtime(&mut second, &a);
    let final_a = wait_selected(&mut second, "A.dproj");
    settle_background(&mut second);
    assert_agree(&answers(&mut second, &root, &a_queries), &expected_a);
    assert_eq!(final_a["extracted"], 0, "{final_a}");
    assert!(final_a["reused"].as_u64() > Some(0), "{final_a}");
    shutdown(second);
}

/// `lib/Shared.pas` moved out of the workspace root to
/// `<environment>/library`, reached through `sourcePaths`.
fn library_fixture(environment: &Path, move_provider: bool) -> (PathBuf, Value) {
    let root = query_fixture(environment, &MATRIX);
    let library = environment.join("library");
    fs::create_dir_all(&library).unwrap();
    if move_provider {
        fs::rename(root.join("lib/Shared.pas"), library.join("Shared.pas")).unwrap();
    }
    pascal_project::backdate_tree_for_test(&library);
    pascal_project::backdate_tree_for_test(&root);
    let options = json!({"sourcePaths": [library.display().to_string()]});
    (root, options)
}

/// A provider in a source path outside the workspace root gains a class
/// while the server is stopped.
#[test]
fn a_library_provider_edited_while_stopped() {
    let environment = tempfile::tempdir().unwrap();
    let (root, options) = library_fixture(environment.path(), true);
    let library = environment.path().join("library");
    let run = differential_full(
        environment.path(),
        &root,
        &options,
        &["App.dpr"],
        |_| {
            let path = library.join("Shared.pas");
            let text = fs::read_to_string(&path).unwrap().replace(
                "TShared = class end;\n",
                "TShared = class end;\n  TLibraryOnly = class(TShared) end;\n",
            );
            write_file(&path, &text);
            pascal_project::backdate_tree_for_test(&library);
        },
        &[
            Query::References("../library/Shared.pas", 2, 6),
            Query::Symbols("TLibraryOnly"),
        ],
    );
    assert_agree(&run.warm, &run.uncached);
    assert!(run.warm[0].locations.len() > 1, "{:?}", run.warm);
    assert!(
        locates(&run.warm[0], "library/Shared.pas", 2, 5),
        "{:?}",
        run.warm
    );
    assert_eq!(run.warm[1].locations.len(), 1, "{:?}", run.warm);
}

/// The provider starts in `lib/` (the project search path) with an empty
/// `library/` source path and moves to `library/` while stopped.
#[test]
fn a_provider_moving_between_project_and_library_origin() {
    let environment = tempfile::tempdir().unwrap();
    let (root, options) = library_fixture(environment.path(), false);
    let library = environment.path().join("library");
    let (warm, uncached) = differential_with(
        environment.path(),
        &root,
        &options,
        &["App.dpr"],
        |root| {
            fs::rename(root.join("lib/Shared.pas"), library.join("Shared.pas")).unwrap();
            pascal_project::backdate_tree_for_test(&library);
            pascal_project::backdate_tree_for_test(root);
        },
        &[
            Query::References("../library/Shared.pas", 2, 6),
            Query::Symbols("TShared"),
        ],
    );
    assert_agree(&warm, &uncached);
    assert!(locates(&warm[0], "library/Shared.pas", 2, 5), "{warm:?}");
    assert!(
        warm[0]
            .locations
            .iter()
            .all(|location| !location.contains("/lib/Shared.pas")),
        "{warm:?}"
    );
}
