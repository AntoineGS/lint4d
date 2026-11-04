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
    .expect("hover answers")
    .expect("hover must bind DepValue");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("unexpected hover contents: {:?}", hover.contents);
    };
    assert!(
        markup
            .value
            .contains("Members inherited by TThing are unknown; the result may be shadowed."),
        "the relaxed binding must say it may be shadowed: {markup:?}"
    );
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

#[test]
fn prepare_call_hierarchy_answers_best_effort_over_an_unproven_dependency() {
    let main = concat!(
        "unit Main;\ninterface\nuses Dep, Broken;\nprocedure Run;\nimplementation\n",
        "procedure Run;\nbegin\n  DepValue;\nend;\nend.\n",
    );
    let fixture = fixture(main, &dep_source(""), &stopped_before_include(), "");
    let items = super::prepare_call_hierarchy_from_input(
        fixture.input.clone(),
        &fixture.main,
        Position::new(3, 11),
        &AtomicBool::new(false),
    )
    .value
    .expect("prepare call hierarchy answers best effort");
    assert!(
        items.is_some_and(|items| items.iter().any(|item| item.name == "Run")),
        "the routine under the cursor is prepared"
    );
}

#[test]
fn semantic_tokens_resolve_best_effort_over_an_unproven_dependency() {
    let fixture = broken_fixture(&stopped_before_include());
    let tokens = super::semantic_tokens_from_input(
        fixture.input.clone(),
        &fixture.main,
        None,
        &AtomicBool::new(false),
    )
    .value
    .expect("semantic tokens answer best effort");
    // `DepValue` (line 6) is classified from its binding, as a function.
    let mut line = 0;
    let mut bound = false;
    for token in &tokens.data {
        line += token.delta_line;
        if line == 6 && token.length == 8 {
            bound = token.token_type != 0 || token.token_modifiers_bitset != 0;
        }
    }
    assert!(
        bound,
        "DepValue must be resolved, not lexical only: {tokens:?}"
    );
}

#[test]
fn a_dependency_without_a_project_context_is_a_coverage_gap() {
    let fixture = broken_fixture("");
    let _fail = FailContext::of("/Broken.pas");
    let hover = hover_from_input(
        fixture.input.clone(),
        &fixture.main,
        VALUE,
        MarkupKind::PlainText,
        &AtomicBool::new(false),
    )
    .value
    .expect("hover answers best effort")
    .expect("hover binds DepValue");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("unexpected hover contents: {:?}", hover.contents);
    };
    assert!(
        markup
            .value
            .contains("Analysis of Broken.pas is incomplete"),
        "{markup:?}"
    );
}

struct FailContext;

impl FailContext {
    fn of(file_name: &str) -> Self {
        crate::workspace::rename::TEST_FAIL_INCLUDE_AUDIT_CONTEXT
            .with(|name| *name.borrow_mut() = Some(file_name.to_string()));
        Self
    }
}

impl Drop for FailContext {
    fn drop(&mut self) {
        crate::workspace::rename::TEST_FAIL_INCLUDE_AUDIT_CONTEXT
            .with(|name| *name.borrow_mut() = None);
    }
}

#[test]
fn the_hover_note_counts_gaps_past_the_retention_limit() {
    let mut coverage = crate::coverage::Coverage::default();
    for index in 0..40 {
        let unit = Url::parse(&format!("file:///workspace/Unit{index:02}.pas")).expect("URI");
        coverage.note(Some(&unit), "conditional analysis is incomplete");
    }
    let note = super::coverage_note(&coverage).expect("note");
    assert!(
        note.contains("Unit00.pas, Unit01.pas, Unit02.pas and 37 more"),
        "{note}"
    );
}

#[test]
fn an_unsupported_directive_in_the_document_blocks_after_error_recording_stopped() {
    let uri = Url::parse("file:///workspace/Main.pas").expect("URI");
    let snapshot = crate::workspace::rename::RenameSnapshot::with_include_errors_for_test(
        vec![
            "rename cannot prove completeness because include error limit (256) was reached".into(),
        ],
        Vec::new(),
        vec![uri.clone()],
    );
    assert!(
        super::blocking_include_error(&snapshot, &uri).is_some(),
        "the document's own unsupported directive still blocks"
    );
}

/// Completion inside a method whose owner's member list ran out of budget
/// cannot know which inherited member shadows a global, so it offers none.
#[test]
fn completion_withholds_globals_when_the_owner_member_lookup_runs_out() {
    let main = concat!(
        "unit Main;\ninterface\nuses Dep;\ntype\n  TThing = class\n    procedure Run;\n  end;\n",
        "implementation\nprocedure TThing.Run;\nbegin\n  DepValue;\nend;\nend.\n",
    );
    let fixture = fixture(main, &dep_source(""), "", "");
    let _exhaust = crate::navigation::test_exhaust_completion_member_lookup();
    let completion = completion_from_input(
        fixture.input.clone(),
        &fixture.main,
        Position::new(10, 5),
        &AtomicBool::new(false),
    )
    .value
    .expect("completion answers");
    assert!(completion.is_incomplete, "{completion:?}");
    assert!(
        completion.items.iter().all(|item| item.label != "DepValue"),
        "{completion:?}"
    );
}

/// l4d-4nbs review C2: `Broken` asks a constant of `Forky`, whose interface
/// exceeds the branch-state limit. The provider's interface is unknown, so
/// `Broken`'s condition is unknown and its analysis incomplete; `Broken`
/// stays imported, and read-only features answer with a note instead of
/// refusing the whole dependency scan.
#[test]
fn a_provider_past_the_branch_state_limit_leaves_its_importer_bound() {
    let fixture = broken_fixture("uses Forky;\n{$IF Forky.FValue = 1}{$DEFINE ONE}{$ENDIF}\n");
    let root = fixture._temp.path();
    fs::write(
        root.join("Forky.pas"),
        format!("unit Forky;\ninterface\n{FORKS}const FValue = 1;\nimplementation\nend.\n"),
    )
    .expect("forking provider");
    assert_read_only_features_answer(&fixture);
    assert_strict_features_keep_the_proof(&fixture);
}

/// l4d-4nbs review (completion parity): the ancestor of a member's type is
/// declared in a unit the cached closure does not hold yet. Completion loads
/// the imports its answer looked through and lists the inherited members,
/// as it did when a wide definition walk had cached the whole closure.
#[test]
fn completion_loads_the_units_an_inherited_member_needs() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    fs::write(
        root.join("Base.pas"),
        "unit Base;\ninterface\ntype\n  TBase = class\n    procedure BaseMember;\n  end;\nimplementation\nprocedure TBase.BaseMember; begin end;\nend.\n",
    )
    .expect("base");
    fs::write(
        root.join("Mid.pas"),
        "unit Mid;\ninterface\nuses Base;\ntype\n  TMid = class(TBase)\n    procedure MidMember;\n  end;\nimplementation\nprocedure TMid.MidMember; begin end;\nend.\n",
    )
    .expect("mid");
    let main = "unit Main;\ninterface\nuses Mid;\nimplementation\nprocedure Run;\nvar X: TMid;\nbegin\n  X.\nend;\nend.\n";
    fs::write(root.join("Main.pas"), main).expect("main");
    let workspace = Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    );
    let completion = completion_from_input(
        workspace.analysis_input(),
        &uri(&root.join("Main.pas")),
        Position::new(7, 4),
        &AtomicBool::new(false),
    )
    .value
    .expect("completion answers");
    let labels = completion
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"MidMember"), "{labels:?}");
    assert!(labels.contains(&"BaseMember"), "{labels:?}");
}

/// A chain `Main -> X: T0`, `T{n} = class(T{n+1})` in unit `U{n}` (which uses
/// `U{n+1}`), each declaring a documented member `M{n}`.
fn ancestor_chain(levels: usize) -> (TempDir, WorkspaceInput, Url, Position) {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    for n in 0..levels {
        let (uses, base) = if n + 1 == levels {
            (String::new(), String::new())
        } else {
            (format!("uses U{};\n", n + 1), format!("(T{})", n + 1))
        };
        fs::write(
            root.join(format!("U{n}.pas")),
            format!(
                "unit U{n};\ninterface\n{uses}type\n  T{n} = class{base}\n    /// Member of level {n}.\n    procedure M{n};\n  end;\nimplementation\nprocedure T{n}.M{n}; begin end;\nend.\n"
            ),
        )
        .expect("chain unit");
    }
    let main = "unit Main;\ninterface\nuses U0;\nimplementation\nprocedure Run;\nvar X: T0;\nbegin\n  X.\nend;\nend.\n";
    fs::write(root.join("Main.pas"), main).expect("main");
    let workspace = Workspace::with_override_session(
        vec![root.to_path_buf()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    );
    let input = workspace.analysis_input();
    let main = uri(&root.join("Main.pas"));
    (temp, input, main, Position::new(7, 4))
}

/// l4d-4nbs re-review: completion found M1 by loading U0's imports in a
/// demand round; resolving each item rebuilds that snapshot, so its
/// observations match and the documentation resolves.
#[test]
fn completion_items_found_by_a_demand_round_resolve() {
    let (_temp, input, main, position) = ancestor_chain(3);
    let cancel = AtomicBool::new(false);
    let completion = super::completion_from_input_with_options(
        input.clone(),
        &main,
        position,
        crate::navigation::CompletionOptions {
            format: MarkupKind::Markdown,
            defer_documentation: true,
            defer_detail: true,
            snippet_support: false,
        },
        &cancel,
    );
    let result = completion.value.expect("completion");
    for member in ["M0", "M1", "M2"] {
        let index = result
            .list
            .items
            .iter()
            .position(|item| item.label == member)
            .unwrap_or_else(|| panic!("{member} is listed: {:?}", result.list.items));
        let seed = &result.seeds[index];
        let metadata = super::completion_metadata_from_input(
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
        .unwrap_or_else(|error| panic!("{member} resolves: {error}"));
        assert!(
            metadata.documentation.is_some(),
            "{member} has documentation"
        );
    }
}

fn chain_completion(levels: usize) -> (Vec<String>, bool, Option<String>) {
    let (_temp, input, main, position) = ancestor_chain(levels);
    let completion = completion_from_input(input, &main, position, &AtomicBool::new(false))
        .value
        .expect("completion answers");
    let labels = completion
        .items
        .iter()
        .map(|item| item.label.clone())
        .filter(|label| label.starts_with('M'))
        .collect();
    let note = completion.items.iter().find_map(|item| {
        item.label
            .starts_with('M')
            .then(|| item.detail.clone())
            .flatten()
    });
    (labels, completion.is_incomplete, note)
}

/// l4d-4nbs re-review (b): past the round cap, completion lists every
/// member the loaded prefix of the chain proves, marked incomplete.
#[test]
fn past_the_demand_round_cap_completion_lists_the_loaded_prefix() {
    let (labels, incomplete, _) = chain_completion(10);
    for member in ["M0", "M1", "M2", "M3", "M4", "M5"] {
        assert!(
            labels.iter().any(|label| label == member),
            "{member}: {labels:?}"
        );
    }
    assert!(incomplete, "the rest of the chain is not loaded");
}

/// l4d-4nbs re-review (a): demand rounds are bounded by the definition
/// walk's byte limit; past it the answer is incomplete.
#[test]
fn demand_rounds_stop_at_the_walk_byte_limit() {
    crate::workspace::TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(Some(1)));
    let (labels, incomplete, _) = chain_completion(4);
    crate::workspace::TEST_NAVIGATION_WALK_BYTES.with(|limit| limit.set(None));
    assert!(labels.iter().any(|label| label == "M0"), "{labels:?}");
    assert!(!labels.iter().any(|label| label == "M3"), "{labels:?}");
    assert!(incomplete);
}

/// l4d-4nbs re-review (c): a demanded unit whose imports fail to load is a
/// coverage gap of the answer, not skipped silently.
#[test]
fn a_demand_round_load_failure_is_noted() {
    crate::workspace::TEST_FAIL_IMPORTS_OF.with(|name| *name.borrow_mut() = Some("/U1.pas".into()));
    let (_temp, input, main, position) = ancestor_chain(4);
    let completion = completion_from_input(input, &main, position, &AtomicBool::new(false))
        .value
        .expect("completion answers");
    crate::workspace::TEST_FAIL_IMPORTS_OF.with(|name| *name.borrow_mut() = None);
    assert!(completion.is_incomplete, "{completion:?}");
    let labels = completion
        .items
        .iter()
        .map(|i| i.label.as_str())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"M1"), "{labels:?}");
    let coverage = super::completion_coverage_for_test();
    assert!(
        coverage.iter().any(|gap| gap.contains("U1.pas")),
        "the failure is noted: {coverage:?}"
    );
}

/// l4d-4nbs final round: a completion whose observations would not fit a
/// resolution context is rendered eagerly from the snapshot its demand
/// rounds built: no second computation.
#[test]
fn an_eager_completion_reuses_its_snapshot() {
    let (_temp, input, main, position) = ancestor_chain(3);
    let cancel = AtomicBool::new(false);
    let options = crate::navigation::CompletionOptions {
        format: MarkupKind::Markdown,
        defer_documentation: true,
        defer_detail: true,
        snippet_support: false,
    };
    let count = || super::TEST_ASSISTANCE_SNAPSHOTS.with(std::cell::Cell::get);
    let before = count();
    let deferred = super::completion_from_input_with_options(
        input.clone(),
        &main,
        position,
        options.clone(),
        &cancel,
    );
    let rounds = count() - before;
    // A fresh workspace: the first one's cached closure now holds the chain.
    let (_temp, input, main, position) = ancestor_chain(3);
    let before = count();
    let eager = super::completion_from_input_within(
        input,
        &main,
        position,
        options,
        &|_, _| false,
        &cancel,
    );
    assert_eq!(count() - before, rounds, "one computation per request");
    let deferred = deferred.value.expect("deferred completion");
    let eager = eager.value.expect("eager completion");
    assert!(eager.eager);
    assert_eq!(
        eager
            .list
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>(),
        deferred
            .list
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    let m1 = eager
        .list
        .items
        .iter()
        .find(|item| item.label == "M1")
        .expect("M1");
    assert!(m1.documentation.is_some(), "rendered eagerly: {m1:?}");
}
