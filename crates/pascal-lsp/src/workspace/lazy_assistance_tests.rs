//! l4d-fpyu: hover and completion parse only the imports their lookups go
//! through and that can spell what they look up (D54), and answer as when
//! every import is parsed.

use super::{TEST_EAGER_SNAPSHOT_IMPORTS, TEST_INDEXED_DEPENDENCIES, Workspace, WorkspaceOptions};
use crate::navigation::CompletionOptions;
use lsp_types::{MarkupKind, Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn workspace(root: &Path) -> Workspace {
    Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    )
}

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("source URI")
}

fn write_sources(root: &Path, files: &[(&str, &str)]) {
    for (name, source) in files {
        fs::write(root.join(name), source).expect("fixture source");
    }
}

/// The file names of the dependencies indexed while `run` ran.
fn indexed_while<T>(run: impl FnOnce() -> T) -> (T, Vec<String>) {
    TEST_INDEXED_DEPENDENCIES.with(|indexed| indexed.borrow_mut().clear());
    let value = run();
    let indexed = TEST_INDEXED_DEPENDENCIES.with(|indexed| indexed.borrow_mut().split_off(0));
    let mut names = indexed
        .iter()
        .filter_map(|uri| uri.path_segments()?.next_back().map(str::to_owned))
        .collect::<Vec<_>>();
    names.retain(|name| name != "Main.pas");
    names.sort();
    names.dedup();
    (value, names)
}

/// The same query with every import parsed (the path before D54), over a
/// cache the crawl warmed with `main`'s closure, as a session's is: the
/// eager snapshot binds a dependency's ancestors only through the cached
/// interface closure.
fn eagerly<T>(root: &Path, main: &str, run: impl FnOnce(&Workspace) -> T) -> T {
    let mut warmed = workspace(root);
    warmed
        .crawl_with_cancel(&uri(&root.join(main)), &AtomicBool::new(false))
        .expect("crawl");
    TEST_EAGER_SNAPSHOT_IMPORTS.with(|eager| eager.set(true));
    let value = run(&warmed);
    TEST_EAGER_SNAPSHOT_IMPORTS.with(|eager| eager.set(false));
    value
}

fn hover(root: &Path, main: &str, position: Position) -> Option<String> {
    hover_in(&workspace(root), root, main, position)
}

fn hover_in(workspace: &Workspace, root: &Path, main: &str, position: Position) -> Option<String> {
    super::queries::hover_from_input(
        workspace.analysis_input(),
        &uri(&root.join(main)),
        position,
        MarkupKind::Markdown,
        &AtomicBool::new(false),
    )
    .value
    .expect("hover answers")
    .map(|hover| format!("{:?}", hover.contents))
}

fn completion(root: &Path, main: &str, position: Position) -> Vec<String> {
    completion_in(&workspace(root), root, main, position)
}

fn completion_in(
    workspace: &Workspace,
    root: &Path,
    main: &str,
    position: Position,
) -> Vec<String> {
    let list = super::queries::completion_from_input_with_options(
        workspace.analysis_input(),
        &uri(&root.join(main)),
        position,
        CompletionOptions {
            format: MarkupKind::Markdown,
            defer_documentation: false,
            defer_detail: false,
            snippet_support: false,
        },
        &AtomicBool::new(false),
    )
    .value
    .expect("completion answers")
    .list;
    let mut items = list
        .items
        .iter()
        .map(|item| format!("{} {:?} {:?}", item.label, item.detail, item.documentation))
        .collect::<Vec<_>>();
    items.sort();
    items.push(format!("incomplete: {}", list.is_incomplete));
    items
}

/// `TTarget` binds through `Target`; `Noise` and `Bulky` (and its include)
/// never spell it; `Later` is an implementation import.
fn type_fixture(root: &Path) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Noise, Target, Bulky;\ntype TUser = class\n  F: TTarget;\nend;\nimplementation\nuses Later;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\ntype TNoise = class end;\nimplementation\nend.\n",
            ),
            (
                "Target.pas",
                "unit Target;\ninterface\ntype\n  { The target. }\n  TTarget = class\n    procedure Run;\n  end;\nimplementation\nprocedure TTarget.Run; begin end;\nend.\n",
            ),
            (
                "Bulky.pas",
                "unit Bulky;\ninterface\n{$I Bulky.inc}\nimplementation\nend.\n",
            ),
            ("Bulky.inc", "const BulkyValue = 1;\n"),
            (
                "Later.pas",
                "unit Later;\ninterface\nimplementation\nend.\n",
            ),
        ],
    );
}

#[test]
fn the_first_hover_parses_only_the_imports_that_can_declare_the_name() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    type_fixture(root);
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(4, 6))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(4, 6)));

    assert!(answer.is_some(), "TTarget hovers");
    assert_eq!(answer, expected);
    assert_eq!(indexed, vec!["Target.pas".to_string()], "{indexed:?}");
}

/// A dependency's own import that a member's ancestor binds through.
fn member_fixture(root: &Path) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Noise, Dep, Base, Root;\nimplementation\nprocedure Run;\nvar X: TDep;\nbegin\n  X.RootMember;\n  X.\nend;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\ntype TNoise = class\n  procedure NoiseMember;\nend;\nimplementation\nprocedure TNoise.NoiseMember; begin end;\nend.\n",
            ),
            (
                "Dep.pas",
                "unit Dep;\ninterface\nuses Bad, Base;\ntype TDep = class(TBase)\n  procedure DepMember;\nend;\nimplementation\nprocedure TDep.DepMember; begin end;\nend.\n",
            ),
            (
                "Bad.pas",
                "unit Bad;\ninterface\nuses Root;\nconst BadValue = 1;\nimplementation\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\nuses Root;\ntype TBase = class(TRoot)\n  procedure BaseMember;\nend;\nimplementation\nprocedure TBase.BaseMember; begin end;\nend.\n",
            ),
            (
                "Root.pas",
                "unit Root;\ninterface\ntype TRoot = class\n  procedure RootMember;\nend;\nimplementation\nprocedure TRoot.RootMember; begin end;\nend.\n",
            ),
        ],
    );
}

#[test]
fn a_hover_on_an_inherited_member_answers_as_with_every_import_parsed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    member_fixture(root);
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(7, 5))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(7, 5)));

    assert_eq!(answer, expected);
    assert!(
        !indexed
            .iter()
            .any(|name| name == "Noise.pas" || name == "Bad.pas"),
        "{indexed:?}"
    );
}

#[test]
fn completion_after_a_qualifier_parses_no_unrelated_import() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    member_fixture(root);
    let expected = eagerly(root, "Main.pas", |warmed| {
        completion_in(warmed, root, "Main.pas", Position::new(8, 4))
    });

    let (items, indexed) = indexed_while(|| completion(root, "Main.pas", Position::new(8, 4)));

    assert_eq!(items, expected);
    assert!(
        items.iter().any(|item| item.starts_with("RootMember")),
        "{items:?}"
    );
    assert!(
        !indexed
            .iter()
            .any(|name| name == "Noise.pas" || name == "Bad.pas"),
        "{indexed:?}"
    );
}

/// A helper an import declares for an ancestor adds members.
fn helper_fixture(root: &Path) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Base, Helps, Noise;\nimplementation\nprocedure Run;\nvar X: TThing;\nbegin\n  X.Member;\n  X.\nend;\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TThing = class\n  procedure Member;\nend;\nimplementation\nprocedure TThing.Member; begin end;\nend.\n",
            ),
            (
                "Helps.pas",
                "unit Helps;\ninterface\ntype TObjectHelps = class helper for TObject\n  procedure Member;\n  procedure Helped;\nend;\nimplementation\nprocedure TObjectHelps.Member; begin end;\nprocedure TObjectHelps.Helped; begin end;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\ntype TNoise = class end;\nimplementation\nend.\n",
            ),
        ],
    );
}

#[test]
fn hover_and_completion_with_a_helper_answer_as_with_every_import_parsed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    helper_fixture(root);
    let expected_hover = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(7, 5))
    });
    let expected_items = eagerly(root, "Main.pas", |warmed| {
        completion_in(warmed, root, "Main.pas", Position::new(8, 4))
    });

    let (answer, hover_indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(7, 5)));
    let (items, completion_indexed) =
        indexed_while(|| completion(root, "Main.pas", Position::new(8, 4)));

    assert_eq!(answer, expected_hover);
    assert_eq!(items, expected_items);
    for indexed in [&hover_indexed, &completion_indexed] {
        assert!(
            !indexed.iter().any(|name| name == "Noise.pas"),
            "{indexed:?}"
        );
    }
}

#[test]
fn hover_through_a_unit_alias_answers_as_with_every_import_parsed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    type_fixture(root);
    fs::write(
        root.join("Main.pas"),
        "unit Main;\ninterface\nuses Noise, OldTarget, Bulky;\ntype TUser = class\n  F: TTarget;\nend;\nimplementation\nend.\n",
    )
    .expect("main");
    fs::write(
        root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitAlias>OldTarget=Target</DCC_UnitAlias></PropertyGroup></Project>",
    )
    .expect("project");
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(4, 6))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(4, 6)));

    assert!(answer.is_some(), "TTarget hovers through the alias");
    assert_eq!(answer, expected);
    assert!(
        !indexed.iter().any(|name| name == "Noise.pas"),
        "{indexed:?}"
    );
}

/// Unqualified completion lists the imported names starting with its
/// prefix: only the imports spelling such a name are parsed.
#[test]
fn prefix_completion_parses_only_the_imports_spelling_the_prefix() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Noise, Target, Bulky;\nimplementation\nprocedure Run;\nbegin\n  Targ\nend;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\nprocedure NoiseRun;\nimplementation\nprocedure NoiseRun; begin end;\nend.\n",
            ),
            (
                "Target.pas",
                "unit Target;\ninterface\nprocedure TargetRun;\nconst TargetValue = 1;\nimplementation\nprocedure TargetRun; begin end;\nend.\n",
            ),
            (
                "Bulky.pas",
                "unit Bulky;\ninterface\n{$I Bulky.inc}\nimplementation\nend.\n",
            ),
            ("Bulky.inc", "procedure TargetFromInclude;\n"),
        ],
    );
    let expected = eagerly(root, "Main.pas", |warmed| {
        completion_in(warmed, root, "Main.pas", Position::new(6, 6))
    });

    let (items, indexed) = indexed_while(|| completion(root, "Main.pas", Position::new(6, 6)));

    assert_eq!(items, expected);
    assert!(
        items.iter().any(|item| item.starts_with("TargetRun")),
        "{items:?}"
    );
    assert!(
        !indexed.iter().any(|name| name == "Noise.pas"),
        "{indexed:?}"
    );
    assert!(
        indexed.iter().any(|name| name == "Bulky.pas"),
        "an include spelling the prefix keeps its unit: {indexed:?}"
    );
}

/// A hover answered from the document itself, before any import is looked
/// through, still proves the document's imports resolved: they are bound,
/// none parsed.
#[test]
fn a_hover_on_a_local_name_binds_the_imports_without_parsing_them() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    type_fixture(root);
    fs::write(
        root.join("Main.pas"),
        "unit Main;\ninterface\nuses Noise, Target;\nprocedure Local;\nimplementation\nprocedure Local;\nbegin\n  Local;\nend;\nend.\n",
    )
    .expect("main");
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(7, 3))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(7, 3)));

    assert!(answer.is_some(), "Local hovers");
    assert_eq!(answer, expected);
    assert!(indexed.is_empty(), "{indexed:?}");
}

/// `EndsStr(.., Uppercase(Result.Name))` in a method: the hover parses the
/// units declaring the call's names, never one that only spells `Result`
/// (the function's implicit `Result` shadows every unit-level name).
#[test]
fn a_call_hover_in_a_function_parses_no_unit_for_its_result() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Info;\ntype TEnv = class(TInterfacedObject)\n  function Get: IInfo;\nend;\nimplementation\nuses StrU, SysU, Noise;\nfunction TEnv.Get: IInfo;\nbegin\n  if EndsStr('X', Uppercase(result.Name)) then;\nend;\nend.\n",
            ),
            (
                "Info.pas",
                "unit Info;\ninterface\ntype IInfo = interface\n  function GetName: string;\n  property Name: string read GetName;\nend;\nimplementation\nend.\n",
            ),
            (
                "StrU.pas",
                "unit StrU;\ninterface\nfunction EndsStr(const S, T: string): Boolean;\nimplementation\nfunction EndsStr(const S, T: string): Boolean; begin Result := False; end;\nend.\n",
            ),
            (
                "SysU.pas",
                "unit SysU;\ninterface\nfunction Uppercase(const S: string): string;\nimplementation\nfunction Uppercase(const S: string): string; begin Result := S; end;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\nfunction Other: Integer;\nimplementation\nfunction Other: Integer; begin Result := 0; end;\nend.\n",
            ),
        ],
    );
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(10, 6))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(10, 6)));

    assert!(answer.is_some(), "EndsStr hovers");
    assert_eq!(answer, expected);
    assert!(
        !indexed.iter().any(|name| name == "Noise.pas"),
        "{indexed:?}"
    );
}
