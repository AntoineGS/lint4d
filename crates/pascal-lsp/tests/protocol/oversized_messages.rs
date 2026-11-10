//! l4d-zheb: a message over the inbound LSP size limit must never drop the
//! connection, and a huge but valid file within the limit must open.

use super::*;

/// Above the inbound cap (the 16 MiB `maxFileBytes` ceiling at the
/// worst-case JSON escaping expansion plus an envelope, about 97 MiB).
const OVERSIZED_TEXT_BYTES: usize = 100 * 1024 * 1024;

fn padded_unit(name: &str, padding_bytes: usize) -> String {
    let mut source = format!("unit {name};\ninterface\nprocedure Run;\nimplementation\n");
    source.push('{');
    source.push_str(&"x".repeat(padding_bytes));
    source.push_str("}\nprocedure Run;\nbegin\nend;\nend.\n");
    source
}

fn assert_connection_alive(server: &mut TestServer, root: &Path, label: &str) {
    let small = root.join("Small.pas");
    let small_source = "unit Small;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&small, small_source);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&small), "languageId": "pascal", "version": 1, "text": small_source}}),
    );
    let id = RequestId::from(format!("alive-{label}"));
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&small)}}),
    );
    let response = server.response(&id);
    assert!(
        response.error.is_none(),
        "the connection must keep answering after {label}: {response:?}"
    );
    assert!(
        response
            .result
            .as_ref()
            .is_some_and(|value| value.as_array().is_some_and(|symbols| !symbols.is_empty())),
        "the follow-up request must be answered normally after {label}: {response:?}"
    );
}

fn diagnostics_for(server: &mut TestServer, target: &Url) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "no diagnostics published for {target}"
        );
        let published =
            server.notification_with_timeout("textDocument/publishDiagnostics", remaining);
        if published["uri"] == target.as_str() {
            return published;
        }
    }
}

#[test]
fn oversized_did_open_is_refused_and_the_connection_survives() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let huge = root.join("Huge.pas");
    write_file(&huge, "unit Huge;\ninterface\nimplementation\nend.\n");

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 16 * 1024 * 1024}));
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&huge),
            "languageId": "pascal",
            "version": 1,
            "text": padded_unit("Huge", OVERSIZED_TEXT_BYTES),
        }}),
    );
    let published = diagnostics_for(&mut server, &uri(&huge));
    let message = published["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("document rejected") && message.contains("message limit"),
        "an oversized didOpen must reject its document with a diagnostic: {published}"
    );
    assert_connection_alive(&mut server, &root, "an oversized didOpen");
    server.shutdown();
}

#[test]
fn oversized_did_change_rejects_the_open_document_and_the_connection_survives() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&main, source);

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 16 * 1024 * 1024}));
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let _ = diagnostics_for(&mut server, &uri(&main));
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": padded_unit("Main", OVERSIZED_TEXT_BYTES)}]
        }),
    );
    let published = diagnostics_for(&mut server, &uri(&main));
    let message = published["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("document rejected") && message.contains("message limit"),
        "an oversized didChange must reject its document with a diagnostic: {published}"
    );
    assert_connection_alive(&mut server, &root, "an oversized didChange");
    server.shutdown();
}

#[test]
fn oversized_request_is_answered_with_an_error_and_the_connection_survives() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    write_file(
        &root.join("Main.pas"),
        "unit Main;\ninterface\nimplementation\nend.\n",
    );

    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("oversized-request".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "x".repeat(OVERSIZED_TEXT_BYTES)}),
    );
    let response = server.response(&id);
    let error = response
        .error
        .expect("an oversized request must be answered with an error");
    assert!(
        error.message.contains("message limit"),
        "unexpected oversized request error: {error:?}"
    );
    assert_connection_alive(&mut server, &root, "an oversized request");
    server.shutdown();
}

#[test]
fn nine_mebibyte_did_open_within_the_cap_is_analyzed() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let big = root.join("Big.pas");
    let source = padded_unit("Big", 9 * 1024 * 1024);
    write_file(&big, "unit Big;\ninterface\nimplementation\nend.\n");

    let mut server = TestServer::launch();
    server.initialize(&root, json!({"maxFileBytes": 16 * 1024 * 1024}));
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&big), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let published = diagnostics_for(&mut server, &uri(&big));
    assert!(
        published["diagnostics"]
            .as_array()
            .is_some_and(
                |diagnostics| diagnostics.iter().all(|diagnostic| !diagnostic["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("rejected"))
            ),
        "a 9 MiB unit within maxFileBytes must be analyzed, not rejected: {published}"
    );
    let id = RequestId::from("big-symbols".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&big)}}),
    );
    let response = server.response(&id);
    assert!(
        response
            .result
            .as_ref()
            .is_some_and(|value| value.as_array().is_some_and(|symbols| !symbols.is_empty())),
        "the 9 MiB unit must answer document symbols: {response:?}"
    );
    server.shutdown();
}
