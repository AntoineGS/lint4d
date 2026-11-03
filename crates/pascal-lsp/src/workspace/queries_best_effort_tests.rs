//! D16: read-only features answer best effort when only a dependency's
//! completeness proof fails; reference features keep the strict proof.

use super::{
    completion_from_input, highlights_from_input, hover_from_input, references_from_input,
    signature_help_from_input, type_definitions_from_input,
};
use crate::workspace::rename::WorkspaceInput;
use crate::workspace::{Workspace, WorkspaceOptions};
use lsp_types::{HoverContents, MarkupKind, Position, Url};
use pascal_project::delphi_overrides::OverrideSession;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;

const MAIN: &str = concat!(
    "unit Main;\ninterface\nuses Dep, Broken;\nimplementation\n",
    "procedure Run;\nbegin\n  DepValue;\n  DepCall(1);\nend;\nend.\n",
);
/// `DepValue` in `MAIN`.
const VALUE: Position = Position {
    line: 6,
    character: 4,
};
/// After the `Dep` prefix of `DepValue` in `MAIN`.
const PREFIX: Position = Position {
    line: 6,
    character: 5,
};
/// Inside the argument list of `DepCall(1)`.
const CALL: Position = Position {
    line: 7,
    character: 10,
};

/// Without a selected installation `CompilerVersion` is unknown, so every
/// such `{$IF}` that defines a symbol doubles the compile-time session's
/// paths. Past its branch-state limit the session stops, ignores later
/// directives, and records the unit's analysis as incomplete.
const FORKS: &str = concat!(
    "{$IF CompilerVersion >= 20}{$DEFINE V20}{$IFEND}\n",
    "{$IF CompilerVersion >= 21}{$DEFINE V21}{$IFEND}\n",
    "{$IF CompilerVersion >= 22}{$DEFINE V22}{$IFEND}\n",
    "{$IF CompilerVersion >= 23}{$DEFINE V23}{$IFEND}\n",
    "{$IF CompilerVersion >= 24}{$DEFINE V24}{$IFEND}\n",
    "{$IF CompilerVersion >= 25}{$DEFINE V25}{$IFEND}\n",
    "{$IF CompilerVersion >= 26}{$DEFINE V26}{$IFEND}\n",
    "{$IF CompilerVersion >= 27}{$DEFINE V27}{$IFEND}\n",
);

/// The include after the point where the session stopped has no proven
/// occurrence (R3), and the unit's analysis is incomplete (R5).
fn stopped_before_include() -> String {
    format!("{FORKS}{{$I BrokenInc.inc}}\n")
}

/// The include is proven; only the unit's analysis is incomplete (R5).
fn incomplete_after_include() -> String {
    format!("{{$I BrokenInc.inc}}\n{FORKS}")
}

struct Fixture {
    _temp: TempDir,
    input: WorkspaceInput,
    main: Url,
}

fn dep_source(directives: &str) -> String {
    format!(
        concat!(
            "unit Dep;\ninterface\n{}",
            "procedure DepValue;\nprocedure DepCall(A: Integer);\nimplementation\n",
            "procedure DepValue; begin end;\nprocedure DepCall(A: Integer); begin end;\nend.\n",
        ),
        directives
    )
}

/// `Main` uses `Dep`, which declares what it binds, and `Broken`, whose
/// directives make its proof fail.
fn fixture(main: &str, dep: &str, broken_directives: &str, include: &str) -> Fixture {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fs::write(root.join("Dep.pas"), dep).expect("dependency");
    fs::write(root.join("DepInc.inc"), include).expect("dependency include");
    fs::write(
        root.join("Broken.pas"),
        format!("unit Broken;\ninterface\n{broken_directives}const BrokenValue = 1;\nimplementation\nend.\n"),
    )
    .expect("broken dependency");
    fs::write(root.join("BrokenInc.inc"), include).expect("broken include");
    fs::write(root.join("Main.pas"), main).expect("main");
    let workspace = Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    );
    Fixture {
        input: workspace.analysis_input(),
        main: uri(&root.join("Main.pas")),
        _temp: temp,
    }
}

fn broken_fixture(broken_directives: &str) -> Fixture {
    fixture(
        MAIN,
        &dep_source(""),
        broken_directives,
        "const BrokenIncValue = 2;\n",
    )
}

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("source URI")
}

fn assert_read_only_features_answer(fixture: &Fixture) {
    let cancel = AtomicBool::new(false);
    let hover = hover_from_input(
        fixture.input.clone(),
        &fixture.main,
        VALUE,
        MarkupKind::PlainText,
        &cancel,
    )
    .value
    .expect("hover answers best effort")
    .expect("hover binds DepValue");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("unexpected hover contents: {:?}", hover.contents);
    };
    assert!(markup.value.contains("procedure DepValue"), "{markup:?}");
    assert!(
        markup
            .value
            .contains("Analysis of Broken.pas is incomplete; the result may be shadowed."),
        "the hover must say which dependency was not proven: {markup:?}"
    );

    let completion = completion_from_input(fixture.input.clone(), &fixture.main, PREFIX, &cancel)
        .value
        .expect("completion answers best effort");
    assert!(completion.is_incomplete, "{completion:?}");
    assert!(
        completion.items.iter().any(|item| item.label == "DepValue"),
        "{completion:?}"
    );

    let highlights = highlights_from_input(fixture.input.clone(), &fixture.main, VALUE, &cancel)
        .value
        .expect("highlights answer best effort");
    assert!(!highlights.is_empty(), "{highlights:?}");

    let signature = signature_help_from_input(fixture.input.clone(), &fixture.main, CALL, &cancel)
        .value
        .expect("signature help answers best effort");
    assert!(
        signature.is_some_and(|help| help.signatures[0].label.contains("DepCall")),
        "signature help must bind DepCall"
    );

    type_definitions_from_input(fixture.input.clone(), &fixture.main, VALUE, &cancel)
        .value
        .expect("type definition answers best effort");
}

/// Rename refuses; references either refuse or report the gap. Neither
/// answers best effort.
fn assert_strict_features_keep_the_proof(fixture: &Fixture) {
    let cancel = AtomicBool::new(false);
    let rename = crate::workspace::rename::rename_from_input(
        fixture.input.clone(),
        &fixture.main,
        VALUE,
        "Renamed",
        false,
        &cancel,
    );
    assert!(
        rename.value.is_err(),
        "rename keeps the strict proof: {:?}",
        rename.value
    );
    let references =
        references_from_input(fixture.input.clone(), &fixture.main, VALUE, true, &cancel);
    match references.value {
        Err(_) => {}
        Ok(partial) => assert!(
            !partial.coverage.is_complete(),
            "references must not claim the unproven dependency was searched: {partial:?}"
        ),
    }
}

#[test]
fn an_include_past_where_a_dependency_session_stopped_leaves_read_only_features_answering() {
    let fixture = broken_fixture(&stopped_before_include());
    assert_read_only_features_answer(&fixture);
    assert_strict_features_keep_the_proof(&fixture);
}

#[test]
fn incomplete_conditional_analysis_of_a_dependency_leaves_read_only_features_answering() {
    let fixture = broken_fixture(&incomplete_after_include());
    assert_read_only_features_answer(&fixture);
    assert_strict_features_keep_the_proof(&fixture);
}

#[test]
fn an_include_with_source_in_a_dependency_leaves_assistance_answering() {
    let fixture = fixture(
        MAIN,
        &dep_source("{$I DepInc.inc}\n"),
        "",
        "const DepIncValue = 2;\n",
    );
    let cancel = AtomicBool::new(false);
    let completion = completion_from_input(fixture.input.clone(), &fixture.main, PREFIX, &cancel)
        .value
        .expect("completion answers best effort");
    // The dependency's expansion indexes what the include declares.
    assert!(
        completion
            .items
            .iter()
            .any(|item| item.label == "DepIncValue"),
        "{completion:?}"
    );
    let signature = signature_help_from_input(fixture.input.clone(), &fixture.main, CALL, &cancel)
        .value
        .expect("signature help answers best effort");
    assert!(signature.is_some());
}

#[test]
fn the_requested_documents_own_unproven_include_still_refuses() {
    let main = format!(
        concat!(
            "unit Main;\ninterface\nuses Dep;\n{}implementation\n",
            "procedure Run;\nbegin\n  DepValue;\nend;\nend.\n",
        ),
        stopped_before_include()
    );
    let fixture = fixture(&main, &dep_source(""), "", "const BrokenIncValue = 2;\n");
    let cancel = AtomicBool::new(false);
    let line = u32::try_from(main.lines().count()).expect("line count") - 4;
    let at = Position::new(line, 4);
    let hover = hover_from_input(
        fixture.input.clone(),
        &fixture.main,
        at,
        MarkupKind::PlainText,
        &cancel,
    );
    assert!(hover.value.is_err(), "{:?}", hover.value);
    let completion = completion_from_input(
        fixture.input.clone(),
        &fixture.main,
        Position::new(line, 5),
        &cancel,
    );
    assert!(completion.value.is_err(), "{:?}", completion.value);
}

#[test]
fn a_blocking_include_error_names_the_requested_document_not_a_tolerated_dependency() {
    // `Aaa.pas` sorts first and contains an unsupported directive, which a
    // dependency may have. The error that blocks is the document's own
    // unresolved include.
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fs::write(
        root.join("Aaa.pas"),
        "unit Aaa;\ninterface\n{$P+}\nconst AaaValue = 1;\nimplementation\nend.\n",
    )
    .expect("dependency");
    let main = concat!(
        "unit Main;\ninterface\nuses Aaa;\n{$I Missing.inc}\nimplementation\n",
        "procedure Run;\nbegin\n  AaaValue;\nend;\nend.\n",
    );
    fs::write(root.join("Main.pas"), main).expect("main");
    let workspace = Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    );
    let main = uri(&root.join("Main.pas"));
    let error = completion_from_input(
        workspace.analysis_input(),
        &main,
        Position::new(7, 5),
        &AtomicBool::new(false),
    )
    .value
    .expect_err("the document's own unresolved include blocks completion");
    assert!(error.contains("Main.pas"), "{error}");
    assert!(!error.contains("Aaa.pas"), "{error}");
}

#[test]
fn completion_past_its_byte_budget_returns_a_truncated_incomplete_list() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    // Each item renders its declaration twice (detail and excerpt): 256
    // items of about 1.2 KiB exceed the render budget.
    let parameters = (0..40)
        .map(|parameter| format!("const ParameterWithALongName{parameter:02}: Integer"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut members = String::new();
    for index in 0..256 {
        members.push_str(&format!(
            "    procedure Method{index:03}({});\n",
            parameters
        ));
    }
    fs::write(
        root.join("Big.pas"),
        format!(
            "unit Big;\ninterface\ntype\n  TBig = class\n  public\n{members}  end;\nimplementation\nend.\n"
        ),
    )
    .expect("big unit");
    let main = "unit Main;\ninterface\nuses Big;\nimplementation\nprocedure Run(Value: TBig);\nbegin\n  Value.\nend;\nend.\n";
    fs::write(root.join("Main.pas"), main).expect("main");
    let workspace = Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    );
    let line = u32::try_from(main.lines().count()).expect("line count") - 3;
    let completion = completion_from_input(
        workspace.analysis_input(),
        &uri(&root.join("Main.pas")),
        Position::new(line, 8),
        &AtomicBool::new(false),
    )
    .value
    .expect("completion answers with what fits its byte budget");
    assert!(completion.is_incomplete, "{}", completion.items.len());
    assert!(!completion.items.is_empty());
    assert!(completion.items.len() < 256, "{}", completion.items.len());
}

/// An imported routine called from a method binds even though an inherited
/// member the index cannot see could shadow it (D16; seen on `EndsStr` in a
/// method of a class whose ancestry spans unproven RTL units).
#[test]
fn an_imported_global_called_from_a_method_binds_for_read_only_features() {
    let main = concat!(
        "unit Main;\ninterface\nuses Dep;\ntype\n  TThing = class(TBase)\n    procedure Run;\n  end;\n",
        "implementation\nprocedure TThing.Run;\nbegin\n  DepValue;\nend;\nend.\n",
    );
    let fixture = fixture(main, &dep_source(""), "", "");
    let cancel = AtomicBool::new(false);
    let at = Position::new(10, 4);
    let hover = hover_from_input(
        fixture.input.clone(),
        &fixture.main,
        at,
        MarkupKind::PlainText,
        &cancel,
    )
    .value
    .expect("hover answers");
    assert!(hover.is_some(), "hover must bind DepValue");
    let definition = super::navigation_from_input(
        fixture.input.clone(),
        &fixture.main,
        at,
        crate::NavigationTarget::Definition,
        &cancel,
    )
    .value
    .expect("definition answers")
    .locations;
    assert_eq!(definition.len(), 1, "{definition:?}");
    let rename = crate::workspace::rename::rename_from_input(
        fixture.input.clone(),
        &fixture.main,
        at,
        "Renamed",
        false,
        &cancel,
    );
    assert!(
        rename.value.is_err(),
        "rename keeps the conservative inherited-member model: {:?}",
        rename.value
    );
}
