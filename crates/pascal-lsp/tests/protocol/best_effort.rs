//! D16 over the protocol: read-only features in an importer answer best
//! effort when a dependency's completeness proof fails, and mark it; rename
//! and references keep the strict proof.

use super::*;

const MAIN: &str = concat!(
    "unit Main;\ninterface\nuses Dep, Broken;\nimplementation\n",
    "procedure Run;\nbegin\n  DepValue;\nend;\nend.\n",
);

/// Without a selected installation `CompilerVersion` is unknown; past the
/// branch-state limit the compile-time session of `Broken` stops, so its
/// include has no proven occurrence and its analysis is incomplete. It
/// spells `DepValue`, so it could shadow it and is parsed (D54, l4d-fpyu).
const BROKEN: &str = concat!(
    "unit Broken;\ninterface\n",
    "{$IF CompilerVersion >= 20}{$DEFINE V20}{$IFEND}\n",
    "{$IF CompilerVersion >= 21}{$DEFINE V21}{$IFEND}\n",
    "{$IF CompilerVersion >= 22}{$DEFINE V22}{$IFEND}\n",
    "{$IF CompilerVersion >= 23}{$DEFINE V23}{$IFEND}\n",
    "{$IF CompilerVersion >= 24}{$DEFINE V24}{$IFEND}\n",
    "{$IF CompilerVersion >= 25}{$DEFINE V25}{$IFEND}\n",
    "{$IF CompilerVersion >= 26}{$DEFINE V26}{$IFEND}\n",
    "{$IF CompilerVersion >= 27}{$DEFINE V27}{$IFEND}\n",
    "{$I BrokenInc.inc}\n",
    "const BrokenValue = 1; { DepValue }\nimplementation\nend.\n",
);

fn write_fixture(root: &Path) -> PathBuf {
    write_file(
        &root.join("Dep.pas"),
        "unit Dep;\ninterface\nprocedure DepValue;\nimplementation\nprocedure DepValue; begin end;\nend.\n",
    );
    write_file(&root.join("Broken.pas"), BROKEN);
    write_file(&root.join("BrokenInc.inc"), "const BrokenIncValue = 2;\n");
    let main = root.join("Main.pas");
    write_file(&main, MAIN);
    main
}

#[test]
fn read_only_features_answer_best_effort_over_an_unproven_dependency() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main = write_fixture(temp.path());
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let hover_id = RequestId::from("best-effort-hover".to_string());
    server.send_request(
        hover_id.clone(),
        "textDocument/hover",
        navigation_params(&main, MAIN, "DepValue", 0),
    );
    let hover = server.response(&hover_id);
    assert!(hover.error.is_none(), "hover must answer: {hover:?}");
    let contents = hover.result.expect("hover result")["contents"]["value"]
        .as_str()
        .expect("markup hover")
        .to_string();
    assert!(contents.contains("procedure DepValue"), "{contents}");
    assert!(
        contents.contains("Analysis of Broken.pas is incomplete; the result may be shadowed."),
        "{contents}"
    );

    let completion_id = RequestId::from("best-effort-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_after(MAIN, "  Dep", 0),
        }),
    );
    let completion = server.response(&completion_id);
    assert!(
        completion.error.is_none(),
        "completion must answer: {completion:?}"
    );
    let completion = completion.result.expect("completion result");
    assert_eq!(completion["isIncomplete"], true, "{completion}");
    assert!(
        completion["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .any(|item| item["label"] == "DepValue"),
        "{completion}"
    );

    let definition_id = RequestId::from("best-effort-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, MAIN, "DepValue", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(
        locations[0]["uri"],
        uri(&temp.path().join("Dep.pas")).to_string()
    );

    let highlight_id = RequestId::from("best-effort-highlight".to_string());
    server.send_request(
        highlight_id.clone(),
        "textDocument/documentHighlight",
        navigation_params(&main, MAIN, "DepValue", 0),
    );
    let highlights = server.response(&highlight_id);
    assert!(highlights.error.is_none(), "{highlights:?}");
    server.shutdown();
}

#[test]
fn rename_and_references_keep_the_strict_proof_over_an_unproven_dependency() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main = write_fixture(temp.path());
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);

    let rename_id = RequestId::from("strict-rename".to_string());
    let mut params = navigation_params(&main, MAIN, "DepValue", 0);
    params["newName"] = json!("Renamed");
    server.send_request(rename_id.clone(), "textDocument/rename", params);
    let rename = server.response(&rename_id);
    assert!(rename.error.is_some(), "rename must refuse: {rename:?}");

    let references_id = RequestId::from("strict-references".to_string());
    let mut params = navigation_params(&main, MAIN, "DepValue", 0);
    params["context"] = json!({"includeDeclaration": true});
    server.send_request(references_id.clone(), "textDocument/references", params);
    let references = server.response(&references_id);
    assert!(
        references.error.is_some(),
        "references must refuse: {references:?}"
    );
    server.shutdown();
}
