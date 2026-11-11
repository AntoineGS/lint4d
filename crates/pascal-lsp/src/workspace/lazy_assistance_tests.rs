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
                "unit Main;\n{$X+}\ninterface\nuses Info;\ntype TEnv = class(TInterfacedObject)\n  function Get: IInfo;\nend;\nimplementation\nuses StrU, SysU, Noise;\nfunction TEnv.Get: IInfo;\nbegin\n  if EndsStr('X', Uppercase(result.Name)) then;\nend;\nend.\n",
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
        hover_in(warmed, root, "Main.pas", Position::new(11, 6))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(11, 6)));

    assert!(answer.is_some(), "EndsStr hovers");
    assert_eq!(answer, expected);
    assert!(
        !indexed.iter().any(|name| name == "Noise.pas"),
        "{indexed:?}"
    );
}

/// `EndsStr('X', Uppercase(S))` where `Noise` and `SysU` both declare
/// `Uppercase`, and `Late` may shadow `EndsStr` (l4d-yz96).
fn call_fixture(root: &Path, late: &str) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nimplementation\nuses StrU, Noise, SysU, Late;\nprocedure Run;\nvar S: string;\nbegin\n  if EndsStr('X', Uppercase(S)) then;\nend;\nend.\n",
            ),
            (
                "StrU.pas",
                "unit StrU;\ninterface\nfunction EndsStr(const S, T: string): Boolean;\nimplementation\nfunction EndsStr(const S, T: string): Boolean; begin Result := False; end;\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\nfunction Uppercase(const S: string): string;\nimplementation\nfunction Uppercase(const S: string): string; begin Result := S; end;\nend.\n",
            ),
            (
                "SysU.pas",
                "unit SysU;\ninterface\nfunction Uppercase(const S: string): string;\nimplementation\nfunction Uppercase(const S: string): string; begin Result := S; end;\nend.\n",
            ),
            ("Late.pas", late),
        ],
    );
}

/// The hover of a routine called with arguments shows the routine's
/// declarations whatever its arguments are: a call to a global routine
/// resolves no argument, so no import is parsed for `Uppercase` (l4d-yz96).
#[test]
fn a_call_hover_on_a_global_routine_parses_no_import_for_its_arguments() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    call_fixture(
        root,
        "unit Late;\ninterface\nconst LateValue = 1;\nimplementation\nend.\n",
    );
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(7, 7))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(7, 7)));

    assert!(
        answer
            .as_deref()
            .is_some_and(|text| text.contains("EndsStr")),
        "{answer:?}"
    );
    assert_eq!(answer, expected);
    assert_eq!(indexed, vec!["StrU.pas".to_string()], "{indexed:?}");
}

/// A later import declaring the called routine is still parsed and shown,
/// as with every import parsed (l4d-yz96).
#[test]
fn a_call_hover_still_parses_a_later_import_declaring_the_routine() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    call_fixture(
        root,
        "unit Late;\ninterface\nfunction EndsStr(const S: string; T: Integer): Boolean;\nimplementation\nfunction EndsStr(const S: string; T: Integer): Boolean; begin Result := False; end;\nend.\n",
    );
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(7, 7))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(7, 7)));

    assert!(
        answer
            .as_deref()
            .is_some_and(|text| text.contains("T: Integer")),
        "{answer:?}"
    );
    assert_eq!(answer, expected);
    assert!(indexed.iter().any(|name| name == "Late.pas"), "{indexed:?}");
    assert!(
        !indexed
            .iter()
            .any(|name| name == "Noise.pas" || name == "SysU.pas"),
        "{indexed:?}"
    );
}

/// A call in a method of `TEnv = class(TInterfacedObject)`: the hover looks
/// up the ancestor through every import, and `Noise` spells it (l4d-yz96).
fn ancestor_fixture(root: &Path, noise: &str) {
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses StrU, Noise;\ntype TEnv = class(TInterfacedObject)\n  procedure Run;\nend;\nimplementation\nprocedure TEnv.Run;\nbegin\n  if EndsStr('X', 'Y') then;\nend;\nend.\n",
            ),
            (
                "StrU.pas",
                "unit StrU;\ninterface\nfunction EndsStr(const S, T: string): Boolean;\nimplementation\nfunction EndsStr(const S, T: string): Boolean; begin Result := False; end;\nend.\n",
            ),
            ("Noise.pas", noise),
        ],
    );
}

/// An import naming the ancestor only as a parent class, a type or a
/// qualifier cannot declare it: it is not parsed (l4d-yz96).
#[test]
fn an_import_only_referencing_a_looked_up_name_is_not_parsed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    ancestor_fixture(
        root,
        "unit Noise;\ninterface\ntype\n  { A TInterfacedObject. }\n  TNoise = class(TInterfacedObject)\n    F: TInterfacedObject;\n  end;\n  TNoiseClass = class of TInterfacedObject;\nimplementation\nprocedure Make; begin TInterfacedObject.Create.Free; end;\nend.\n",
    );
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(9, 7))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(9, 7)));

    assert!(
        answer
            .as_deref()
            .is_some_and(|text| text.contains("EndsStr")),
        "{answer:?}"
    );
    assert_eq!(answer, expected);
    assert_eq!(indexed, vec!["StrU.pas".to_string()], "{indexed:?}");
}

/// An import that may declare the looked-up name is parsed: as an
/// enumeration value, or through an include (l4d-yz96).
#[test]
fn an_import_that_may_declare_a_looked_up_name_is_parsed() {
    for (noise, include) in [
        (
            "unit Noise;\ninterface\ntype TKinds = (TInterfacedObject, TOther);\nimplementation\nend.\n",
            None,
        ),
        (
            "unit Noise;\ninterface\ntype TNoise = class(TInterfacedObject) end;\n{$I Noise.inc}\nimplementation\nend.\n",
            Some("type TInterfacedObject = class end;\n"),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        ancestor_fixture(root, noise);
        if let Some(include) = include {
            fs::write(root.join("Noise.inc"), include).expect("include");
        }
        let expected = eagerly(root, "Main.pas", |warmed| {
            hover_in(warmed, root, "Main.pas", Position::new(9, 7))
        });

        let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(9, 7)));

        assert_eq!(answer, expected, "{noise}");
        assert!(
            indexed.iter().any(|name| name == "Noise.pas"),
            "{noise}: {indexed:?}"
        );
    }
}

/// A member's hover still resolves the arguments its overload selection
/// specializes the display with: `Shadow<T>` shows `TOther`, which only an
/// import declares (l4d-yz96).
#[test]
fn a_call_hover_on_a_generic_method_still_selects_from_its_arguments() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nimplementation\nuses Box, Other, Noise;\nprocedure Run;\nvar B: TBox<Integer>; O: TOther;\nbegin\n  B.Shadow(O);\nend;\nend.\n",
            ),
            (
                "Box.pas",
                "unit Box;\ninterface\ntype TBox<T> = class\n  function Shadow<T>(Value: T): T;\nend;\nimplementation\nfunction TBox<T>.Shadow<T>(Value: T): T; begin Result := Value; end;\nend.\n",
            ),
            (
                "Other.pas",
                "unit Other;\ninterface\ntype TOther = class end;\nimplementation\nend.\n",
            ),
            (
                "Noise.pas",
                "unit Noise;\ninterface\ntype TNoise = class end;\nimplementation\nend.\n",
            ),
        ],
    );
    let expected = eagerly(root, "Main.pas", |warmed| {
        hover_in(warmed, root, "Main.pas", Position::new(7, 5))
    });

    let (answer, indexed) = indexed_while(|| hover(root, "Main.pas", Position::new(7, 5)));

    assert!(
        answer
            .as_deref()
            .is_some_and(|text| text.contains("Shadow<T>(Value: TOther): TOther")),
        "{answer:?}"
    );
    assert_eq!(answer, expected);
    assert!(
        !indexed.iter().any(|name| name == "Noise.pas"),
        "{indexed:?}"
    );
}

/// Review F1: completion inside a method of a class whose ancestor has a
/// helper in an import. Resolving `TMy`'s parent type `TDep` as a member of
/// `TMy` selected `TMy`'s helpers again, each turn at a fresh depth: only
/// the work budget stopped it, past a small thread stack. Runs on 1 MiB.
#[test]
fn helper_selection_through_a_methods_parent_type_is_bounded_on_a_small_stack() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().to_path_buf();
    write_sources(
        &root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Dep, Helps;\ntype TMy = class(TDep)\n  procedure Run;\nend;\nimplementation\nprocedure TMy.Run;\nbegin\n  Ba\nend;\nend.\n",
            ),
            (
                "Dep.pas",
                "unit Dep;\ninterface\nuses Base;\ntype TDep = class(TBase)\n  procedure Own;\nend;\nimplementation\nprocedure TDep.Own; begin end;\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TBase = class\n  procedure BaseOwn;\nend;\nimplementation\nprocedure TBase.BaseOwn; begin end;\nend.\n",
            ),
            (
                "Helps.pas",
                "unit Helps;\ninterface\nuses Base;\ntype TBaseHelps = class helper for TBase\n  procedure BaseExtra;\nend;\nimplementation\nprocedure TBaseHelps.BaseExtra; begin end;\nend.\n",
            ),
        ],
    );
    let lazy_root = root.clone();
    let lazy = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || completion(&lazy_root, "Main.pas", Position::new(9, 4)))
        .expect("thread")
        .join()
        .expect("completion returns");
    let expected = eagerly(&root, "Main.pas", |warmed| {
        completion_in(warmed, &root, "Main.pas", Position::new(9, 4))
    });
    assert_eq!(lazy, expected);
}

/// Review F2: resolving an item rebuilds the completion's snapshot and
/// requires every record of the completion in it (equal per
/// `completion_observations_equal`). A rebuild that renders more may parse
/// an import the listing proved absent: the proof's records of the unit and
/// of its include must then equal those the parse records. `Other` (with an
/// include) is proven absent by the lazy completion and parsed by the eager
/// one over the same input.
#[test]
fn an_absence_proofs_records_equal_those_of_a_parse() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Other, Dep;\nimplementation\nprocedure Run;\nvar X: TDep;\nbegin\n  X.\nend;\nend.\n",
            ),
            (
                "Dep.pas",
                "unit Dep;\ninterface\ntype TDep = class\n  procedure Use;\nend;\nimplementation\nprocedure TDep.Use; begin end;\nend.\n",
            ),
            (
                "Other.pas",
                "unit Other;\ninterface\n{$I Other.inc}\ntype TOther = class end;\nimplementation\nend.\n",
            ),
            ("Other.inc", "const OtherValue = 1;\n"),
        ],
    );
    let records = |eager: bool| {
        TEST_EAGER_SNAPSHOT_IMPORTS.with(|flag| flag.set(eager));
        let workspace = workspace(root);
        let (computed, indexed) = indexed_while(|| {
            super::queries::completion_from_input_with_options(
                workspace.analysis_input(),
                &uri(&root.join("Main.pas")),
                Position::new(7, 4),
                CompletionOptions {
                    format: MarkupKind::Markdown,
                    defer_documentation: true,
                    defer_detail: true,
                    snippet_support: false,
                },
                &AtomicBool::new(false),
            )
        });
        TEST_EAGER_SNAPSHOT_IMPORTS.with(|flag| flag.set(false));
        assert_eq!(
            indexed.iter().any(|name| name == "Other.pas"),
            eager,
            "{indexed:?}"
        );
        computed.records
    };
    let proven = records(false);
    let parsed = records(true);

    let of_other = |record: &&super::rename::SourceRecord| {
        record
            .uri
            .path()
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("Other."))
    };
    let proof = proven.iter().filter(of_other).collect::<Vec<_>>();
    assert!(
        proof
            .iter()
            .any(|record| record.uri.path().ends_with("Other.inc")),
        "the proof records the include: {proof:?}"
    );
    for record in proof {
        assert!(
            parsed
                .iter()
                .any(|other| super::queries::completion_observations_equal(record, other)),
            "{record:?} has no equal record in the parse's: {:?}",
            parsed.iter().filter(of_other).collect::<Vec<_>>()
        );
    }
}

/// Review F2: an item of a lazy completion resolves; its rebuild walks with
/// a probe rendering everything and holds every record of the completion,
/// absence proofs included (`Other` through its include).
#[test]
fn completion_resolve_after_a_lazy_completion_resolves() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    write_sources(
        root,
        &[
            (
                "Main.pas",
                "unit Main;\ninterface\nuses Other, Dep;\nimplementation\nprocedure Run;\nvar X: TDep;\nbegin\n  X.\nend;\nend.\n",
            ),
            (
                "Dep.pas",
                "unit Dep;\ninterface\nuses Other, Base;\ntype TDep = class(TBase)\n  /// Uses an other.\n  procedure Use(A: TOther);\nend;\nimplementation\nprocedure TDep.Use(A: TOther); begin end;\nend.\n",
            ),
            (
                "Base.pas",
                "unit Base;\ninterface\ntype TBase = class\n  procedure BaseOwn;\nend;\nimplementation\nprocedure TBase.BaseOwn; begin end;\nend.\n",
            ),
            (
                "Other.pas",
                "unit Other;\ninterface\n{$I Other.inc}\ntype TOther = class end;\nimplementation\nend.\n",
            ),
            ("Other.inc", "const OtherValue = 1;\n"),
        ],
    );
    let workspace = workspace(root);
    let input = workspace.analysis_input();
    let main = uri(&root.join("Main.pas"));
    let position = Position::new(7, 4);
    let cancel = AtomicBool::new(false);
    let (completion, listed) = indexed_while(|| {
        super::queries::completion_from_input_with_options(
            input.clone(),
            &main,
            position,
            CompletionOptions {
                format: MarkupKind::Markdown,
                defer_documentation: true,
                defer_detail: true,
                snippet_support: false,
            },
            &cancel,
        )
    });
    assert!(!listed.iter().any(|name| name == "Other.pas"), "{listed:?}");
    let result = completion.value.as_ref().expect("completion answers");
    let index = result
        .list
        .items
        .iter()
        .position(|item| item.label == "Use")
        .unwrap_or_else(|| panic!("Use is listed: {:?}", result.list.items));
    let seed = &result.seeds[index];

    let metadata = super::queries::completion_metadata_from_input(
        input.clone(),
        &main,
        position,
        input.source_generation,
        input.configuration_generation,
        seed.candidate_uri(),
        seed.candidate_index(),
        MarkupKind::Markdown,
        true,
        true,
        false,
        &completion.records,
        &result.demanded,
        &cancel,
    )
    .value
    .unwrap_or_else(|error| panic!("Use resolves: {error}"));

    assert!(metadata.documentation.is_some(), "{metadata:?}");
}
