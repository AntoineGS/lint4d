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
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
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
    let run = differential_uncold(
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
    let (warm, uncached) = (run.warm, run.uncached);
    assert_agree(&warm, &uncached);
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
    assert!(locates(&warm[0], "library/Shared.pas", 2, 5), "{warm:?}");
    assert!(
        warm[0]
            .locations
            .iter()
            .all(|location| !location.contains("/lib/Shared.pas")),
        "{warm:?}"
    );
}

fn assert_reused(run: &Differential) {
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
}

/// `Importer0.pas` includes `lib/Outer.inc` twice in its implementation
/// (a conditional include is a documented analysis limit, so none guards
/// the repetition); `Outer.inc` includes `Inner.inc`. While stopped,
/// `Inner.inc` gains `FromInner`, which spells `TShared`.
#[test]
fn nested_and_repeated_includes_edited_while_stopped() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let importer = root.join("lib/Importer0.pas");
    let text = fs::read_to_string(&importer)
        .unwrap()
        .replace("implementation\n", "implementation\n{$I Outer.inc}\n")
        .replace("end.\n", "{$I Outer.inc}\nend.\n");
    write_file(&importer, &text);
    write_file(&root.join("lib/Outer.inc"), "{$I Inner.inc}\n");
    write_file(&root.join("lib/Inner.inc"), "{ inner }\n");
    pascal_project::backdate_tree_for_test(&root);
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |root| {
            write_file(
                &root.join("lib/Inner.inc"),
                "procedure FromInner;\nbegin\n  TShared.Create;\nend;\n",
            );
            pascal_project::backdate_tree_for_test(root);
        },
        &[SHARED_REFERENCES, Query::Symbols("FromInner")],
    );
    assert_agree(&run.warm, &run.uncached);
    assert_reused(&run);
    assert!(locates(&run.warm[0], "Inner.inc", 2, 2), "{:?}", run.warm);
    // The two expansions of `Inner.inc` map to one source location, which
    // the server reports once: the warm answer gains exactly that location
    // over the pre-mutation one and keeps `Importer0.pas`'s own.
    assert!(
        !run.cold[0]
            .locations
            .iter()
            .any(|location| location.contains("Inner.inc")),
        "{:?}",
        run.cold
    );
    assert_eq!(
        run.warm[0].locations.len(),
        run.cold[0].locations.len() + 1,
        "cold {:?} warm {:?}",
        run.cold,
        run.warm
    );
    assert!(
        locates(&run.warm[0], "Importer0.pas", 7, 2),
        "{:?}",
        run.warm
    );
    assert_eq!(run.warm[1].locations.len(), 1, "{:?}", run.warm);
}

/// `Importer0.pas` includes `lib/Outer.inc` in its interface and its
/// implementation; `Outer.inc` defines `OUTER_SEEN` the first time and
/// includes `Inner.inc` the second (`{$IFNDEF OUTER_SEEN}`). `Inner.inc`
/// gains `FromInner` while stopped. A conditional include is a documented
/// analysis limit: the uncached server reports the references as
/// incomplete too, so the check is that the warm server reports the same
/// coverage and never more than the uncached one.
#[test]
fn a_guarded_include_edited_while_stopped_is_truthful() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let importer = root.join("lib/Importer0.pas");
    let text = fs::read_to_string(&importer)
        .unwrap()
        .replace("uses Shared;\n", "uses Shared;\n{$I Outer.inc}\n")
        .replace("implementation\n", "implementation\n{$I Outer.inc}\n");
    write_file(&importer, &text);
    write_file(
        &root.join("lib/Outer.inc"),
        "{$IFNDEF OUTER_SEEN}{$DEFINE OUTER_SEEN}{$ELSE}{$I Inner.inc}{$ENDIF}\n",
    );
    write_file(&root.join("lib/Inner.inc"), "{ inner }\n");
    pascal_project::backdate_tree_for_test(&root);
    let run = differential_uncold(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |root| {
            write_file(
                &root.join("lib/Inner.inc"),
                "procedure FromInner;\nbegin\n  TShared.Create;\nend;\n",
            );
            pascal_project::backdate_tree_for_test(root);
        },
        &[SHARED_REFERENCES, Query::Symbols("FromInner")],
    );
    assert_truthful(&run.warm, &run.uncached);
    assert_reused(&run);
    for (warm, uncached) in run.warm.iter().zip(&run.uncached) {
        assert_eq!(warm.complete, uncached.complete, "{warm:?} {uncached:?}");
        assert_eq!(warm.notes, uncached.notes);
    }
}

/// Three units include `Missing.inc`, which does not resolve; while
/// stopped it appears and declares `TFromMissing`. Follow-up R9-4 allowed
/// the warm server to keep reporting the three members as a gap; it does
/// not (complete and equal to the uncached answer), and that is asserted.
#[test]
fn a_missing_include_appearing_while_stopped_is_truthful() {
    const MISSING: FixtureSpec = FixtureSpec {
        units: 60,
        importers: 5,
        with_include_every: 10,
        unresolved_includes: 3,
        shared_name: "TShared",
    };
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MISSING);
    let run = differential_uncold(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |root| {
            write_file(
                &root.join("lib/Missing.inc"),
                "type TFromMissing = class(TShared) end;\n",
            );
            pascal_project::backdate_tree_for_test(root);
        },
        &[SHARED_REFERENCES, Query::Symbols("TFromMissing")],
    );
    assert_agree(&run.warm, &run.uncached);
    assert_reused(&run);
}

/// `App.dproj`'s search path gains `lib2` and `App.dpr` uses
/// `lib2/Extra.pas`, which imports `Shared` and spells `TShared`.
#[test]
fn a_project_metadata_edit_while_stopped() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &["App.dpr"],
        |root| {
            let project = root.join("App.dproj");
            let text = fs::read_to_string(&project).unwrap().replace(
                ">lib</DCC_UnitSearchPath>",
                ">lib;lib2</DCC_UnitSearchPath>",
            );
            write_file(&project, &text);
            write_file(
                &root.join("App.dpr"),
                "program App;\nuses Extra;\nbegin\nend.\n",
            );
            write_file(
                &root.join("lib2/Extra.pas"),
                "unit Extra;\ninterface\nuses Shared;\ntype TExtra = class(TShared) end;\nimplementation\nend.\n",
            );
            pascal_project::backdate_tree_for_test(root);
        },
        &[SHARED_REFERENCES, Query::Symbols("TExtra")],
    );
    assert_agree(&run.warm, &run.uncached);
    assert_reused(&run);
    assert!(
        run.warm[0]
            .locations
            .iter()
            .any(|location| location.contains("/lib2/Extra.pas:")),
        "{:?}",
        run.warm
    );
    assert_eq!(run.warm[1].locations.len(), 1, "{:?}", run.warm);
}

/// The provider sits behind `$(SHAREDDIR)` in `App.dproj`'s search path.
/// The user override config (`delphi-tools/config.toml`, shared by every
/// server of the environment) sets `SHAREDDIR = 'lib1'`; while stopped it
/// becomes `lib2`, whose `Shared.pas` also declares `TOnlyInLib2`.
#[test]
fn a_delphi_environment_variable_change_while_stopped() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    fs::create_dir_all(root.join("lib1")).unwrap();
    fs::rename(root.join("lib/Shared.pas"), root.join("lib1/Shared.pas")).unwrap();
    let project = root.join("App.dproj");
    let text = fs::read_to_string(&project).unwrap().replace(
        ">lib</DCC_UnitSearchPath>",
        ">lib;$(SHAREDDIR)</DCC_UnitSearchPath>",
    );
    write_file(&project, &text);
    let config = environment.path().join("config/delphi-tools/config.toml");
    write_file(&config, "[properties]\nSHAREDDIR = 'lib1'\n");
    pascal_project::backdate_tree_for_test(&root);
    pascal_project::backdate_tree_for_test(&environment.path().join("config"));
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &["App.dpr"],
        |root| {
            write_file(
                &root.join("lib2/Shared.pas"),
                "unit Shared;\ninterface\ntype TShared = class end;\n  TOnlyInLib2 = class(TShared) end;\nimplementation\nend.\n",
            );
            write_file(&config, "[properties]\nSHAREDDIR = 'lib2'\n# switched\n");
            pascal_project::backdate_tree_for_test(root);
            pascal_project::backdate_tree_for_test(config.parent().unwrap().parent().unwrap());
        },
        &[
            Query::References("lib/Importer0.pas", 6, 2),
            Query::Symbols("TOnlyInLib2"),
        ],
    );
    assert_agree(&run.warm, &run.uncached);
    assert_reused(&run);
    assert!(run.cold[1].locations.is_empty(), "{:?}", run.cold);
    assert_eq!(run.warm[1].locations.len(), 1, "{:?}", run.warm);
    assert!(
        run.warm[0]
            .locations
            .iter()
            .any(|location| location.contains("/lib2/Shared.pas:")),
        "{:?}",
        run.warm
    );
}

/// `Shared.pas` declares `TLegacyOnly` unless `VER360`. The first server
/// runs as compiler 24.0, the warm and uncached servers as 36.0; nothing
/// changes on disk.
#[test]
fn a_compiler_version_change_while_stopped() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    write_file(
        &root.join("lib/Shared.pas"),
        "unit Shared;\ninterface\ntype TShared = class end;\n{$IFDEF VER360}{$ELSE}type TLegacyOnly = class(TShared) end;{$ENDIF}\nimplementation\nend.\n",
    );
    pascal_project::backdate_tree_for_test(&root);
    let run = differential_switch(
        environment.path(),
        &root,
        &json!({"compilerVersion": "24.0"}),
        &json!({"compilerVersion": "36.0"}),
        &OPENS,
        |_| {},
        &[SHARED_REFERENCES, Query::Symbols("TLegacyOnly")],
    );
    assert_agree(&run.warm, &run.uncached);
    // The compiler version is part of the context key, so nothing of the
    // first server's records is reusable: the warm server extracts anew.
    assert!(
        run.warm_status["members"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
    assert_eq!(run.warm_status["reused"], 0, "{}", run.warm_status);
    assert_eq!(
        run.warm_status["extracted"], run.warm_status["members"],
        "{}",
        run.warm_status
    );
    assert_eq!(run.cold[1].locations.len(), 1, "{:?}", run.cold);
    assert!(run.warm[1].locations.is_empty(), "{:?}", run.warm);
    assert_ne!(run.warm, run.cold, "the version changes the answers");
}

/// The `Importer`'s occurrence moves from column 2 to column 0; size and
/// modification time stay.
fn move_occurrence_keeping_the_stamp(root: &Path, importer: &str) {
    let path = root.join(importer);
    let text = fs::read_to_string(&path).unwrap();
    let moved = text.replace("begin\n  TShared.Create;\n", "begin\nTShared.Create;  \n");
    assert_ne!(moved, text);
    rewrite_keeping_the_stamp(&path, &moved);
}

/// `workspace/didChangeWatchedFiles` with `(path, type)` entries (1 created,
/// 2 changed, 3 deleted).
fn watched_batch(server: &mut TestServer, changes: &[(PathBuf, u8)]) {
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": changes
            .iter()
            .map(|(path, kind)| json!({"uri": uri(path), "type": kind}))
            .collect::<Vec<_>>()}),
    );
}

/// Waits until the server has processed the change events: the (complete)
/// context's `extracted` counter, 0 on a warm server that reused every
/// member, only rises once the changed members were re-read.
fn await_extraction(server: &mut TestServer) -> Value {
    wait_index(server, |context| {
        complete(context) && context["extracted"].as_u64() > Some(0)
    })
}

/// Answers `SHARED_REFERENCES` and `Symbols("TShared")` on a warm server
/// once its index is complete and its background settled, stops it and
/// returns them with the uncached answers of the tree as it then is.
fn finish_and_compare(
    environment: &Path,
    root: &Path,
    mut server: TestServer,
    queries: &[Query],
) -> (Vec<Answer>, Vec<Answer>, Value) {
    let status = wait_index(&mut server, complete);
    settle_background(&mut server);
    let warm = answers(&mut server, root, queries);
    shutdown(server);
    let uncached = uncached_answers(environment, root, &Value::Null, &OPENS, queries);
    (warm, uncached, status)
}

/// The first half of a same-stamp write while the warm server runs:
/// `Importer3.pas` is rewritten (occurrence to column 0, stamp kept) and a
/// `Changed` event names it. The second half rewrites `Importer4.pas` the
/// same way with no event and restarts the server, which never trusts
/// watcher coverage.
#[test]
fn a_same_stamp_write_during_the_session() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let mut server = warm_server(environment.path(), &root, &OPENS);
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let before = answers(&mut server, &root, &queries);
    assert!(before.iter().all(|answer| answer.complete), "{before:?}");
    assert!(locates(&before[0], "Importer3.pas", 6, 2), "{before:?}");

    move_occurrence_keeping_the_stamp(&root, "lib/Importer3.pas");
    watched_batch(&mut server, &[(root.join("lib/Importer3.pas"), 2)]);
    let processed = await_extraction(&mut server);
    assert!(processed["extracted"].as_u64() >= Some(1), "{processed}");
    let (warm, uncached, _) = finish_and_compare(environment.path(), &root, server, &queries);
    assert_agree(&warm, &uncached);
    assert_ne!(warm, before, "the write is visible");
    assert!(locates(&warm[0], "Importer3.pas", 6, 0), "{warm:?}");
    assert!(!locates(&warm[0], "Importer3.pas", 6, 2), "{warm:?}");

    move_occurrence_keeping_the_stamp(&root, "lib/Importer4.pas");
    let restarted = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let (warm, uncached, status) =
        finish_and_compare(environment.path(), &root, restarted, &queries);
    assert_agree(&warm, &uncached);
    assert!(locates(&warm[0], "Importer4.pas", 6, 0), "{warm:?}");
    assert!(locates(&warm[0], "Importer3.pas", 6, 0), "{warm:?}");
    assert!(status["reused"].as_u64() > Some(0), "{status}");
}

/// A checkout's effect: `Other3.pas` deleted, `Branch.pas` added,
/// `Importer1.pas` replaced by other content with an older mtime than any
/// stamp seen, `Importer2.pas` restored byte-identical with a new mtime and
/// `App.dproj` edited by whitespace only. Everything settled.
fn branch_switch(root: &Path) {
    let first_stamp = fs::metadata(root.join("lib/Importer1.pas"))
        .unwrap()
        .modified()
        .unwrap();
    fs::remove_file(root.join("lib/Other3.pas")).unwrap();
    write_file(
        &root.join("lib/Branch.pas"),
        "unit Branch;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  TShared.Create;\nend;\nend.\n",
    );
    write_file(
        &root.join("lib/Importer1.pas"),
        "unit Importer1;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  TShared.Create;\n  TShared.Create;\nend;\nend.\n",
    );
    let importer2 = root.join("lib/Importer2.pas");
    let text = fs::read_to_string(&importer2).unwrap();
    write_file(&importer2, &text);
    let project = root.join("App.dproj");
    let text = fs::read_to_string(&project).unwrap();
    write_file(&project, &format!("{text}\n"));
    pascal_project::backdate_tree_for_test(root);
    fs::File::options()
        .write(true)
        .open(root.join("lib/Importer1.pas"))
        .unwrap()
        .set_modified(first_stamp - Duration::from_secs(3600))
        .unwrap();
}

fn branch_changes(root: &Path) -> Vec<(PathBuf, u8)> {
    vec![
        (root.join("lib/Other3.pas"), 3),
        (root.join("lib/Branch.pas"), 1),
        (root.join("lib/Importer1.pas"), 2),
        (root.join("lib/Importer2.pas"), 2),
        (root.join("App.dproj"), 2),
    ]
}

fn assert_branch_answers(warm: &[Answer]) {
    assert!(locates(&warm[0], "Branch.pas", 6, 2), "{warm:?}");
    assert!(locates(&warm[0], "Importer1.pas", 7, 2), "{warm:?}");
    assert!(
        !warm[0]
            .locations
            .iter()
            .any(|location| location.contains("Other3.pas")),
        "{warm:?}"
    );
    assert!(
        !warm[1]
            .locations
            .iter()
            .any(|location| location.contains("Other3.pas")),
        "{warm:?}"
    );
}

/// The branch switch while the first server is stopped.
#[test]
fn a_branch_switch_while_stopped() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        branch_switch,
        &[SHARED_REFERENCES, Query::Symbols("TShared")],
    );
    assert_agree(&run.warm, &run.uncached);
    assert_branch_answers(&run.warm);
    assert_reused(&run);
    let members = run.warm_status["members"].as_u64().unwrap();
    let extracted = run.warm_status["extracted"].as_u64().unwrap();
    // `Branch.pas` (new) and `Importer1.pas` (other bytes). `Importer2.pas`
    // (same bytes, new mtime) and the whitespace-edited `App.dproj` are
    // reused by content; `Other3.pas` is gone.
    assert_eq!(extracted, 2, "{}", run.warm_status);
    assert_eq!(
        run.warm_status["reused"],
        members - 2,
        "{}",
        run.warm_status
    );
}

/// The same switch while the warm server runs, announced by one watched
/// batch naming every change.
#[test]
fn a_branch_switch_while_running() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let mut server = warm_server(environment.path(), &root, &OPENS);
    branch_switch(&root);
    watched_batch(&mut server, &branch_changes(&root));
    let processed = await_extraction(&mut server);
    assert!(processed["extracted"].as_u64() >= Some(2), "{processed}");
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let (warm, uncached, _) = finish_and_compare(environment.path(), &root, server, &queries);
    assert_agree(&warm, &uncached);
    assert_branch_answers(&warm);
}

/// The same switch, but the server is told through an oversized watched
/// batch (65 entries, past `MAX_FILE_OPERATION_BATCH_ENTRIES`) that names
/// nothing real: it discards derived state and must rediscover the tree.
#[test]
fn a_branch_switch_while_running_with_a_watcher_overflow() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let mut server = warm_server(environment.path(), &root, &OPENS);
    branch_switch(&root);
    let padding: Vec<_> = (0..65)
        .map(|index| (root.join(format!("lib/Padding{index}.pas")), 2))
        .collect();
    watched_batch(&mut server, &padding);
    let processed = await_extraction(&mut server);
    assert!(processed["extracted"].as_u64() >= Some(2), "{processed}");
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let (warm, uncached, _) = finish_and_compare(environment.path(), &root, server, &queries);
    assert_agree(&warm, &uncached);
    assert_branch_answers(&warm);
}

const GENERATED: &str = "procedure Gen;\nbegin\n  TShared.Create;\nend;\n";

/// `Importer0.pas` includes `lib/Generated.inc`. While stopped a generator
/// rewrites it (a) with the same bytes and a new mtime, then (b) in a
/// second round with other bytes of the same size and the mtime of (a).
#[test]
fn a_regenerated_source_is_reused_or_reextracted_by_content() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let importer = root.join("lib/Importer0.pas");
    let text = fs::read_to_string(&importer)
        .unwrap()
        .replace("implementation\n", "implementation\n{$I Generated.inc}\n");
    write_file(&importer, &text);
    let generated = root.join("lib/Generated.inc");
    write_file(&generated, GENERATED);
    pascal_project::backdate_tree_for_test(&root);
    let queries = [SHARED_REFERENCES, Query::Symbols("Gen")];

    let mut first = launch_labelled(environment.path(), "cache", "generated-first");
    first.initialize(&root, Value::Null);
    open_disk(&mut first, &root, &OPENS);
    wait_index(&mut first, persisted);
    let original = answers(&mut first, &root, &queries);
    shutdown(first);
    assert!(locates(&original[0], "Generated.inc", 2, 2), "{original:?}");

    write_file(&generated, GENERATED);
    pascal_project::backdate_tree_for_test(&root);
    let cached = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let (warm, uncached, status) = finish_and_compare(environment.path(), &root, cached, &queries);
    assert_agree(&warm, &uncached);
    assert_eq!(warm, original);
    assert_eq!(status["extracted"], 0, "{status}");
    assert_eq!(status["reused"], status["members"], "{status}");

    rewrite_keeping_the_stamp(
        &generated,
        &GENERATED.replace("begin\n  TShared.Create;\n", "begin\nTShared.Create;  \n"),
    );
    let cached = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let (warm, uncached, status) = finish_and_compare(environment.path(), &root, cached, &queries);
    assert_agree(&warm, &uncached);
    assert!(locates(&warm[0], "Generated.inc", 2, 0), "{warm:?}");
    assert!(!locates(&warm[0], "Generated.inc", 2, 2), "{warm:?}");
    assert!(status["reused"].as_u64() > Some(0), "{status}");
}

/// A mutation applied with fresh mtimes (a changed member, a new member and
/// their directory) while the warm server is stopped. The immediate answers
/// are truthful (a partial answer only lists uncached locations); the
/// context still completes by itself (re-extracting the changed and new
/// members; it does not wait for the 2 s settle window) and the answers
/// equal the uncached ones.
#[test]
fn an_unsettled_tree_is_truthful_then_completes() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let mut first = launch_labelled(environment.path(), "cache", "unsettled-first");
    first.initialize(&root, Value::Null);
    open_disk(&mut first, &root, &OPENS);
    wait_index(&mut first, persisted);
    shutdown(first);

    let importer = root.join("lib/Importer3.pas");
    let text = fs::read_to_string(&importer).unwrap();
    write_file(
        &importer,
        &text.replace("begin\n  TShared.Create;\n", "begin\nTShared.Create;  \n"),
    );
    write_file(
        &root.join("lib/Fresh.pas"),
        "unit Fresh;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  TShared.Create;\nend;\nend.\n",
    );

    let mut server = launch_labelled(environment.path(), "cache", "unsettled-warm");
    server.initialize(&root, Value::Null);
    open_disk(&mut server, &root, &OPENS);
    let started = Instant::now();
    let immediate = answers(&mut server, &root, &queries);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        let status = request(&mut server, "pascal/indexStatus", json!({}));
        if let Some(context) = status["contexts"]
            .as_array()
            .and_then(|contexts| contexts.iter().find(|context| complete(context)))
        {
            break context.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the context never completed after {:?}: {status}",
            started.elapsed()
        );
    };
    settle_background(&mut server);
    let settled = answers(&mut server, &root, &queries);
    shutdown(server);
    let uncached = uncached_answers(environment.path(), &root, &Value::Null, &OPENS, &queries);
    assert_truthful(&immediate, &uncached);
    assert_agree(&settled, &uncached);
    assert!(locates(&settled[0], "Fresh.pas", 6, 2), "{settled:?}");
    assert!(locates(&settled[0], "Importer3.pas", 6, 0), "{settled:?}");
    assert!(status["reused"].as_u64() > Some(0), "{status}");
    assert!(status["extracted"].as_u64() >= Some(1), "{status}");
}

/// Servers One and Two run concurrently over one warmed store. One holds
/// an unsaved `Importer0.pas` without its `TShared` occurrence, Two an
/// unsaved `Other1.pas` that imports `Shared` and spells `TShared`. Each
/// answers like an uncached server with the same buffer, neither sees the
/// other's buffer, and a later warm server without buffers answers like an
/// uncached one on disk.
#[test]
fn two_live_sessions_with_distinct_overlays_answer_from_their_own_buffers() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let mut first = launch_labelled(environment.path(), "cache", "overlay-first");
    first.initialize(&root, Value::Null);
    open_disk(&mut first, &root, &OPENS);
    wait_index(&mut first, persisted);
    shutdown(first);

    let importer = fs::read_to_string(root.join("lib/Importer0.pas"))
        .unwrap()
        .replace("  TShared.Create;\n", "");
    let other = fs::read_to_string(root.join("lib/Other1.pas"))
        .unwrap()
        .replace("type TShared = class end;\n", "uses Shared;\n");
    assert!(other.contains("uses Shared;") && other.contains("TShared.Create"));
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];

    let mut one = launch_labelled(environment.path(), "cache", "overlay-one");
    let mut two = launch_labelled(environment.path(), "cache", "overlay-two");
    for (server, label, text) in [
        (&mut one, "lib/Importer0.pas", &importer),
        (&mut two, "lib/Other1.pas", &other),
    ] {
        server.initialize(&root, Value::Null);
        open_disk(server, &root, &OPENS);
        open_text(server, &root.join(label), text);
    }
    for server in [&mut one, &mut two] {
        let status = wait_index(server, complete);
        assert!(status["reused"].as_u64() > Some(0), "{status}");
        settle_background(server);
    }
    let from_one = answers(&mut one, &root, &queries);
    let from_two = answers(&mut two, &root, &queries);
    shutdown(one);
    shutdown(two);

    let uncached_one = uncached_answers_with_overlays(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        &[("lib/Importer0.pas", &importer)],
        &queries,
    );
    let uncached_two = uncached_answers_with_overlays(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        &[("lib/Other1.pas", &other)],
        &queries,
    );
    assert_agree(&from_one, &uncached_one);
    assert_agree(&from_two, &uncached_two);
    assert!(
        !locates(&from_one[0], "Importer0.pas", 6, 2),
        "{from_one:?}"
    );
    assert!(!locates(&from_one[0], "Other1.pas", 6, 2), "{from_one:?}");
    assert!(locates(&from_two[0], "Importer0.pas", 6, 2), "{from_two:?}");
    assert!(locates(&from_two[0], "Other1.pas", 6, 2), "{from_two:?}");
    assert!(
        from_one[1]
            .locations
            .iter()
            .any(|symbol| symbol.contains("/lib/Other1.pas")),
        "{from_one:?}"
    );
    assert!(
        from_two[1]
            .locations
            .iter()
            .all(|symbol| !symbol.contains("/lib/Other1.pas")),
        "{from_two:?}"
    );

    let mut third = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let status = wait_index(&mut third, complete);
    let warm = answers(&mut third, &root, &queries);
    shutdown(third);
    let on_disk = uncached_answers(environment.path(), &root, &Value::Null, &OPENS, &queries);
    assert_agree(&warm, &on_disk);
    assert!(locates(&warm[0], "Importer0.pas", 6, 2), "{warm:?}");
    assert!(!locates(&warm[0], "Other1.pas", 6, 2), "{warm:?}");
    assert_eq!(status["extracted"], 0, "{status}");
    assert!(status["reused"].as_u64() > Some(0), "{status}");
}

/// `Importer0.pas` is edited twice in a buffer (the occurrence removed,
/// then moved to column 0), the final buffer is written to disk with
/// `didSave` and a `Changed` event, and the document closes. A fresh warm
/// server answers like an uncached one on disk, so only the saved text was
/// published.
#[test]
fn a_save_transition_publishes_the_saved_text_only() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let path = root.join("lib/Importer0.pas");
    let original = fs::read_to_string(&path).unwrap();
    let removed = original.replace("  TShared.Create;\n", "");
    let moved = original.replace("  TShared.Create;\n", "TShared.Create;\n");
    assert!(removed != original && moved != original);
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];

    let mut server = warm_server(environment.path(), &root, &OPENS);
    open_disk(&mut server, &root, &["lib/Importer0.pas"]);
    super::persistent_cache::change(&mut server, &path, 2, &removed);
    super::persistent_cache::change(&mut server, &path, 3, &moved);
    settle_background(&mut server);
    fs::write(&path, &moved).unwrap();
    server.send_notification(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri(&path)}}),
    );
    super::persistent_cache::watched_change(&mut server, &path);
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&path)}}),
    );
    let processed = await_extraction(&mut server);
    assert!(processed["extracted"].as_u64() >= Some(1), "{processed}");
    settle_background(&mut server);
    shutdown(server);

    let mut fresh = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let status = wait_index(&mut fresh, complete);
    let warm = answers(&mut fresh, &root, &queries);
    shutdown(fresh);
    let uncached = uncached_answers(environment.path(), &root, &Value::Null, &OPENS, &queries);
    assert_agree(&warm, &uncached);
    assert!(locates(&warm[0], "Importer0.pas", 6, 0), "{warm:?}");
    assert!(!locates(&warm[0], "Importer0.pas", 6, 2), "{warm:?}");
    assert!(status["reused"].as_u64() > Some(0), "{status}");
}

/// `textDocument/rename` through the protocol: the in-process rule that
/// rename never uses index selection is
/// `rename_and_code_actions_never_use_index_selection`
/// (src/workspace/queries.rs); here a warm server's edits equal an
/// uncached server's. With unresolved includes both refuse or both edit
/// identically; a smaller warm edit set would be an unproven edit.
#[test]
fn rename_on_a_warm_server_edits_like_an_uncached_one() {
    const RENAME: Query = Query::Rename("lib/Shared.pas", 2, 6, "TRenamed");
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |_| {},
        &[RENAME],
    );
    assert_agree(&run.cold, &run.uncached);
    assert_agree(&run.warm, &run.uncached);
    assert!(
        run.warm[0].locations.len() > MATRIX.importers,
        "{:?}",
        run.warm
    );
    assert_eq!(run.warm_status["extracted"], 0, "{}", run.warm_status);
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );

    let environment = tempfile::tempdir().unwrap();
    let spec = FixtureSpec {
        unresolved_includes: 3,
        ..MATRIX
    };
    let root = query_fixture(environment.path(), &spec);
    let run = differential_uncold(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |_| {},
        &[RENAME],
    );
    let (warm, uncached) = (&run.warm[0], &run.uncached[0]);
    for answer in [warm, uncached] {
        assert!(
            !answer.complete && answer.locations.is_empty(),
            "{answer:?}"
        );
        assert!(
            answer.notes.len() == 1
                && answer.notes[0].starts_with("rename workspace scan incomplete: ")
                && answer.notes[0].contains("include Missing.inc was not found"),
            "{answer:?}"
        );
    }
    assert_eq!(warm.notes, uncached.notes, "the same refusal");
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
}

/// `VmHWM` (KiB) from `/proc/<pid>/status` and `write_bytes` and `rchar`
/// from `/proc/<pid>/io` of the server process; read just before shutdown,
/// after `settle_background`, so the measurement adds no work of its own.
fn process_peaks(server: &TestServer) -> (u64, u64, u64) {
    let pid = server.child.id();
    let field = |file: &str, name: &str| -> u64 {
        fs::read_to_string(format!("/proc/{pid}/{file}"))
            .unwrap_or_else(|error| panic!("read /proc/{pid}/{file}: {error}"))
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| {
                rest.trim_start_matches(':')
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .unwrap_or_else(|| panic!("{name} missing from /proc/{pid}/{file}"))
    };
    (
        field("status", "VmHWM"),
        field("io", "write_bytes"),
        field("io", "rchar"),
    )
}

/// The sizes in bytes of the analysis store's database and write-ahead log
/// under `<environment>/<cache>`.
fn store_sizes(environment: &Path, cache: &str) -> (u64, u64) {
    let directory = environment.join(cache).join("lint4d/analysis");
    let (mut database, mut wal) = (0, 0);
    for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if name.starts_with("analysis-v") && name.ends_with(".sqlite3") {
            database += size;
        } else if name.starts_with("analysis-v") && name.ends_with(".sqlite3-wal") {
            wal += size;
        }
    }
    (database, wal)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

/// One JSON line on stdout and, when `PASCAL_LSP_ACCEPTANCE_RECORD` names a
/// file, appended to it.
fn emit(record: Value) {
    println!("{record}");
    if let Ok(path) = std::env::var("PASCAL_LSP_ACCEPTANCE_RECORD") {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open the acceptance record");
        writeln!(file, "{record}").expect("append to the acceptance record");
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn sample_summary(samples: &[Duration]) -> Value {
    let (p50, p95) = percentiles(samples.to_vec());
    let (rest50, rest95) = percentiles(samples[1..].to_vec());
    json!({
        "all": {"p50_ms": millis(p50), "p95_ms": millis(p95)},
        "after_first": {"p50_ms": millis(rest50), "p95_ms": millis(rest95)},
        "first_ms": millis(samples[0]),
    })
}

/// Spec §12.2: the synthetic acceptance record (supersedes
/// `synthetic_warm_query_latency`), numbers only. Five paired cold builds of
/// the 5,000-unit fixture (persistence on to `persisted`, off to `complete`,
/// each on a fresh store), then a warm server on the last store with 30
/// reference and 30 symbol samples. One JSON object per line on stdout and
/// in `PASCAL_LSP_ACCEPTANCE_RECORD`. Run with `cargo test --release
/// --features test-support --test protocol_barriers
/// synthetic_acceptance_record -- --ignored --nocapture`.
#[test]
#[ignore = "release measurement"]
fn synthetic_acceptance_record() {
    const LARGE: FixtureSpec = FixtureSpec {
        units: 5_000,
        importers: 20,
        with_include_every: 10,
        unresolved_includes: 50,
        shared_name: "TShared",
    };
    const PAIRS: usize = 5;
    const RUNS: usize = 30;
    const KIB_PER_MIB: f64 = 1024.0;
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &LARGE);
    let shared = root.join("lib/Shared.pas");
    let (mut ratios, mut deltas) = (Vec::new(), Vec::new());
    let mut uncached = None;
    for pair in 0..PAIRS {
        let cache = format!("cache-{pair}");
        let started = Instant::now();
        let mut on = launch_labelled(environment.path(), &cache, "record-on");
        on.initialize(&root, Value::Null);
        open_disk(&mut on, &root, &OPENS);
        let done = wait_index(&mut on, persisted);
        let on_seconds = started.elapsed().as_secs_f64();
        settle_background(&mut on);
        let on_status = request(&mut on, "pascal/cacheStatus", json!({}));
        let (on_hwm, on_written, on_read) = process_peaks(&on);
        let (running_db, running_wal) = store_sizes(environment.path(), &cache);
        shutdown(on);
        let (stopped_db, stopped_wal) = store_sizes(environment.path(), &cache);

        let started = Instant::now();
        let mut off = launch_labelled(environment.path(), "cold-cache", "record-off");
        off.initialize(&root, json!({"persistentCacheEnabled": false}));
        open_disk(&mut off, &root, &OPENS);
        wait_index(&mut off, complete);
        let off_seconds = started.elapsed().as_secs_f64();
        settle_background(&mut off);
        let (off_hwm, ..) = process_peaks(&off);
        if pair + 1 == PAIRS {
            let mut symbols = symbols_with_notes(&mut off, LARGE.shared_name).0;
            symbols.sort_by_key(Value::to_string);
            uncached = Some((references(&mut off, &shared, 2, 6), symbols));
        }
        shutdown(off);

        let ratio = on_seconds / off_seconds;
        let hwm_delta = (on_hwm as f64 - off_hwm as f64) / KIB_PER_MIB;
        ratios.push(ratio);
        deltas.push(hwm_delta);
        emit(json!({
            "record": "cold_pair",
            "pair": pair,
            "units": LARGE.units,
            "on": {
                "seconds_to_persisted": on_seconds,
                "vmhwm_mib": on_hwm as f64 / KIB_PER_MIB,
                "write_bytes": on_written,
                "rchar": on_read,
                "store_running": {"db_bytes": running_db, "wal_bytes": running_wal},
                "store_stopped": {"db_bytes": stopped_db, "wal_bytes": stopped_wal},
                "extracted": done["extracted"],
                "lock_waits": on_status["lockWaits"],
                "maintenance": on_status["counters"]["maintenance"],
            },
            "off": {"seconds_to_complete": off_seconds, "vmhwm_mib": off_hwm as f64 / KIB_PER_MIB},
            "ratio_on_off": ratio,
            "vmhwm_delta_mib": hwm_delta,
        }));
    }

    let cache = format!("cache-{}", PAIRS - 1);
    let started = Instant::now();
    let mut warm = launch_labelled(environment.path(), &cache, "record-warm");
    warm.initialize(&root, Value::Null);
    open_disk(&mut warm, &root, &OPENS);
    let done = wait_index(&mut warm, complete);
    let warm_seconds = started.elapsed().as_secs_f64();
    let counters = metrics(&mut warm);
    let lifetime = delta(counters, [0; 12]);
    assert_eq!(done["extracted"], 0, "{done}");
    let (settled_hwm, ..) = process_peaks(&warm);

    let mut reference_samples = Vec::new();
    let mut symbol_samples = Vec::new();
    let mut first_answers = None;
    for run in 0..RUNS {
        let started = Instant::now();
        let found = references(&mut warm, &shared, 2, 6);
        reference_samples.push(started.elapsed());
        gaps(&mut warm);
        let started = Instant::now();
        let mut symbols = symbols_with_notes(&mut warm, LARGE.shared_name).0;
        symbol_samples.push(started.elapsed());
        if run == 0 {
            symbols.sort_by_key(Value::to_string);
            first_answers = Some((found, symbols));
        }
    }
    assert_eq!(
        first_answers, uncached,
        "warm answers equal the uncached ones"
    );
    settle_background(&mut warm);
    let status = request(&mut warm, "pascal/cacheStatus", json!({}));
    let (hwm, written, read) = process_peaks(&warm);
    shutdown(warm);
    emit(json!({
        "record": "warm",
        "seconds_to_complete": warm_seconds,
        "reused": done["reused"],
        "extracted": done["extracted"],
        "process_lifetime_counters_at_complete": {
            "parses": lifetime[5],
            "payload_read_attempts": lifetime[0],
            "payload_bytes": lifetime[1],
            "stamp_probes": lifetime[3],
            "legacy_hash_bytes": lifetime[4],
            "import_binding_calls": lifetime[8],
            "import_binding_pairs": lifetime[9],
        },
        "cache_status": {"counters": status["counters"], "lock_waits": status["lockWaits"]},
        "references": sample_summary(&reference_samples),
        "symbols": sample_summary(&symbol_samples),
        "vmhwm_after_reuse_mib": settled_hwm as f64 / KIB_PER_MIB,
        "vmhwm_end_mib": hwm as f64 / KIB_PER_MIB,
        "write_bytes": written,
        "rchar": read,
    }));
    emit(json!({
        "record": "summary",
        "pairs": PAIRS,
        "median_ratio_on_off": median(ratios),
        "median_vmhwm_delta_mib": median(deltas),
    }));
}
