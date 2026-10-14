#[cfg(feature = "test-support")]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
#[cfg(feature = "test-support")]
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use lsp_types::Url;
use serde_json::{Value, json};

use common::*;

#[path = "barriers_workspace_selection.rs"]
mod workspace_selection;
#[path = "workspace_selection_support.rs"]
mod workspace_selection_support;

#[cfg(feature = "test-support")]
#[test]
fn queued_compiled_content_and_symbols_reject_changed_importer_and_project() {
    let environment = tempfile::tempdir().expect("server environment");
    let root = environment.path().join("workspace");
    let lib = root.join("lib");
    fs::create_dir_all(&lib).expect("library directory");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu");
    fs::copy(fixture, lib.join("Lint4dFixture.Classes.dcu")).expect("D13 fixture");
    let main = root.join("Consumer.pas");
    let source = "unit Consumer;\ninterface\nuses Lint4dFixture.Classes;\ntype TAlias = TSimpleClass;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    barrier.release();
    let definition_id = RequestId::from("establish-compiled-binding".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        json!({"textDocument": {"uri": uri(&main)}, "position": {"line": 3, "character": 16}}),
    );
    let definition = server.response(&definition_id);
    assert!(
        definition.error.is_none(),
        "initial binding failed: {definition:?}"
    );
    let virtual_uri = definition.result.expect("definition")[0]["uri"]
        .as_str()
        .expect("virtual URI")
        .to_owned();
    fs::remove_file(&barrier.release).expect("re-arm navigation barrier");
    fs::remove_file(&barrier.entered).expect("reset navigation entry count");

    for number in 0..3 {
        let other = root.join(format!("Blocker{number}.pas"));
        write_file(
            &other,
            &format!(
                "unit Blocker{number};\ninterface\ntype TLocal = Integer;\nimplementation\nend.\n"
            ),
        );
        server.send_request(
            RequestId::from(format!("compiled-queue-blocker-{number}")),
            "textDocument/definition",
            json!({"textDocument": {"uri": uri(&other)}, "position": {"line": 2, "character": 7}}),
        );
    }
    barrier.wait_for_entries(3);
    let queued_content = RequestId::from("queued-compiled-content".to_string());
    let queued_symbols = RequestId::from("queued-compiled-symbols".to_string());
    server.send_request(
        queued_content.clone(),
        "textDocument/content",
        json!({"textDocument": {"uri": virtual_uri}}),
    );
    server.send_request(
        queued_symbols.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": virtual_uri}}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": "unit Consumer;\ninterface\nimplementation\nend.\n"}]
        }),
    );
    // This inline response fences the preceding notification in the protocol
    // stream while all analysis workers remain blocked.
    let fence_id = RequestId::from("compiled-queue-overlay-fence".to_string());
    server.send_request(
        fence_id.clone(),
        "workspace/willCreateFiles",
        json!({"files": []}),
    );
    let fence = server.response(&fence_id);
    assert!(fence.error.is_none(), "overlay fence failed: {fence:?}");
    barrier.release();

    for request_id in [queued_content, queued_symbols] {
        let response = server.response(&request_id);
        assert!(
            response.error.is_some(),
            "stale queued virtual result: {response:?}"
        );
    }

    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&main), "version": 3}, "contentChanges": [{"text": source}]}),
    );
    let rebound_id = RequestId::from("rebind-after-compiled-overlay-change".to_string());
    server.send_request(
        rebound_id.clone(),
        "textDocument/definition",
        json!({"textDocument": {"uri": uri(&main)}, "position": {"line": 3, "character": 16}}),
    );
    let rebound = server.response(&rebound_id);
    assert!(rebound.error.is_none(), "rebind failed: {rebound:?}");
    assert_eq!(
        rebound.result.expect("rebound location")[0]["uri"],
        virtual_uri
    );
    fs::remove_file(&barrier.release).expect("re-arm project-change barrier");
    fs::remove_file(&barrier.entered).expect("reset project-change barrier count");
    for number in 3..6 {
        let other = root.join(format!("Blocker{number}.pas"));
        write_file(
            &other,
            &format!(
                "unit Blocker{number};\ninterface\ntype TLocal = Integer;\nimplementation\nend.\n"
            ),
        );
        server.send_request(
            RequestId::from(format!("project-queue-blocker-{number}")),
            "textDocument/definition",
            json!({"textDocument": {"uri": uri(&other)}, "position": {"line": 2, "character": 7}}),
        );
    }
    barrier.wait_for_entries(3);
    let queued_project_content = RequestId::from("queued-project-compiled-content".to_string());
    let queued_project_symbols = RequestId::from("queued-project-compiled-symbols".to_string());
    server.send_request(
        queued_project_content.clone(),
        "textDocument/content",
        json!({"textDocument": {"uri": virtual_uri}}),
    );
    server.send_request(
        queued_project_symbols.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": virtual_uri}}),
    );
    let project = root.join("App.dproj");
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource><DCC_UnitSearchPath>elsewhere</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&project), "type": 2}]}),
    );
    let project_fence_id = RequestId::from("compiled-queue-project-fence".to_string());
    server.send_request(
        project_fence_id.clone(),
        "workspace/willCreateFiles",
        json!({"files": []}),
    );
    let project_fence = server.response(&project_fence_id);
    assert!(
        project_fence.error.is_none(),
        "project fence failed: {project_fence:?}"
    );
    barrier.release();
    for request_id in [queued_project_content, queued_project_symbols] {
        let response = server.response(&request_id);
        assert!(
            response.error.is_some(),
            "stale queued project result: {response:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_compiled_navigation_rejects_same_size_restored_mtime_dcu_change() {
    let temp = tempfile::tempdir().expect("isolated workspace");
    let root = temp.path();
    let lib = root.join("lib");
    fs::create_dir_all(&lib).expect("library directory");
    let dcu = lib.join("Lint4dFixture.Classes.dcu");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu");
    fs::copy(&fixture, &dcu).expect("real D13 Win64 fixture");
    let main = root.join("Consumer.pas");
    write_file(
        &main,
        "unit Consumer;\ninterface\nuses Lint4dFixture.Classes;\ntype TAlias = TSimpleClass;\nimplementation\nend.\n",
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    let (mut server, barrier) =
        TestServer::launch_with_partial_validation_barrier(tempfile::tempdir().unwrap());
    server.initialize(root, Value::Null);
    let id = RequestId::from("queued-compiled-navigation".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": {"line": 3, "character": 16}
        }),
    );
    barrier.wait_until_entered();

    let original_metadata = fs::metadata(&dcu).expect("DCU metadata");
    let mut replacement = fs::read(&dcu).expect("DCU bytes");
    *replacement.last_mut().expect("nonempty fixture") ^= 1;
    fs::write(&dcu, replacement).expect("same-size DCU replacement");
    restore_mtime(&dcu, &original_metadata);
    barrier.release();

    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "queued definition must be discarded after the DCU bytes change with restored mtime: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn type_hierarchy_rejects_queued_overlay_change_and_unknown_parent() {
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().join("FreshTypes.pas");
    let source = "unit FreshTypes; interface type TBase = class end; TChild = class(TBase) end; TUnknown = class(TMissing) end; implementation end.";
    write_file(&path, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(path.parent().expect("workspace root"), json!({}));
    let uri_value = uri(&path);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri_value, "languageId": "pascal", "version": 1, "text": source
        }}),
    );
    let prepare_id = RequestId::from("type-fresh-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareTypeHierarchy",
        json!({
            "textDocument": {"uri": uri(&path)}, "position": position_of(source, "TChild", 0)
        }),
    );
    let item = server.response(&prepare_id).result.expect("prepared item")[0].clone();
    let query_id = RequestId::from("type-fresh-children".to_string());
    server.send_request(
        query_id.clone(),
        "typeHierarchy/subtypes",
        json!({"item": item}),
    );
    barrier.wait_until_entered();
    let changed = source.replace("TChild", "TChanged");
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri_value, "version": 2}, "contentChanges": [{"text": changed}]
        }),
    );
    barrier.release();
    let response = server.response(&query_id);
    assert!(
        response.error.is_some(),
        "stale result should be rejected: {response:?}"
    );

    let unknown_id = RequestId::from("type-unknown-prepare".to_string());
    server.send_request(
        unknown_id.clone(),
        "textDocument/prepareTypeHierarchy",
        json!({
            "textDocument": {"uri": uri(&path)}, "position": position_of(&changed, "TUnknown", 0)
        }),
    );
    let unknown = server
        .response(&unknown_id)
        .result
        .expect("unknown type item")[0]
        .clone();
    let parent_id = RequestId::from("type-unknown-parent".to_string());
    server.send_request(
        parent_id.clone(),
        "typeHierarchy/supertypes",
        json!({"item": unknown}),
    );
    let parent = server.response(&parent_id);
    assert!(
        parent.error.is_none(),
        "unknown parent query should fail closed without exposing an edge: {parent:?}"
    );
    let no_parent_result = match parent.result.as_ref() {
        None => true,
        Some(result) => result.is_null(),
    };
    assert!(
        no_parent_result,
        "unresolved parent is not an edge: {parent:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn type_hierarchy_rejects_queued_project_owner_change() {
    let root = tempfile::tempdir().expect("workspace");
    let project = root.path().join("App.dproj");
    let provider = root.path().join("Provider.pas");
    let other = root.path().join("Other.pas");
    let owner_source = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>ONE</DCC_Define></PropertyGroup></Project>";
    write_file(&project, owner_source);
    let provider_source = "unit Provider; interface type TBase = class end; implementation end.";
    write_file(&provider, provider_source);
    write_file(
        &root.path().join("Main.pas"),
        "program Main; uses Provider; type TChild = class(TBase) end; begin end.",
    );
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(
        project.parent().expect("project root"),
        json!({"projectFile": "App.dproj"}),
    );
    let prepare_id = RequestId::from("type-owner-prepare".to_string());
    server.send_request(prepare_id.clone(), "textDocument/prepareTypeHierarchy", json!({
        "textDocument": {"uri": uri(&provider)}, "position": position_of(provider_source, "TBase", 0)
    }));
    let prepared = server.response(&prepare_id);
    assert!(
        prepared.error.is_none(),
        "project parent prepare failed: {prepared:?}"
    );
    let item = prepared.result.expect("parent type item")[0].clone();
    let id = RequestId::from("type-owner-subtypes".to_string());
    server.send_request(id.clone(), "typeHierarchy/subtypes", json!({"item": item}));
    barrier.wait_until_entered();
    write_file(
        &project,
        &owner_source
            .replace("Main.pas", "Other.pas")
            .replace("ONE", "TWO"),
    );
    write_file(&other, "program Other; begin end.");
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.result.is_none(),
        "stale project subtype result must not publish: {response:?}"
    );
    let error = response.error.expect("project read-set invalidation");
    assert_eq!(error.code, -32803);
    assert!(
        error
            .message
            .contains("workspace metadata or membership changed"),
        "unexpected freshness error: {error:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn type_hierarchy_cancellation_discards_computed_subtypes() {
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().join("CancelledTypes.pas");
    let source = "unit CancelledTypes; interface type TBase = class end; TChild = class(TBase) end; implementation end.";
    write_file(&path, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(path.parent().expect("workspace root"), json!({}));
    let prepare_id = RequestId::from("type-cancel-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareTypeHierarchy",
        json!({
            "textDocument": {"uri": uri(&path)}, "position": position_of(source, "TBase", 0)
        }),
    );
    let item = server.response(&prepare_id).result.expect("base item")[0].clone();
    let id = RequestId::from("type-cancel-subtypes".to_string());
    server.send_request(id.clone(), "typeHierarchy/subtypes", json!({"item": item}));
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": id}));
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.result.is_none(),
        "cancelled subtype result must not publish: {response:?}"
    );
    assert!(
        response.error.is_some(),
        "cancelled request should fail: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn call_hierarchy_result_is_rejected_when_open_overlay_changes_before_delivery() {
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().join("FreshCalls.pas");
    let source = "unit FreshCalls;\ninterface\nfunction Target: Integer;\nfunction Caller: Integer;\nimplementation\nfunction Target: Integer;\nbegin Result := 1; end;\nfunction Caller: Integer;\nbegin Result := Target(); end;\nend.\n";
    write_file(&path, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(path.parent().expect("workspace root"), json!({}));
    let uri_value = uri(&path);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri_value, "languageId": "pascal", "version": 1, "text": source
        }}),
    );
    let prepare_id = RequestId::from("fresh-caller-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareCallHierarchy",
        json!({
            "textDocument": {"uri": uri(&path)}, "position": position_of(source, "Caller", 0)
        }),
    );
    let prepare = server.response(&prepare_id);
    assert!(prepare.error.is_none(), "prepare failed: {prepare:?}");
    let item = prepare.result.expect("caller item")[0].clone();

    let outgoing_id = RequestId::from("fresh-outgoing".to_string());
    server.send_request(
        outgoing_id.clone(),
        "callHierarchy/outgoingCalls",
        json!({"item": item}),
    );
    barrier.wait_until_entered();
    let changed = source.replace("Target()", "Target(1)");
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri_value, "version": 2},
            "contentChanges": [{"text": changed}]
        }),
    );
    barrier.release();
    let outgoing = server.response(&outgoing_id);
    assert!(
        outgoing.result.is_none(),
        "stale call edges must not be published: {outgoing:?}"
    );
    assert!(outgoing.error.is_some(), "stale request should fail closed");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn call_hierarchy_cancellation_discards_completed_edges() {
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().join("CancelledCalls.pas");
    let source = "unit CancelledCalls;\ninterface\nfunction Target: Integer;\nfunction Caller: Integer;\nimplementation\nfunction Target: Integer;\nbegin Result := 1; end;\nfunction Caller: Integer;\nbegin Result := Target(); end;\nend.\n";
    write_file(&path, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(path.parent().expect("workspace root"), json!({}));
    let prepare_id = RequestId::from("cancelled-caller-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareCallHierarchy",
        json!({
            "textDocument": {"uri": uri(&path)}, "position": position_of(source, "Caller", 0)
        }),
    );
    let prepare = server.response(&prepare_id);
    assert!(prepare.error.is_none(), "prepare failed: {prepare:?}");
    let item = prepare.result.expect("caller item")[0].clone();
    let id = RequestId::from("cancelled-call-edges".to_string());
    server.send_request(
        id.clone(),
        "callHierarchy/outgoingCalls",
        json!({"item": item}),
    );
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": id}));
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.result.is_none(),
        "cancelled edges must not publish: {response:?}"
    );
    assert!(response.error.is_some(), "cancellation should be reported");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn call_hierarchy_result_is_rejected_after_queued_disk_content_change() {
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().join("DiskFreshCalls.pas");
    let source = "unit DiskFreshCalls;\ninterface\nfunction Target: Integer;\nfunction Caller: Integer;\nimplementation\nfunction Target: Integer;\nbegin Result := 1; end;\nfunction Caller: Integer;\nbegin Result := Target(); end;\nend.\n";
    write_file(&path, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(path.parent().expect("workspace root"), json!({}));
    let prepare_id = RequestId::from("disk-fresh-caller-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareCallHierarchy",
        json!({
            "textDocument": {"uri": uri(&path)}, "position": position_of(source, "Caller", 0)
        }),
    );
    let prepare = server.response(&prepare_id);
    assert!(prepare.error.is_none(), "prepare failed: {prepare:?}");
    let item = prepare.result.expect("caller item")[0].clone();
    let id = RequestId::from("disk-fresh-outgoing".to_string());
    server.send_request(
        id.clone(),
        "callHierarchy/outgoingCalls",
        json!({"item": item}),
    );
    barrier.wait_until_entered();
    write_file(&path, &source.replace("Target()", "Target(1)"));
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.result.is_none(),
        "stale disk edges must not publish: {response:?}"
    );
    let error = response
        .error
        .expect("disk freshness failure should be reported");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("closed source changed")
            && error.message.contains("retry the request"),
        "expected disk read-set invalidation, got: {error:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn call_hierarchy_result_is_rejected_after_queued_project_owner_change() {
    let root = tempfile::tempdir().expect("workspace");
    let project = root.path().join("App.dproj");
    let provider = root.path().join("Provider.pas");
    let main = root.path().join("Main.pas");
    let other = root.path().join("Other.pas");
    let owner_source = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>ONE</DCC_Define></PropertyGroup></Project>";
    write_file(&project, owner_source);
    write_file(
        &provider,
        "unit Provider;\ninterface\nfunction Target: Integer;\nimplementation\nfunction Target: Integer;\nbegin Result := 1; end;\nend.\n",
    );
    write_file(
        &main,
        "program Main;\nuses Provider;\nfunction Caller: Integer;\nbegin Result := Target(); end;\nbegin end.\n",
    );
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize(
        project.parent().expect("project root"),
        json!({"projectFile": "App.dproj"}),
    );
    let prepare_id = RequestId::from("owner-target-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareCallHierarchy",
        json!({
            "textDocument": {"uri": uri(&provider)}, "position": {"line": 2, "character": 10}
        }),
    );
    let prepare = server.response(&prepare_id);
    assert!(
        prepare.error.is_none(),
        "project target prepare failed: {prepare:?}"
    );
    let item = prepare.result.expect("target item")[0].clone();
    let id = RequestId::from("owner-incoming".to_string());
    server.send_request(
        id.clone(),
        "callHierarchy/incomingCalls",
        json!({"item": item}),
    );
    barrier.wait_until_entered();
    write_file(
        &project,
        &owner_source
            .replace("Main.pas", "Other.pas")
            .replace("ONE", "TWO"),
    );
    write_file(&other, "program Other; begin end.\n");
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.result.is_none(),
        "old project-owner edges must not publish: {response:?}"
    );
    let error = response
        .error
        .expect("project ownership freshness failure should be reported");
    assert_eq!(error.code, -32803);
    assert!(
        error
            .message
            .contains("workspace metadata or membership changed")
            && error.message.contains("retry the request"),
        "expected project read-set invalidation, got: {error:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn document_links_revalidate_resource_bytes_after_restored_stamp_replacement() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace root");
    let main = root.join("Main.pas");
    let resource = root.join("Main.dfm");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$R Main.dfm}\nend.\n",
    );
    write_file(&resource, "object A: TForm1\nend\n");
    let original_stamp = fs::metadata(&resource).expect("resource metadata");
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let id = RequestId::from("document-links-resource-changed".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    write_file(&resource, "object B: TForm1\nend\n");
    restore_mtime(&resource, &original_stamp);
    barrier.release();
    let response = server.response(&id);
    assert_eq!(
        response
            .error
            .expect("changed resource must stale the link")
            .code,
        -32803
    );
    server.shutdown();
}

#[cfg(all(feature = "test-support", target_os = "linux"))]
#[test]
fn document_links_stale_after_same_stamp_include_target_replacement() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let target = root.join("Selected.inc");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I Selected.inc}\nend.\n",
    );
    write_file(&target, "const Selected = 1;\n");
    let target_metadata = fs::metadata(&target).expect("target metadata");

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("document-links-same-stamp-target".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    write_file(&target, "const Selected = 2;\n");
    restore_mtime(&target, &target_metadata);
    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("same-size include target replacement must stale the link result");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("retry the request"),
        "replacement should be rejected as stale: {}",
        error.message
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn document_links_stale_after_selected_include_opens_as_overlay() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let target = root.join("Selected.inc");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I Selected.inc}\nend.\n",
    );
    write_file(&target, "const Selected = 1;\n");

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("document-links-target-overlay".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&target), "languageId": "pascal", "version": 1,
            "text": "const Selected = 2;\n"
        }}),
    );
    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("new target overlay must stale the link result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn document_links_stale_after_selected_include_overlay_changes() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let target = root.join("Selected.inc");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I Selected.inc}\nend.\n",
    );
    let original_target = "const Selected = 1;\n";
    write_file(&target, original_target);

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&target), "languageId": "pascal", "version": 1,
            "text": original_target
        }}),
    );
    let request_id = RequestId::from("document-links-target-overlay-change".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&target), "version": 2},
            "contentChanges": [{"text": "const Selected = 2;\n"}]}),
    );
    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("changed selected-target overlay must stale the link result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(all(feature = "test-support", target_os = "linux"))]
#[test]
fn document_links_reject_a_new_higher_priority_include_candidate() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let library = root.join("lib");
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let lower_priority_target = library.join("Selected.inc");
    let stale_directory_entry = root.join("Shadowed.inc");
    let higher_priority_target = root.join("Selected.inc");
    fs::create_dir_all(&library).expect("include search directory");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I Selected.inc}\nend.\n",
    );
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>lib</DCC_IncludePath></PropertyGroup></Project>",
    );
    write_file(&lower_priority_target, "const Selected = 1;\n");
    write_file(&stale_directory_entry, "const Shadowed = 1;\n");
    let root_metadata = fs::metadata(&root).expect("workspace directory metadata");

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("document-links-new-priority-candidate".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    fs::rename(&stale_directory_entry, &higher_priority_target)
        .expect("replace unrelated entry with higher-priority include candidate");
    restore_mtime(&root, &root_metadata);
    barrier.release();

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("new higher-priority candidate must stale the old link target");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("retry the request"),
        "candidate appearance should invalidate the result: {}",
        error.message
    );

    let retry_id = RequestId::from("document-links-new-priority-candidate-retry".to_string());
    server.send_request(
        retry_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let retry = server.response(&retry_id);
    assert!(retry.error.is_none(), "fresh retry failed: {retry:?}");
    let links = retry.result.expect("fresh document links");
    assert_eq!(links.as_array().expect("links").len(), 1);
    assert_eq!(links[0]["target"], uri(&higher_priority_target).as_str());
    server.shutdown();
}

#[cfg(all(feature = "test-support", target_os = "linux"))]
#[test]
fn document_links_reject_case_variant_higher_priority_candidate_with_restored_directory_stamp() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let library = root.join("lib");
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let old_target = library.join("Selected.inc");
    let noise = root.join("Shadowed.inc");
    let new_target = root.join("selected.inc");
    fs::create_dir_all(&library).expect("include search directory");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I Selected.inc}\nend.\n",
    );
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>lib</DCC_IncludePath></PropertyGroup></Project>",
    );
    write_file(&old_target, "const Selected = 1;\n");
    write_file(&noise, "const Shadowed = 1;\n");
    let metadata = fs::metadata(&root).expect("workspace directory metadata");

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let id = RequestId::from("document-links-case-variant-candidate".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    fs::rename(&noise, &new_target).expect("replace unrelated entry with case-variant candidate");
    restore_mtime(&root, &metadata);
    barrier.release();
    let response = server.response(&id);
    assert_eq!(
        response
            .error
            .expect("case-variant target must stale the link")
            .code,
        -32803
    );

    let retry = RequestId::from("document-links-case-variant-retry".to_string());
    server.send_request(
        retry.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let response = server.response(&retry);
    assert!(response.error.is_none(), "retry failed: {response:?}");
    assert_eq!(
        response.result.expect("fresh links")[0]["target"],
        uri(&new_target).as_str()
    );
    server.shutdown();
}

#[cfg(all(feature = "test-support", unix))]
#[test]
fn document_links_reject_an_ancestor_symlink_inserted_while_delivery_waits() {
    use std::os::unix::fs::symlink;

    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let assets = root.join("assets");
    fs::create_dir_all(&assets).expect("asset directory");
    let main = root.join("Main.pas");
    let old_target = assets.join("Selected.inc");
    let external_dir = environment.path().join("external");
    let external_target = external_dir.join("Selected.inc");
    fs::create_dir_all(&external_dir).expect("external directory");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I assets/Selected.inc}\nend.\n",
    );
    write_file(&old_target, "const Selected = 1;\n");
    write_file(&external_target, "const Selected = 1;\n");
    let old_target_metadata = fs::metadata(&old_target).expect("old include metadata");
    restore_mtime(&external_target, &old_target_metadata);
    let root_metadata = fs::metadata(&root).expect("workspace metadata");

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let id = RequestId::from("document-links-ancestor-symlink".to_string());
    server.send_request(
        id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    fs::rename(&assets, external_dir.join("previous-assets")).expect("move previous assets away");
    symlink(&external_dir, &assets).expect("external replacement directory");
    restore_mtime(&root, &root_metadata);
    barrier.release();
    let response = server.response(&id);
    assert_eq!(
        response
            .error
            .expect("ancestor symlink must stale the link")
            .code,
        -32803
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn document_links_reject_a_new_higher_priority_include_overlay() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let library = root.join("lib");
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let lower_priority_target = library.join("Selected.inc");
    let higher_priority_target = root.join("Selected.inc");
    fs::create_dir_all(&library).expect("include search directory");
    write_file(
        &main,
        "unit Main;\ninterface\nimplementation\n{$I Selected.inc}\nend.\n",
    );
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>lib</DCC_IncludePath></PropertyGroup></Project>",
    );
    write_file(&lower_priority_target, "const Selected = 1;\n");

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("document-links-new-priority-overlay".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&higher_priority_target), "languageId": "pascal", "version": 1,
            "text": "const Selected = 2;\n"
        }}),
    );
    barrier.release();

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("new higher-priority overlay must stale the old link target");
    assert_eq!(error.code, -32803);
    assert!(error.message.contains("retry the request"));

    let retry_id = RequestId::from("document-links-new-priority-overlay-retry".to_string());
    server.send_request(
        retry_id.clone(),
        "textDocument/documentLink",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let retry = server.response(&retry_id);
    assert!(retry.error.is_none(), "fresh retry failed: {retry:?}");
    let links = retry.result.expect("fresh document links");
    assert_eq!(links.as_array().expect("links").len(), 1);
    assert_eq!(links[0]["target"], uri(&higher_priority_target).as_str());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_an_inflight_pull_diagnostic_returns_request_canceled_once() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);
    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(root);
    server.initialize_with_pull_diagnostics(source.parent().expect("workspace root"));

    let request_id = RequestId::from("pull-document-diagnostic-cancel".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument": {"uri": uri(&source)}}),
    );
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": request_id}));

    let response = server.response(&request_id);
    let error = response.error.expect("cancelled pull must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    barrier.release();
    server.assert_no_response(&request_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn pull_diagnostics_stale_worker_returns_server_cancelled_with_retrigger_data() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let original = "unit Main;\ninterface\nimplementation\nend.\n";
    let changed = "unit Main;\ninterface\nconst Changed = 1;\nimplementation\nend.\n";
    write_file(&source, original);
    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(root);
    server.initialize_with_pull_diagnostics(source.parent().expect("workspace root"));

    let request_id = RequestId::from("pull-document-diagnostic-stale".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument": {"uri": uri(&source)}}),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": changed
            }
        }),
    );
    barrier.release();

    let response = server.response(&request_id);
    let error = response.error.expect("stale pull must fail");
    assert_eq!(error.code, -32802);
    assert_eq!(error.data, Some(json!({"retriggerRequest": true})));
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_fence_cancels_an_inflight_dependency_scoped_definition() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let mut directory = root.join("deep");
    fs::create_dir_all(&directory).expect("workspace directory");
    let component = "é".repeat(100);
    while directory
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        directory = directory.join(&component);
        fs::create_dir_all(&directory).expect("deep provider directory");
    }
    let provider = directory.join("DiskProvider.pas");
    let provider_text =
        "unit DiskProvider;\ninterface\nconst\n  diskOnly = 1;\nimplementation\nend.\n";
    let consumer = root.join("Consumer.pas");
    let consumer_text = "unit Consumer;\ninterface\nuses DiskProvider;\nimplementation\nend.\n";
    write_file(&provider, provider_text);
    write_file(&consumer, consumer_text);
    let provider_uri = uri(&provider);
    assert!(provider_uri.as_str().len() > 4_096);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(temp);
    server.initialize(&root, Value::Null);
    let definition_id = RequestId::from("inflight-definition-before-rejected-open".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&consumer)},
            "position": position_of(consumer_text, "DiskProvider", 0)
        }),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": provider_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorProvider;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let fenced_id = RequestId::from("request-observes-rejected-open-fence".to_string());
    server.send_request(
        fenced_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&consumer)},
            "position": position_of(consumer_text, "DiskProvider", 0)
        }),
    );
    assert!(
        server.response(&fenced_id).error.is_some(),
        "rejected open must install the fence"
    );

    // Lift the request fence before releasing the old worker. Its read set
    // must stay stale rather than becoming authoritative again on didClose.
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": provider_uri}}),
    );
    barrier.release();
    let response = server.response(&definition_id);
    assert!(
        response.error.is_some(),
        "pre-fence dependency-scoped definition must not succeed: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_fence_prevents_an_inflight_rename_workspace_edit() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    fs::create_dir_all(&root).expect("workspace directory");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    let oversized = root.join("deep");
    let mut directory = oversized;
    fs::create_dir_all(&directory).expect("deep directory");
    let component = "é".repeat(100);
    while directory
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        directory = directory.join(&component);
        fs::create_dir_all(&directory).expect("deep directory component");
    }
    let rejected_uri = uri(&directory.join("Rejected.pas"));

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(temp);
    server.initialize(&root, Value::Null);
    let rename_id = RequestId::from("inflight-rename-before-rejected-open".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0),
            "newName": "GOOD_CONST"
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rejected_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorRejected;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let fenced_id = RequestId::from("rename-observes-rejected-open-fence".to_string());
    server.send_request(
        fenced_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "badConst", 0)
        }),
    );
    assert!(server.response(&fenced_id).error.is_some());
    barrier.release();
    let response = server.response(&rename_id);
    assert!(
        response.error.is_some(),
        "a pre-fence rename must not return a WorkspaceEdit: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_fence_stops_partial_result_delivery_before_first_chunk() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    fs::create_dir_all(&root).expect("workspace directory");
    let main = root.join("Main.pas");
    write_file(&main, "unit Main;\ninterface\nimplementation\nend.\n");
    let mut directory = root.join("deep");
    fs::create_dir_all(&directory).expect("deep directory");
    let component = "é".repeat(100);
    while directory
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        directory = directory.join(&component);
        fs::create_dir_all(&directory).expect("deep directory component");
    }
    let rejected_uri = uri(&directory.join("Rejected.pas"));

    let (mut server, barrier) = TestServer::launch_with_workspace_symbols_barrier(temp);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("inflight-partial-symbols-before-rejected-open".to_string());
    server.send_request(
        request_id.clone(),
        "workspace/symbol",
        json!({"query": "Main", "partialResultToken": "fenced-symbols"}),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rejected_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorRejected;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let fenced_id = RequestId::from("partial-request-observes-rejected-open-fence".to_string());
    server.send_request(
        fenced_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": {"line": 0, "character": 5}
        }),
    );
    assert!(server.response(&fenced_id).error.is_some());
    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_some(),
        "partial request must fail rather than return a complete result: {response:?}"
    );
    server.assert_no_notification("$/progress");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_fence_fails_queued_client_jobs_without_dispatch() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    fs::create_dir_all(&root).expect("workspace directory");
    let provider = root.join("DiskProvider.pas");
    write_file(
        &provider,
        "unit DiskProvider;\ninterface\nconst\n  diskOnly = 1;\nimplementation\nend.\n",
    );
    let mut consumers = Vec::new();
    for name in ["First", "Second", "Third", "Queued"] {
        let path = root.join(format!("{name}.pas"));
        let text = format!("unit {name};\ninterface\nuses DiskProvider;\nimplementation\nend.\n");
        write_file(&path, &text);
        consumers.push((path, text));
    }
    let mut directory = root.join("deep");
    fs::create_dir_all(&directory).expect("deep directory");
    let component = "é".repeat(100);
    while directory
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        directory = directory.join(&component);
        fs::create_dir_all(&directory).expect("deep directory component");
    }
    let rejected_uri = uri(&directory.join("Rejected.pas"));

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(temp);
    server.initialize(&root, Value::Null);
    let mut running_ids = Vec::new();
    for (index, (path, text)) in consumers.iter().take(3).enumerate() {
        let id = RequestId::from(format!("running-definition-{index}"));
        server.send_request(
            id.clone(),
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri(path)},
                "position": position_of(text, "DiskProvider", 0)
            }),
        );
        running_ids.push(id);
    }
    barrier.wait_for_entries(3);
    let queued_id = RequestId::from("queued-definition-before-rejected-open".to_string());
    let (queued_path, queued_text) = &consumers[3];
    server.send_request(
        queued_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(queued_path)},
            "position": position_of(queued_text, "DiskProvider", 0)
        }),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rejected_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorRejected;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let fenced_id = RequestId::from("queued-fence-confirmation".to_string());
    server.send_request(
        fenced_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(queued_path)},
            "position": position_of(queued_text, "DiskProvider", 0)
        }),
    );
    assert!(server.response(&fenced_id).error.is_some());
    barrier.release();
    for id in running_ids.into_iter().chain([queued_id]) {
        assert!(
            server.response(&id).error.is_some(),
            "pre-fence running/queued request {id:?} must fail"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_fence_cancels_inflight_pull_diagnostics_and_requests_refresh() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    let mut directory = root.join("deep");
    fs::create_dir_all(&directory).expect("workspace directory");
    let component = "é".repeat(100);
    while directory
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        directory = directory.join(&component);
        fs::create_dir_all(&directory).expect("deep provider directory");
    }
    let provider = directory.join("DiskProvider.pas");
    write_file(
        &provider,
        "unit DiskProvider;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n",
    );
    let provider_uri = uri(&provider);
    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(temp);
    server.initialize_with_pull_diagnostics(&root);

    // Prime a report ID, then re-arm the worker barrier. A stale response must
    // not be allowed to claim this old report is still `unchanged`.
    barrier.release();
    let baseline_id = RequestId::from("pull-diagnostics-baseline-before-fence".to_string());
    server.send_request(
        baseline_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument": {"uri": provider_uri}, "previousResultId": null}),
    );
    let baseline = server.response(&baseline_id);
    assert!(
        baseline.error.is_none(),
        "baseline pull failed: {baseline:?}"
    );
    let previous_result_id = baseline.result.expect("baseline diagnostic result")["resultId"]
        .as_str()
        .expect("baseline result ID")
        .to_string();
    fs::remove_file(&barrier.release).expect("re-arm diagnostics worker barrier");

    let diagnostic_id =
        RequestId::from("inflight-pull-diagnostics-before-rejected-open".to_string());
    server.send_request(
        diagnostic_id.clone(),
        "textDocument/diagnostic",
        json!({
            "textDocument": {"uri": provider_uri},
            "previousResultId": previous_result_id
        }),
    );
    barrier.wait_for_entries(2);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": provider_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorProvider;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let fenced_id = RequestId::from("pull-request-observes-rejected-open-fence".to_string());
    server.send_request(
        fenced_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument": {"uri": provider_uri}, "previousResultId": null}),
    );
    assert!(server.response(&fenced_id).error.is_some());

    let refresh = server
        .request_with_timeout("workspace/diagnostic/refresh", Duration::from_secs(2))
        .expect("rejected didOpen must refresh pull diagnostics");
    server.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
    barrier.release();
    let response = server.response(&diagnostic_id);
    assert!(
        response.error.is_some(),
        "pre-fence pull diagnostics must not return full/unchanged: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_fence_clears_push_diagnostics_for_open_roots() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("fixture");
    fs::create_dir_all(&root).expect("workspace directory");
    let open_source = root.join("Open.pas");
    let open_text = "unit Open;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&open_source, open_text);
    let mut directory = root.join("deep");
    fs::create_dir_all(&directory).expect("deep directory");
    let component = "é".repeat(100);
    while directory
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        directory = directory.join(&component);
        fs::create_dir_all(&directory).expect("deep provider directory");
    }
    let rejected = directory.join("Rejected.pas");
    write_file(
        &rejected,
        "unit Rejected;\ninterface\nimplementation\nend.\n",
    );
    let rejected_uri = uri(&rejected);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(temp);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&open_source),
                "languageId": "pascal",
                "version": 1,
                "text": open_text
            }
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rejected_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorRejected;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let fenced_id = RequestId::from("push-request-observes-rejected-open-fence".to_string());
    server.send_request(
        fenced_id.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&open_source)},
            "position": position_of(open_text, "badConst", 0)
        }),
    );
    assert!(server.response(&fenced_id).error.is_some());

    let cleared = [
        server.notification("textDocument/publishDiagnostics"),
        server.notification("textDocument/publishDiagnostics"),
    ];
    for expected_uri in [uri(&open_source), rejected_uri] {
        assert!(
            cleared.iter().any(|publication| {
                publication["uri"] == expected_uri.as_str()
                    && publication["diagnostics"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
            }),
            "missing empty cleanup publication for {expected_uri}"
        );
    }
    barrier.release();
    server.assert_no_notification("textDocument/publishDiagnostics");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_push_cleanup_resumes_after_outbound_backpressure() {
    const ROOTS: usize = 64;
    let temp = tempfile::tempdir().expect("temporary workspace");
    let component = "é".repeat(100);
    let mut root = temp.path().join("fixture");
    while uri(&root).as_str().len() < 900 {
        root = root.join(&component);
    }
    fs::create_dir_all(&root).expect("workspace directory");
    let mut expected_uris = HashSet::new();
    for index in 0..ROOTS {
        let source = root.join(format!("Root{index}.pas"));
        write_file(
            &source,
            &format!(
                "unit Root{index};\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n"
            ),
        );
        let source_uri = uri(&source);
        expected_uris.insert(source_uri.clone());
    }

    let mut long_dir = root.join("deep");
    fs::create_dir_all(&long_dir).expect("deep directory");
    while long_dir
        .join(&component)
        .as_os_str()
        .to_string_lossy()
        .len()
        < 3_500
    {
        long_dir = long_dir.join(&component);
        fs::create_dir_all(&long_dir).expect("deep directory component");
    }
    let rejected_uri = uri(&long_dir.join("Rejected.pas"));
    assert!(rejected_uri.as_str().len() > 4_096);

    let (mut server, writer_barrier) = TestServer::launch_with_outbound_writer_barrier(temp);
    server.initialize(&root, Value::Null);
    for index in 0..ROOTS {
        let source = root.join(format!("Root{index}.pas"));
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&source),
                    "languageId": "pascal",
                    "version": 1,
                    "text": format!(
                        "unit Root{index};\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n"
                    )
                }
            }),
        );
    }

    let mut initial_publications = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while initial_publications.len() < ROOTS {
        assert!(
            Instant::now() < deadline,
            "only received {} initial root publications",
            initial_publications.len()
        );
        let message = match server.pending.pop_front() {
            Some(message) => message,
            None => server
                .messages
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "timed out waiting for initial publications: got {} of {ROOTS}: {error}",
                        initial_publications.len()
                    )
                })
                .expect("read LSP message")
                .expect("LSP server closed during initial publications"),
        };
        match message {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                let published_uri = Url::parse(
                    notification.params["uri"]
                        .as_str()
                        .expect("published diagnostic URI"),
                )
                .expect("valid published URI");
                if expected_uris.contains(&published_uri) {
                    assert!(
                        initial_publications.insert(published_uri.clone()),
                        "duplicate initial publication for {published_uri}"
                    );
                }
            }
            _ => {}
        }
    }
    assert_eq!(initial_publications, expected_uris);

    fs::write(&writer_barrier.armed, b"pause").expect("arm outbound writer barrier");
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rejected_uri,
                "languageId": "pascal",
                "version": 1,
                "text": "unit EditorRejected;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": rejected_uri}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rejected_uri,
                "languageId": "pascal",
                "version": 2,
                "text": "unit EditorRejectedAgain;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let root_zero_uri = uri(&root.join("Root0.pas"));
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": root_zero_uri}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": root_zero_uri,
                "languageId": "pascal",
                "version": 2,
                "text": "unit Root0;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n"
            }
        }),
    );
    let request_id = RequestId::from("request-during-resumable-push-cleanup".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&root.join("Root0.pas"))}}),
    );
    writer_barrier.wait_until_entered();
    thread::sleep(Duration::from_millis(500));
    let process_status = server.child.try_wait().expect("inspect LSP process");
    writer_barrier.release();
    assert!(
        process_status.is_none(),
        "server exited instead of retaining cleanup after outbound backpressure: {process_status:?}"
    );

    let mut clear_counts = HashMap::<Url, usize>::new();
    let mut request_response = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut expected_clears = expected_uris.clone();
    expected_clears.insert(rejected_uri.clone());
    while clear_counts.len() < expected_clears.len() || request_response.is_none() {
        assert!(
            Instant::now() < deadline,
            "cleanup/request did not drain: cleared={}, response={:?}",
            clear_counts.len(),
            request_response
        );
        let message = server
            .pending
            .pop_front()
            .unwrap_or_else(|| server.receive_until(deadline));
        match message {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                let publication_uri = Url::parse(
                    notification.params["uri"]
                        .as_str()
                        .expect("publication URI"),
                )
                .expect("valid publication URI");
                if expected_clears.contains(&publication_uri) {
                    assert!(
                        notification.params["diagnostics"]
                            .as_array()
                            .is_some_and(Vec::is_empty),
                        "stale diagnostics were republished for {publication_uri}"
                    );
                    *clear_counts.entry(publication_uri.clone()).or_default() += 1;
                }
            }
            Message::Response(response) if response.id == request_id => {
                request_response = Some(response);
            }
            _ => {}
        }
    }
    assert_eq!(
        clear_counts.len(),
        expected_clears.len(),
        "every previously published or rejected URI must be cleared"
    );
    assert!(
        clear_counts.values().all(|count| *count == 1),
        "every old root must receive exactly one cleanup publication"
    );
    let request_error = request_response
        .expect("request response")
        .error
        .expect("fenced analysis request must fail closed");
    assert_eq!(
        request_error.code, -32803,
        "server must service the request and return RequestFailed while fenced"
    );
    assert!(
        server
            .child
            .try_wait()
            .expect("inspect server status")
            .is_none()
    );
    server.assert_no_notification("textDocument/publishDiagnostics");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rejected_open_cleanup_caps_do_not_exit_for_large_uri_targets() {
    for uri_length in [16_384_usize, 16_385, 17_016] {
        let root = tempfile::tempdir().expect("workspace");
        let source = root.path().join("Main.pas");
        let text = "unit Main;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
        write_file(&source, text);
        let root_uri = uri(&source);
        let mut server = TestServer::launch();
        server.initialize(root.path(), Value::Null);
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": root_uri,
                    "languageId": "pascal",
                    "version": 1,
                    "text": text
                }
            }),
        );
        let initial = server
            .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
            .expect("initial push publication for open root");
        assert!(
            initial["diagnostics"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );

        let prefix = "file:///";
        let oversized_uri = Url::parse(&format!(
            "{prefix}{}",
            "u".repeat(uri_length.saturating_sub(prefix.len()))
        ))
        .expect("syntactically valid large file URI");
        assert_eq!(oversized_uri.as_str().len(), uri_length);
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": oversized_uri,
                    "languageId": "pascal",
                    "version": 1,
                    "text": "unit Rejected;\ninterface\nimplementation\nend.\n"
                }
            }),
        );

        let request_id = RequestId::from(format!("request-after-cleanup-uri-{uri_length}"));
        server.send_request(
            request_id.clone(),
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri(&source)}}),
        );
        let response = server.response(&request_id);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(-32803),
            "analysis must fail closed for rejected URI length {uri_length}: {response:?}"
        );
        assert!(
            server
                .child
                .try_wait()
                .expect("inspect server status")
                .is_none(),
            "push server exited for rejected URI length {uri_length}"
        );
        let clear = server
            .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
            .expect("previously published root must be cleared");
        assert!(
            clear["diagnostics"].as_array().is_some_and(Vec::is_empty),
            "previous diagnostics must be cleared for rejected URI length {uri_length}"
        );
        server.shutdown();
    }
}

#[cfg(feature = "test-support")]
#[test]
fn stale_workspace_diagnostic_partial_delivery_requests_retrigger() {
    let root = tempfile::tempdir().expect("workspace");
    let first = root.path().join("Unit000.pas");
    let first_text = "unit Unit000;\ninterface\nimplementation\nend.\n";
    for index in 0..64 {
        let path = root.path().join(format!("Unit{index:03}.pas"));
        write_file(
            &path,
            &format!("unit Unit{index:03};\ninterface\nimplementation\nend.\n"),
        );
    }

    let mut server = TestServer::launch();
    server.initialize_with_pull_diagnostics(root.path());
    let request_id = RequestId::from("stale-workspace-diagnostic-partial".to_string());
    server.send_request(
        request_id.clone(),
        "workspace/diagnostic",
        json!({
            "previousResultIds": [],
            "partialResultToken": "stale-workspace-diagnostic-partial"
        }),
    );
    let partial = server.notification("$/progress");
    assert_eq!(partial["token"], "stale-workspace-diagnostic-partial");
    assert!(
        !partial["value"]["items"]
            .as_array()
            .expect("partial workspace diagnostic items")
            .is_empty()
    );

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&first),
                "languageId": "pascal",
                "version": 1,
                "text": first_text
            }
        }),
    );

    let response = server.response(&request_id);
    let error = response.error.expect("stale workspace partial must fail");
    assert_eq!(error.code, -32802);
    assert_eq!(error.data, Some(json!({"retriggerRequest": true})));
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn evidence_pressure_partial_delivery_retriggers_before_stale_success() {
    let root = tempfile::tempdir().expect("workspace");
    let mut units = Vec::new();
    for index in 0..8 {
        let path = root.path().join(format!("U{index:02}.pas"));
        write_file(
            &path,
            &format!("unit U{index:02};\ninterface\nconst badConst = 1;\nimplementation\nend.\n"),
        );
        units.push(path);
    }
    let mut source_paths = vec![".".to_string()];
    for index in 0..64 {
        let directory = root.path().join(format!("p{index:03}_{}", "x".repeat(115)));
        fs::create_dir(&directory).expect("authorized empty source directory");
        source_paths.push(directory.to_string_lossy().to_string());
    }

    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    server.initialize_with_pull_diagnostics_options(
        units[0].parent().expect("workspace root"),
        json!({"sourcePaths": source_paths}),
    );
    let request_id = RequestId::from("evidence-pressure-partial".to_string());
    server.send_request(
        request_id.clone(),
        "workspace/diagnostic",
        json!({
            "previousResultIds": [],
            "partialResultToken": "evidence-pressure-partial"
        }),
    );
    barrier.wait_until_entered();

    let original = fs::read_to_string(&units[0]).expect("read first unit");
    fs::write(&units[0], original.replace("badConst", "BADCONST"))
        .expect("mutate first unit during validation");
    barrier.release();

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("evidence pressure must not complete stale partial delivery");
    assert_eq!(error.code, -32802);
    assert_eq!(error.data, Some(json!({"retriggerRequest": true})));
    let partial = server.take_partial_items("evidence-pressure-partial");
    assert!(
        partial.is_empty(),
        "stale evidence must not be published in a partial chunk: {partial:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn workspace_symbol_request_reports_string_work_done_progress_in_order() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    write_file(
        &source,
        "unit Main;\ninterface\ntype\n  TMain = class\n  end;\nimplementation\nend.\n",
    );

    let mut server = TestServer::launch();
    let initialize_id = RequestId::from("progress-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(root.path()),
            "capabilities": {
                "window": {"workDoneProgress": true}
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    let initialize_result = initialize.result.expect("initialize result");
    assert_eq!(
        initialize_result["capabilities"]["workspaceSymbolProvider"]["workDoneProgress"],
        true
    );
    server.send_notification("initialized", json!({}));

    let request_id = RequestId::from("workspace-symbol-progress".to_string());
    server.send_request(
        request_id.clone(),
        "workspace/symbol",
        json!({"query": "TMain", "workDoneToken": "symbols-progress"}),
    );

    let begin = server.notification("$/progress");
    assert_eq!(begin["token"], "symbols-progress");
    assert_eq!(begin["value"]["kind"], "begin");
    let report = server.notification("$/progress");
    assert_eq!(report["token"], "symbols-progress");
    assert_eq!(report["value"]["kind"], "report");
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "workspace symbol failed: {response:?}"
    );
    let end = server.notification("$/progress");
    assert_eq!(end["token"], "symbols-progress");
    assert_eq!(end["value"]["kind"], "end");

    let integer_request_id = RequestId::from("workspace-symbol-integer-progress".to_string());
    server.send_request(
        integer_request_id.clone(),
        "workspace/symbol",
        json!({"query": "TMain", "workDoneToken": 37}),
    );
    let integer_begin = server.notification("$/progress");
    assert_eq!(integer_begin["token"], 37);
    assert_eq!(integer_begin["value"]["kind"], "begin");
    let integer_report = server.notification("$/progress");
    assert_eq!(integer_report["token"], 37);
    assert_eq!(integer_report["value"]["kind"], "report");
    let integer_response = server.response(&integer_request_id);
    assert!(integer_response.error.is_none());
    let integer_end = server.notification("$/progress");
    assert_eq!(integer_end["token"], 37);
    assert_eq!(integer_end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn workspace_symbol_partial_result_string_token_chunks_without_final_duplicates() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let text = workspace_symbol_source("Main", 400);
    write_file(&source, &text);

    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let partial_id = RequestId::from("workspace-symbol-partial".to_string());
    server.send_request(
        partial_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "symbols-partial"}),
    );

    let mut partial_items = Vec::new();
    let final_result = loop {
        match server.next_message() {
            Message::Notification(notification) if notification.method == "$/progress" => {
                assert_eq!(notification.params["token"], "symbols-partial");
                let value = notification.params["value"]
                    .as_array()
                    .expect("partial result progress value must be an array");
                assert!(
                    !value.is_empty(),
                    "non-empty output must use non-empty chunks"
                );
                assert!(
                    value.len() <= 128,
                    "partial result chunk exceeds the item bound"
                );
                assert!(
                    serde_json::to_vec(value)
                        .expect("partial result chunk must encode")
                        .len()
                        <= 64 * 1024,
                    "partial result chunk exceeds the encoded byte bound"
                );
                partial_items.extend(value.iter().cloned());
            }
            Message::Response(response) if response.id == partial_id => {
                break response.result.expect("partial final result");
            }
            other => server.pending.push_back(other),
        }
    };
    assert!(final_result.as_array().expect("final array").is_empty());
    assert!(
        partial_items.len() > 1,
        "the bounded delivery should produce multiple chunks/items"
    );

    let ordinary_id = RequestId::from("workspace-symbol-ordinary".to_string());
    server.send_request(
        ordinary_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol"}),
    );
    let ordinary = server
        .response(&ordinary_id)
        .result
        .expect("ordinary result")
        .as_array()
        .expect("ordinary array")
        .clone();
    assert_eq!(partial_items, ordinary);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn oversized_partial_item_fails_only_its_request_and_keeps_the_session_alive() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    write_file(
        &source,
        &format!(
            "unit Main; interface var Symbol{}: Integer; implementation end.\n",
            "x".repeat(70_000)
        ),
    );
    let mut server = TestServer::launch();
    server.initialize_with_progress(root.path());

    let partial_id = RequestId::from("oversized-partial".to_string());
    server.send_request(
        partial_id.clone(),
        "workspace/symbol",
        json!({
            "query": "Symbol",
            "partialResultToken": "oversized-partial-token",
            "workDoneToken": "oversized-work-done"
        }),
    );

    let mut progress_kinds = Vec::new();
    let response = loop {
        match server.next_message() {
            Message::Notification(notification) if notification.method == "$/progress" => {
                if notification.params["token"] == "oversized-work-done" {
                    progress_kinds.push(
                        notification.params["value"]["kind"]
                            .as_str()
                            .expect("work-done progress kind")
                            .to_owned(),
                    );
                }
            }
            Message::Response(response) if response.id == partial_id => break response,
            other => server.pending.push_back(other),
        }
    };
    assert_eq!(
        response
            .error
            .expect("oversized partial request must fail")
            .code,
        -32803
    );
    let end = server.notification("$/progress");
    assert_eq!(end["token"], "oversized-work-done");
    assert_eq!(end["value"]["kind"], "end");
    progress_kinds.push(
        end["value"]["kind"]
            .as_str()
            .expect("work-done end kind")
            .to_owned(),
    );
    assert_eq!(progress_kinds, ["begin", "report", "end"]);

    let healthy_id = RequestId::from("healthy-after-oversized".to_string());
    server.send_request(
        healthy_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol"}),
    );
    let healthy = server.response(&healthy_id);
    assert!(
        healthy.error.is_none(),
        "the oversized partial request must not terminate the session: {healthy:?}"
    );
    assert_eq!(
        healthy
            .result
            .expect("healthy symbol result")
            .as_array()
            .unwrap()
            .len(),
        1
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn oversized_ordinary_result_is_request_scoped_and_keeps_the_session_alive() {
    let root = tempfile::tempdir().expect("workspace");
    for unit in 0..10 {
        let mut source = format!("unit Unit{unit};\ninterface\nvar\n");
        for index in 0..800 {
            source.push_str(&format!(
                "  Symbol{unit}_{index}_{}: Integer;\n",
                "x".repeat(1_000)
            ));
        }
        source.push_str("implementation\nend.\n");
        write_file(&root.path().join(format!("Unit{unit}.pas")), &source);
    }

    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let large_id = RequestId::from("ordinary-large-result".to_string());
    server.send_request(
        large_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol"}),
    );
    let large = server.response(&large_id);
    assert_eq!(
        large
            .error
            .expect("oversized ordinary result must fail")
            .code,
        -32803,
        "a valid result that exceeds the bounded control budget must fail only its request"
    );

    let healthy_id = RequestId::from("ordinary-large-result-follow-up".to_string());
    server.send_request(
        healthy_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol0_0_"}),
    );
    let healthy = server.response(&healthy_id);
    assert!(
        healthy.error.is_none(),
        "an oversized ordinary result must not terminate the session: {healthy:?}"
    );
    assert_eq!(
        healthy
            .result
            .expect("healthy ordinary result")
            .as_array()
            .expect("healthy ordinary array")
            .len(),
        1
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn references_partial_result_integer_token_matches_complete_result_with_declaration() {
    let (_temp, main, provider, main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let partial_id = RequestId::from("references-partial-integer".to_string());
    server.send_request(
        partial_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(&main_source, "PublicRoutine", 0),
            "context": {"includeDeclaration": true},
            "partialResultToken": 77
        }),
    );
    let (partial_items, partial_response) =
        collect_partial_response(&mut server, &partial_id, &json!(77));
    assert!(
        partial_response.error.is_none(),
        "partial references failed"
    );
    assert!(
        partial_response
            .result
            .expect("partial final references")
            .as_array()
            .expect("partial final array")
            .is_empty()
    );

    let ordinary_id = RequestId::from("references-ordinary-after-partial".to_string());
    server.send_request(
        ordinary_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(&main_source, "PublicRoutine", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let ordinary_items = result_locations(server.response(&ordinary_id));
    assert_eq!(partial_items, ordinary_items);
    assert!(
        partial_items
            .iter()
            .any(|location| location["uri"] == uri(&provider).to_string()),
        "declaration-inclusive references must retain the provider declaration"
    );
    let unique = partial_items
        .iter()
        .map(location_signature)
        .collect::<HashSet<_>>();
    assert_eq!(unique.len(), partial_items.len());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn empty_partial_workspace_result_has_only_the_final_empty_response() {
    let root = tempfile::tempdir().expect("workspace");
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let id = RequestId::from("empty-partial-symbols".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "Missing", "partialResultToken": "empty-partial"}),
    );
    let (items, response) = collect_partial_response(&mut server, &id, &json!("empty-partial"));
    assert!(items.is_empty());
    assert!(
        response.error.is_none(),
        "empty partial request failed: {response:?}"
    );
    assert_eq!(response.result, Some(Value::Array(Vec::new())));
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn malformed_partial_result_token_is_rejected_without_analysis() {
    let root = tempfile::tempdir().expect("workspace");
    write_file(
        &root.path().join("Main.pas"),
        "unit Main; interface implementation end.\n",
    );
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let id = RequestId::from("malformed-partial-token".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "Main", "partialResultToken": {"not": "a token"}}),
    );
    let response = server.response(&id);
    let error = response.error.expect("malformed partial token error");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("partialResultToken"));
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn partial_and_work_done_tokens_use_separate_progress_payloads() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    write_file(&source, &workspace_symbol_source("Main", 400));
    let mut server = TestServer::launch();
    server.initialize_with_progress(root.path());

    let id = RequestId::from("partial-and-workdone".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({
            "query": "Symbol",
            "workDoneToken": "work-done",
            "partialResultToken": "partial-items"
        }),
    );

    let mut partial_items = Vec::new();
    let mut work_kinds = Vec::new();
    let response = loop {
        match server.next_message() {
            Message::Notification(notification) if notification.method == "$/progress" => {
                let token = &notification.params["token"];
                let value = &notification.params["value"];
                if token == "partial-items" {
                    partial_items.extend(
                        value
                            .as_array()
                            .expect("partial token must carry arrays")
                            .iter()
                            .cloned(),
                    );
                } else {
                    assert_eq!(token, "work-done");
                    work_kinds.push(
                        value["kind"]
                            .as_str()
                            .expect("work progress kind")
                            .to_owned(),
                    );
                }
            }
            Message::Response(response) if response.id == id => break response,
            other => server.pending.push_back(other),
        }
    };
    assert!(response.error.is_none(), "combined progress request failed");
    assert_eq!(response.result, Some(Value::Array(Vec::new())));
    assert!(!partial_items.is_empty());
    assert_eq!(work_kinds, ["begin", "report"]);
    let end = server.notification("$/progress");
    assert_eq!(end["token"], "work-done");
    assert_eq!(end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn active_partial_token_collision_is_rejected_and_token_can_be_reused() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    write_file(&source, &workspace_symbol_source("Main", 1_200));
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let first_id = RequestId::from("partial-token-owner".to_string());
    server.send_request(
        first_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "reused-token"}),
    );
    let first_chunk = server.notification("$/progress");
    assert_eq!(first_chunk["token"], "reused-token");
    assert!(first_chunk["value"].as_array().is_some());

    let collision_id = RequestId::from("partial-token-collision".to_string());
    server.send_request(
        collision_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "reused-token"}),
    );
    let collision = server.response(&collision_id);
    let collision_error = collision.error.expect("active token collision error");
    assert_eq!(collision_error.code, -32803);
    assert!(collision_error.message.contains("progress token"));

    let (items, response) =
        collect_partial_response(&mut server, &first_id, &json!("reused-token"));
    assert!(response.error.is_none(), "original partial request failed");
    assert!(!items.is_empty());

    let reused_id = RequestId::from("partial-token-reused".to_string());
    server.send_request(
        reused_id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol1199", "partialResultToken": "reused-token"}),
    );
    let (_reused_items, reused_response) =
        collect_partial_response(&mut server, &reused_id, &json!("reused-token"));
    assert!(
        reused_response.error.is_none(),
        "finished token must be reusable"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_partial_delivery_stops_future_chunks_and_returns_request_canceled() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let text = workspace_symbol_source("Main", 3_000);
    write_file(&source, &text);
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let id = RequestId::from("cancel-partial-delivery".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "cancel-partial"}),
    );
    let first = server.notification("$/progress");
    assert_eq!(first["token"], "cancel-partial");
    assert!(first["value"].as_array().is_some());

    server.send_notification("$/cancelRequest", json!({"id": id}));
    let response = server.response(&id);
    let error = response.error.expect("partial cancellation error");
    assert_eq!(error.code, -32800);
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn source_change_between_partial_chunks_fails_without_a_successful_final_response() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let text = workspace_symbol_source("Main", 2_000);
    write_file(&source, &text);
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );

    let id = RequestId::from("stale-partial-delivery".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "stale-partial"}),
    );
    let first = server.notification("$/progress");
    assert_eq!(first["token"], "stale-partial");
    assert!(first["value"].as_array().is_some());

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source), "version": 2},
            "contentChanges": [{"text": text}]
        }),
    );
    let response = server.response(&id);
    let error = response.error.expect("stale partial delivery error");
    assert_eq!(error.code, -32803);
    assert!(error.message.contains("stale"));
    server.assert_no_response(&id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unnotified_disk_change_between_workspace_symbol_chunks_fails_closed() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    write_file(&source, &workspace_symbol_source("Main", 3_000));
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let id = RequestId::from("unnotified-disk-symbols".to_string());
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "disk-symbols"}),
    );
    let first = server.notification("$/progress");
    assert_eq!(first["token"], "disk-symbols");
    write_file(&source, "unit Main; interface implementation end.\n");

    let response = server.response(&id);
    let error = response
        .error
        .expect("unnotified disk change must fail the partial request");
    assert_eq!(error.code, -32803);
    assert!(
        response.result.is_none(),
        "stale delivery must not claim success"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unnotified_disk_change_between_reference_chunks_fails_closed() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let text = format!(
        "unit Main;\ninterface\nvar Target: Integer;\nimplementation\nprocedure Run;\nbegin\n{}end;\nend.\n",
        "Target := Target + 1;\n".repeat(1_000)
    );
    write_file(&source, &text);
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);

    let id = RequestId::from("unnotified-disk-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&source)},
            "position": {"line": 2, "character": 5},
            "context": {"includeDeclaration": true},
            "partialResultToken": "disk-references"
        }),
    );
    let first = server.notification("$/progress");
    assert_eq!(first["token"], "disk-references");
    write_file(&source, "unit Main; interface implementation end.\n");

    let response = server.response(&id);
    let error = response
        .error
        .expect("unnotified disk change must fail the partial request");
    assert_eq!(error.code, -32803);
    assert!(
        response.result.is_none(),
        "stale delivery must not claim success"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn generated_work_done_tokens_avoid_active_partial_tokens() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = workspace_symbol_source("Main", 1_000);
    write_file(&source, &text);
    let (mut server, workspace_symbols, diagnostics) =
        TestServer::launch_with_workspace_symbols_and_diagnostics_barriers(environment);
    server.initialize_with_progress(&root);

    let request_id = RequestId::from("generated-token-collision".to_string());
    server.send_request(
        request_id.clone(),
        "workspace/symbol",
        json!({
            "query": "Symbol",
            "partialResultToken": "pascal-lsp-progress-1"
        }),
    );
    workspace_symbols.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );
    let create = server.request("window/workDoneProgress/create");
    let generated_token = create.params["token"].clone();
    assert_ne!(
        generated_token,
        json!("pascal-lsp-progress-1"),
        "server progress allocation must reserve the partial-token namespace"
    );
    server.send(Message::Response(Response::new_ok(create.id, Value::Null)));
    let begin = server.notification("$/progress");
    assert_eq!(begin["token"], generated_token);
    assert_eq!(begin["value"]["kind"], "begin");

    workspace_symbols.release();
    diagnostics.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none()
            || response
                .error
                .as_ref()
                .is_some_and(|error| error.code == -32803),
        "partial request must terminate after the controlled state change: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn opening_a_file_reports_indexing_progress() {
    let root = tempfile::tempdir().expect("workspace");
    let main = root.path().join("Main.pas");
    write_file(
        &main,
        "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n",
    );
    write_file(
        &root.path().join("Provider.pas"),
        "unit Provider;\ninterface\nimplementation\nend.\n",
    );
    let mut server = TestServer::launch();
    server.initialize_with_progress(root.path());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&main), "languageId": "pascal", "version": 1,
            "text": fs::read_to_string(&main).unwrap()}}),
    );

    let mut seen_titles = Vec::new();
    let mut indexing_token = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && indexing_token.is_none() {
        let Some(create) = server.request_with_timeout(
            "window/workDoneProgress/create",
            deadline.saturating_duration_since(Instant::now()),
        ) else {
            break;
        };
        let token = create.params["token"].clone();
        server.send(Message::Response(Response::new_ok(create.id, Value::Null)));

        while Instant::now() < deadline {
            let progress = server.notification_with_timeout(
                "$/progress",
                deadline.saturating_duration_since(Instant::now()),
            );
            if progress["value"]["kind"] != "begin" || progress["token"] != token {
                continue;
            }
            let title = progress["value"]["title"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            seen_titles.push(title.clone());
            if title == "Indexing Main" {
                assert_eq!(progress["value"]["message"], "0/1 units");
                indexing_token = Some(token.clone());
            }
            break;
        }
    }
    let token =
        indexing_token.unwrap_or_else(|| panic!("no Indexing progress; saw {seen_titles:?}"));
    loop {
        let progress = server.notification("$/progress");
        if progress["token"] == token && progress["value"]["kind"] == "end" {
            break;
        }
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn no_indexing_progress_without_client_support() {
    let root = tempfile::tempdir().expect("workspace");
    let main = root.path().join("Main.pas");
    write_file(&main, "unit Main;\ninterface\nimplementation\nend.\n");
    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&main), "languageId": "pascal", "version": 1,
            "text": fs::read_to_string(&main).unwrap()}}),
    );
    assert!(
        server
            .request_with_timeout("window/workDoneProgress/create", Duration::from_secs(2))
            .is_none()
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn coalesced_partial_recipients_keep_independent_tokens_and_ordinary_results() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    write_file(&source, &workspace_symbol_source("Main", 1_000));
    let (mut server, barrier) = TestServer::launch_with_workspace_symbols_barrier(root);
    let root_path = source.parent().expect("workspace root");
    server.initialize(root_path, Value::Null);

    let first_id = RequestId::from("coalesced-partial-one".to_string());
    let second_id = RequestId::from("coalesced-partial-two".to_string());
    let ordinary_id = RequestId::from("coalesced-ordinary".to_string());
    let query = "Symbol";
    server.send_request(
        first_id.clone(),
        "workspace/symbol",
        json!({"query": query, "partialResultToken": "coalesced-one"}),
    );
    barrier.wait_until_entered();
    server.send_request(
        second_id.clone(),
        "workspace/symbol",
        json!({"query": query, "partialResultToken": "coalesced-two"}),
    );
    server.send_request(
        ordinary_id.clone(),
        "workspace/symbol",
        json!({"query": query}),
    );
    barrier.release();

    let mut first_items = Vec::new();
    let mut second_items = Vec::new();
    let mut first_response = None;
    let mut second_response = None;
    let mut ordinary_response = None;
    while first_response.is_none() || second_response.is_none() || ordinary_response.is_none() {
        match server.next_message() {
            Message::Notification(notification) if notification.method == "$/progress" => {
                let chunk = notification.params["value"]
                    .as_array()
                    .expect("coalesced partial chunk");
                if notification.params["token"] == "coalesced-one" {
                    first_items.extend(chunk.iter().cloned());
                } else if notification.params["token"] == "coalesced-two" {
                    second_items.extend(chunk.iter().cloned());
                } else {
                    panic!(
                        "unexpected coalesced token: {:?}",
                        notification.params["token"]
                    );
                }
            }
            Message::Response(response) if response.id == first_id => {
                first_response = Some(response)
            }
            Message::Response(response) if response.id == second_id => {
                second_response = Some(response)
            }
            Message::Response(response) if response.id == ordinary_id => {
                ordinary_response = Some(response)
            }
            other => server.pending.push_back(other),
        }
    }
    assert!(
        first_response
            .as_ref()
            .expect("first response")
            .error
            .is_none()
    );
    assert!(
        second_response
            .as_ref()
            .expect("second response")
            .error
            .is_none()
    );
    assert!(
        ordinary_response
            .as_ref()
            .expect("ordinary response")
            .error
            .is_none()
    );
    let ordinary_items = ordinary_response
        .and_then(|response| response.result)
        .expect("ordinary result")
        .as_array()
        .expect("ordinary result array")
        .clone();
    assert_eq!(first_items, ordinary_items);
    assert_eq!(second_items, ordinary_items);
    assert_eq!(
        first_items
            .iter()
            .map(symbol_signature)
            .collect::<HashSet<_>>()
            .len(),
        first_items.len()
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn coalesced_work_done_lifecycles_survive_a_paused_stdout_queue() {
    let root = tempfile::tempdir().expect("workspace");
    write_file(
        &root.path().join("Main.pas"),
        &workspace_symbol_source("Main", 7_000),
    );
    let mut server = TestServer::launch();
    server.initialize_with_progress(root.path());

    let flood_id = RequestId::from("paused-output-flood".to_string());
    server.send_request(
        flood_id,
        "workspace/symbol",
        json!({"query": "Symbol", "partialResultToken": "paused-flood"}),
    );
    server.pause_stdout();
    thread::sleep(Duration::from_secs(2));

    let expected_ids = (0..30)
        .map(|index| RequestId::from(format!("paused-coalesced-{index}")))
        .collect::<HashSet<_>>();
    for (index, id) in expected_ids.iter().enumerate() {
        server.send_request(
            id.clone(),
            "workspace/symbol",
            json!({
                "query": "Missing",
                "partialResultToken": format!("paused-partial-{index}"),
                "workDoneToken": format!("paused-work-done-{index}")
            }),
        );
    }
    server.resume_stdout();

    let mut responses = HashSet::new();
    let mut work_done_ends = HashMap::<String, usize>::new();
    let lifecycle_deadline = Instant::now() + Duration::from_secs(15);
    while responses.len() < expected_ids.len() || work_done_ends.len() < expected_ids.len() {
        assert!(
            Instant::now() < lifecycle_deadline,
            "coalesced lifecycle did not drain: responses={}, ends={}, pending={:?}",
            responses.len(),
            work_done_ends.len(),
            server.pending
        );
        match server.next_message() {
            Message::Response(response) if expected_ids.contains(&response.id) => {
                assert!(
                    response.error.is_none(),
                    "every admitted coalesced request needs a terminal success: {response:?}"
                );
                assert!(responses.insert(response.id));
            }
            Message::Notification(notification)
                if notification.method == "$/progress"
                    && notification.params["value"]["kind"] == "end"
                    && notification.params["token"]
                        .as_str()
                        .is_some_and(|token| token.starts_with("paused-work-done-")) =>
            {
                let token = notification.params["token"]
                    .as_str()
                    .expect("work-done token")
                    .to_string();
                *work_done_ends.entry(token).or_default() += 1;
            }
            _ => {}
        }
    }
    assert_eq!(responses.len(), expected_ids.len());
    assert!(
        work_done_ends.values().all(|count| *count == 1),
        "each admitted work-done token must end exactly once: {work_done_ends:?}"
    );

    let healthy_id = RequestId::from("paused-output-follow-up".to_string());
    server.send_request(
        healthy_id.clone(),
        "workspace/symbol",
        json!({"query": "Missing"}),
    );
    let healthy = server.response(&healthy_id);
    assert!(
        healthy.error.is_none(),
        "the session must remain usable after the coalesced control burst: {healthy:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn large_ordinary_results_survive_temporary_control_byte_pressure() {
    let root = tempfile::tempdir().expect("workspace");
    write_file(
        &root.path().join("Flood.pas"),
        &wide_workspace_symbol_source("Flood", "Flood", 7_000, 0),
    );
    for index in 0..3 {
        write_file(
            &root.path().join(format!("Large{index}.pas")),
            &wide_workspace_symbol_source(&format!("Large{index}"), "Large", 1_000, 1_500),
        );
    }

    let mut server = TestServer::launch();
    server.initialize_with_progress(root.path());

    let flood_id = RequestId::from("temporary-byte-flood".to_string());
    server.send_request(
        flood_id.clone(),
        "workspace/symbol",
        json!({"query": "Flood", "partialResultToken": "temporary-flood"}),
    );
    server.pause_stdout();
    thread::sleep(Duration::from_secs(2));

    let ordinary_ids = [
        RequestId::from("large-ordinary-1".to_string()),
        RequestId::from("large-ordinary-2".to_string()),
    ];
    for (index, id) in ordinary_ids.iter().enumerate() {
        server.send_request(
            id.clone(),
            "workspace/symbol",
            json!({
                "query": "Large",
                "workDoneToken": format!("large-ordinary-work-{index}")
            }),
        );
    }
    thread::sleep(Duration::from_secs(1));
    server.resume_stdout();

    let expected_ids = ordinary_ids.iter().cloned().collect::<HashSet<_>>();
    let mut responses = HashMap::new();
    let mut work_done_ends = HashMap::<String, usize>::new();
    let mut flood_finished = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while responses.len() < expected_ids.len() || work_done_ends.len() < expected_ids.len() {
        assert!(
            Instant::now() < deadline,
            "large ordinary lifecycle did not drain: responses={}, ends={}, pending={:?}",
            responses.len(),
            work_done_ends.len(),
            server.pending
        );
        match server.next_message() {
            Message::Response(response) if expected_ids.contains(&response.id) => {
                assert!(
                    response.error.is_none(),
                    "individually valid ordinary result was not preserved: {response:?}"
                );
                let result = response
                    .result
                    .expect("large ordinary result")
                    .as_array()
                    .expect("large ordinary result array")
                    .len();
                assert_eq!(result, 3_003);
                assert!(responses.insert(response.id, result).is_none());
            }
            Message::Notification(notification)
                if notification.method == "$/progress"
                    && notification.params["value"]["kind"] == "end"
                    && notification.params["token"]
                        .as_str()
                        .is_some_and(|token| token.starts_with("large-ordinary-work-")) =>
            {
                let token = notification.params["token"]
                    .as_str()
                    .expect("large ordinary work-done token")
                    .to_string();
                *work_done_ends.entry(token).or_default() += 1;
            }
            // The flood can finish while the ordinary results drain.
            Message::Response(response) if response.id == flood_id => flood_finished = true,
            _ => {}
        }
    }
    assert_eq!(responses.len(), expected_ids.len());
    assert_eq!(
        work_done_ends,
        HashMap::from([
            ("large-ordinary-work-0".to_string(), 1),
            ("large-ordinary-work-1".to_string(), 1),
        ])
    );

    if !flood_finished {
        server.send_notification("$/cancelRequest", json!({"id": flood_id}));
        let _ = server.response(&flood_id);
    }

    let healthy_id = RequestId::from("temporary-byte-follow-up".to_string());
    server.send_request(
        healthy_id.clone(),
        "workspace/symbol",
        json!({"query": "Missing"}),
    );
    let healthy = server.response(&healthy_id);
    assert!(
        healthy.error.is_none(),
        "session must remain usable after deferred large results: {healthy:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelled_partial_validators_are_bounded_until_they_retire() {
    let root = tempfile::tempdir().expect("workspace");
    write_file(
        &root.path().join("Main.pas"),
        &workspace_symbol_source("Main", 64),
    );
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(root);
    let root_path = server
        ._environment
        .as_ref()
        .expect("test environment")
        .path()
        .to_path_buf();
    server.initialize(&root_path, Value::Null);

    let mut admitted = 0;
    let mut rejected = None;
    for index in 0..40 {
        let id = RequestId::from(format!("retiring-validator-{index}"));
        server.send_request(
            id.clone(),
            "workspace/symbol",
            json!({
                "query": format!("Symbol{index}"),
                "partialResultToken": format!("retiring-partial-{index}")
            }),
        );
        if let Some(response) = server.response_with_timeout(&id, Duration::from_millis(100)) {
            rejected = response.error;
            break;
        }
        barrier.wait_for_entries(index + 1);
        server.send_notification("$/cancelRequest", json!({"id": id}));
        let response = server.response(&id);
        assert_eq!(
            response.error.expect("cancellation response").code,
            -32800,
            "an admitted request must be cancelled before the next replacement"
        );
        admitted += 1;
    }

    let error = rejected.expect("retiring validator admission must eventually be bounded");
    assert_eq!(error.code, -32803);
    assert_eq!(
        admitted, 34,
        "retiring validators must consume the 34-recipient admission budget"
    );
    barrier.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn server_indexing_progress_requires_create_ack_and_closes_after_diagnostics() {
    let root = tempfile::tempdir().expect("workspace");
    let root_path = root.path().to_path_buf();
    let source = root_path.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(root);
    let initialize_id = RequestId::from("server-progress-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root_path),
            "capabilities": {
                "window": {"workDoneProgress": true},
                "workspace": {"configuration": true}
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));
    let configuration = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(
        configuration.id,
        json!([{"projectFile": null}]),
    )));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );

    let create = server.request("window/workDoneProgress/create");
    let token = create.params["token"].clone();
    assert!(token.is_string(), "server progress token must be opaque");
    barrier.wait_until_entered();
    server.assert_no_progress();
    server.send(Message::Response(Response::new_ok(create.id, Value::Null)));

    let begin = match server.next_message() {
        Message::Notification(notification) => {
            assert_eq!(notification.method, "$/progress");
            notification.params
        }
        other => panic!("expected progress begin notification, got {other:?}"),
    };
    assert_eq!(begin["token"], token);
    assert_eq!(begin["value"]["kind"], "begin");
    assert_eq!(begin["value"]["title"], "Indexing workspace");
    let report = match server.next_message() {
        Message::Notification(notification) => {
            assert_eq!(notification.method, "$/progress");
            notification.params
        }
        other => panic!("expected progress report notification, got {other:?}"),
    };
    assert_eq!(report["token"], token);
    assert_eq!(report["value"]["kind"], "report");
    barrier.release();
    let diagnostic = match server.next_message() {
        Message::Notification(notification) => notification,
        other => panic!("expected diagnostics before progress end, got {other:?}"),
    };
    assert_eq!(diagnostic.method, "textDocument/publishDiagnostics");
    assert_eq!(diagnostic.params["uri"], uri(&source).to_string());
    let end = match server.next_message() {
        Message::Notification(notification) => {
            assert_eq!(notification.method, "$/progress");
            notification.params
        }
        other => panic!("expected progress end notification, got {other:?}"),
    };
    assert_eq!(end["token"], token);
    assert_eq!(end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn interleaved_configuration_and_progress_responses_route_by_request_type() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    let initialize_id = RequestId::from("interleaved-routing-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "capabilities": {
                "window": {"workDoneProgress": true},
                "workspace": {"configuration": true}
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));
    let initial_configuration = server.request("workspace/configuration");
    server.send(Message::Response(Response::new_ok(
        initial_configuration.id,
        json!([{"projectFile": null}]),
    )));

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );
    let create = server.request("window/workDoneProgress/create");
    barrier.wait_until_entered();

    server.send_notification("workspace/didChangeConfiguration", json!({"settings": {}}));
    let refreshed_configuration = server.request("workspace/configuration");
    assert_eq!(refreshed_configuration.method, "workspace/configuration");

    // Deliver the progress-create response while a Task20 configuration
    // request is pending, then deliver the configuration response.  The
    // response IDs must be routed by their request registries, not by arrival
    // order or generic error handling.
    server.send(Message::Response(Response::new_ok(create.id, Value::Null)));
    server.send(Message::Response(Response::new_ok(
        refreshed_configuration.id,
        json!([{"projectFile": null}]),
    )));
    let begin = server.notification("$/progress");
    assert_eq!(begin["value"]["kind"], "begin");
    let token = begin["token"].clone();
    let report = server.notification("$/progress");
    assert_eq!(report["token"], token);
    assert_eq!(report["value"]["kind"], "report");

    barrier.release();
    assert!(
        server
            .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
            .is_some(),
        "interleaved configuration must not block diagnostics"
    );
    let end = server.notification("$/progress");
    assert_eq!(end["token"], token);
    assert_eq!(end["value"]["kind"], "end");
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn late_server_progress_create_ack_after_success_is_ignored() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    let initialize_id = RequestId::from("late-create-success-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "capabilities": {"window": {"workDoneProgress": true}}
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );

    let create = server.request("window/workDoneProgress/create");
    let token = create.params["token"].clone();
    barrier.wait_until_entered();
    barrier.release();
    assert!(
        server
            .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
            .is_some(),
        "diagnostics must complete while create acknowledgement is pending"
    );

    // The computation is terminal before the client acknowledges creation.
    // Synchronize after sending the late response so the assertion observes
    // the server's response-dispatch path, not merely the outbound queue.
    server.send(Message::Response(Response::new_ok(create.id, Value::Null)));
    let sync_id = RequestId::from("late-create-success-sync".to_string());
    server.send_request(
        sync_id.clone(),
        "workspace/symbol",
        json!({"query": "Main"}),
    );
    let sync = server.response(&sync_id);
    assert!(sync.error.is_none(), "sync request failed: {sync:?}");
    server.assert_no_progress();
    assert!(!token.is_null(), "server create token must be present");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn late_server_progress_create_ack_after_invalidation_is_ignored() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    let initialize_id = RequestId::from("late-create-invalidation-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "capabilities": {"window": {"workDoneProgress": true}}
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );

    let create = server.request("window/workDoneProgress/create");
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&source)}}),
    );
    let cleared = server
        .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
        .expect("closing the document must clear diagnostics");
    assert!(
        cleared["diagnostics"].as_array().is_some_and(Vec::is_empty),
        "close must publish an empty diagnostic set: {cleared:?}"
    );
    barrier.release();

    // Invalidation retires the pending create as well as the worker target;
    // acknowledging it later must not resurrect an active lifecycle.
    server.send(Message::Response(Response::new_ok(create.id, Value::Null)));
    let sync_id = RequestId::from("late-create-invalidation-sync".to_string());
    server.send_request(
        sync_id.clone(),
        "workspace/symbol",
        json!({"query": "Main"}),
    );
    let sync = server.response(&sync_id);
    assert!(sync.error.is_none(), "sync request failed: {sync:?}");
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn denied_duplicate_and_unknown_progress_create_responses_are_harmless() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize_with_progress(&root);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );
    let create = server.request("window/workDoneProgress/create");
    barrier.wait_until_entered();

    server.send(Message::Response(Response::new_err(
        create.id.clone(),
        -32603,
        "progress denied".to_string(),
    )));
    // A duplicate response for a denied request and an unrelated response ID
    // must not be routed to configuration, watcher, or analysis state.
    server.send(Message::Response(Response::new_ok(create.id, Value::Null)));
    server.send(Message::Response(Response::new_ok(
        RequestId::from("pascal-lsp-progress-create-unknown".to_string()),
        Value::Null,
    )));

    barrier.release();
    assert!(
        server
            .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
            .is_some(),
        "denied progress creation must not block diagnostics"
    );
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_server_diagnostic_progress_discards_old_result_and_retries() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = concat!(
        "unit Main;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var I: Integer; B: Boolean;\n",
        "begin\n",
        "  B := I;\n",
        "end;\n",
        "end.\n",
    );
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize_with_progress(&root);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );

    let first_create = server.request("window/workDoneProgress/create");
    let first_token = first_create.params["token"].clone();
    barrier.wait_until_entered();
    server.send(Message::Response(Response::new_ok(
        first_create.id,
        Value::Null,
    )));
    let first_begin = server.notification("$/progress");
    assert_eq!(first_begin["token"], first_token);
    assert_eq!(first_begin["value"]["kind"], "begin");
    let first_report = server.notification("$/progress");
    assert_eq!(first_report["token"], first_token);
    assert_eq!(first_report["value"]["kind"], "report");

    server.send_notification(
        "window/workDoneProgress/cancel",
        json!({"token": first_token}),
    );
    let first_end = server.notification("$/progress");
    assert_eq!(first_end["token"], first_token);
    assert_eq!(first_end["value"]["kind"], "end");
    server.assert_no_notification("textDocument/publishDiagnostics");

    let second_create = server.request("window/workDoneProgress/create");
    let second_token = second_create.params["token"].clone();
    assert_ne!(
        first_token, second_token,
        "a cancelled token must not be reused"
    );
    server.assert_no_notification("textDocument/publishDiagnostics");
    server.send(Message::Response(Response::new_ok(
        second_create.id,
        Value::Null,
    )));
    barrier.release();

    let diagnostics = server
        .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
        .expect("fresh diagnostic retry must publish once");
    assert_eq!(diagnostics["uri"], uri(&source).to_string());
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("fresh diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "pascal-type-mismatch"),
        "the retried computation must retain the type diagnostic: {diagnostics}"
    );
    let second_begin = server.notification("$/progress");
    assert_eq!(second_begin["token"], second_token);
    assert_eq!(second_begin["value"]["kind"], "begin");
    let second_report = server.notification("$/progress");
    assert_eq!(second_report["token"], second_token);
    assert_eq!(second_report["value"]["kind"], "report");
    let second_end = server.notification("$/progress");
    assert_eq!(second_end["token"], second_token);
    assert_eq!(second_end["value"]["kind"], "end");
    server.assert_no_notification("textDocument/publishDiagnostics");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rapid_diagnostic_invalidation_retires_old_progress_before_fresh_retry() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let initial = "unit Main;\ninterface\nimplementation\nend.\n";
    let changed = "unit Main;\ninterface\nprocedure Run;\nimplementation\nend.\n";
    write_file(&source, initial);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize_with_progress(&root);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": initial
            }
        }),
    );

    let first_create = server.request("window/workDoneProgress/create");
    let first_token = first_create.params["token"].clone();
    barrier.wait_until_entered();
    server.send(Message::Response(Response::new_ok(
        first_create.id,
        Value::Null,
    )));
    let first_begin = server.notification("$/progress");
    assert_eq!(first_begin["token"], first_token);
    assert_eq!(first_begin["value"]["kind"], "begin");
    let first_report = server.notification("$/progress");
    assert_eq!(first_report["token"], first_token);
    assert_eq!(first_report["value"]["kind"], "report");

    // Two source invalidations arrive before the cancelled worker is released;
    // only the live diagnostic computation may own the first token.
    for version in [2, 3] {
        server.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri(&source), "version": version},
                "contentChanges": [{"text": format!("{changed}\n// version {version}\n")}]
            }),
        );
    }
    let sync_id = RequestId::from("rapid-invalidation-sync".to_string());
    server.send_request(sync_id.clone(), "workspace/symbol", json!({"query": "Run"}));
    let sync = server.response(&sync_id);
    assert!(
        sync.error.is_none(),
        "invalidation synchronization failed: {sync:?}"
    );
    let first_end = server.notification("$/progress");
    assert_eq!(first_end["token"], first_token);
    assert_eq!(first_end["value"]["kind"], "end");

    // Force the fresh retry to use the same deterministic worker barrier;
    // the cancelled worker exits by observing its flag, while the retry waits
    // for an explicit release below.
    let _ = fs::remove_file(&barrier.release);
    let second_create = server.request("window/workDoneProgress/create");
    let second_token = second_create.params["token"].clone();
    assert_ne!(first_token, second_token);
    server.send(Message::Response(Response::new_ok(
        second_create.id,
        Value::Null,
    )));
    let second_begin = server.notification("$/progress");
    assert_eq!(second_begin["token"], second_token);
    assert_eq!(second_begin["value"]["kind"], "begin");
    let second_report = server.notification("$/progress");
    assert_eq!(second_report["token"], second_token);
    assert_eq!(second_report["value"]["kind"], "report");

    barrier.release();
    let diagnostics = server
        .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
        .expect("fresh retry must publish diagnostics");
    assert_eq!(diagnostics["uri"], uri(&source).to_string());
    assert_eq!(
        diagnostics["version"], 3,
        "retry must publish the newest version"
    );
    let second_end = server.notification("$/progress");
    assert_eq!(second_end["token"], second_token);
    assert_eq!(second_end["value"]["kind"], "end");
    server.assert_no_progress();
    server.assert_no_notification("textDocument/publishDiagnostics");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_retires_unacknowledged_server_progress_creation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize_with_progress(&root);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );
    let _create = server.request("window/workDoneProgress/create");
    barrier.wait_until_entered();

    server.shutdown();
    server.assert_no_progress();
}

#[cfg(feature = "test-support")]
#[test]
fn active_client_token_collision_isolated_and_reusable_after_completion() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize_with_progress(&root);
    let token = json!(-37);

    let first_id = RequestId::from("token-collision-first".to_string());
    let mut first_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    first_params["workDoneToken"] = token.clone();
    server.send_request(first_id.clone(), "textDocument/definition", first_params);
    barrier.wait_until_entered();
    let first_begin = server.notification("$/progress");
    assert_eq!(first_begin["token"], token);
    let first_report = server.notification("$/progress");
    assert_eq!(first_report["token"], token);

    let second_id = RequestId::from("token-collision-second".to_string());
    let mut second_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    second_params["workDoneToken"] = token.clone();
    server.send_request(second_id.clone(), "textDocument/definition", second_params);
    server.assert_no_progress();

    server.send_notification("window/workDoneProgress/cancel", json!({"token": -37}));
    let first_response = server.response(&first_id);
    assert_eq!(
        first_response.error.expect("first cancellation").code,
        -32800
    );
    let first_end = server.notification("$/progress");
    assert_eq!(first_end["token"], token);
    assert_eq!(first_end["value"]["kind"], "end");

    barrier.release();
    let second_response = server.response(&second_id);
    assert!(
        second_response.error.is_none(),
        "surviving coalesced request failed: {second_response:?}"
    );
    server.assert_no_progress();

    let third_id = RequestId::from("token-collision-reuse".to_string());
    let mut third_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    third_params["workDoneToken"] = token.clone();
    server.send_request(third_id.clone(), "textDocument/definition", third_params);
    let third_begin = server.notification("$/progress");
    assert_eq!(third_begin["token"], token);
    let third_report = server.notification("$/progress");
    assert_eq!(third_report["token"], token);
    let third_response = server.response(&third_id);
    assert!(
        third_response.error.is_none(),
        "reuse request failed: {third_response:?}"
    );
    let third_end = server.notification("$/progress");
    assert_eq!(third_end["token"], token);
    assert_eq!(third_end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn ignored_server_progress_creation_is_bounded_without_blocking_analysis() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = root.join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let mut server = TestServer::launch_with_environment(environment);
    server.initialize_with_progress(&root);
    let mut creates = Vec::new();
    for version in 1..=33 {
        if version == 1 {
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&source),
                        "languageId": "pascal",
                        "version": version,
                        "text": text
                    }
                }),
            );
        } else {
            server.send_notification(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri(&source), "version": version},
                    "contentChanges": [{"text": format!("{text}\n{version}")}]
                }),
            );
        }
        if version <= 32 {
            creates.push(server.request("window/workDoneProgress/create"));
        }
        assert!(
            server
                .diagnostic_with_timeout(&uri(&source), IO_TIMEOUT)
                .is_some(),
            "diagnostic {version} must complete without a create acknowledgement"
        );
    }
    assert_eq!(creates.len(), 32);
    assert!(
        creates
            .iter()
            .all(|request| request.method == "window/workDoneProgress/create")
    );

    // Wait for diagnostic 33 before checking the create channel.  Any create
    // request for that operation is emitted at dispatch, before its
    // diagnostic publication, and is therefore either buffered here or
    // observed before the synchronized response below.  This avoids using a
    // short absence window ahead of the workspace's diagnostic debounce.
    let inspect_id = RequestId::from("ignored-create-bound-inspect".to_string());
    server.send_request(
        inspect_id.clone(),
        "workspace/symbol",
        json!({"query": "Main"}),
    );
    let inspect = server.response(&inspect_id);
    assert!(
        inspect.error.is_none(),
        "post-diagnostic synchronization failed: {inspect:?}"
    );
    let extra_creates = server.pending_request_count("window/workDoneProgress/create");
    assert_eq!(
        creates.len() + extra_creates,
        32,
        "all emitted create requests must be counted after diagnostic completion"
    );
    server.assert_no_request("window/workDoneProgress/create");

    // A late acknowledgement for a terminal operation is consumed as a
    // bounded tombstone, not as a new visible lifecycle.
    server.send(Message::Response(Response::new_ok(
        creates[0].id.clone(),
        Value::Null,
    )));
    let sync_id = RequestId::from("ignored-create-bound-sync".to_string());
    server.send_request(
        sync_id.clone(),
        "workspace/symbol",
        json!({"query": "Main"}),
    );
    let sync = server.response(&sync_id);
    assert!(sync.error.is_none(), "sync request failed: {sync:?}");
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn client_progress_token_collision_does_not_claim_server_diagnostic_progress() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let other = root.join("Other.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let other_source = "unit Other;\ninterface\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);
    write_file(&other, other_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize_with_progress(&root);
    let request_id = RequestId::from("client-server-token-collision".to_string());
    let mut params = navigation_params(&main, main_source, "PublicRoutine", 0);
    params["workDoneToken"] = json!("pascal-lsp-progress-1");
    server.send_request(request_id.clone(), "textDocument/definition", params);
    barrier.wait_until_entered();
    let begin = server.notification("$/progress");
    assert_eq!(begin["token"], "pascal-lsp-progress-1");
    let report = server.notification("$/progress");
    assert_eq!(report["token"], "pascal-lsp-progress-1");

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&other),
                "languageId": "pascal",
                "version": 1,
                "text": other_source
            }
        }),
    );
    assert!(
        server
            .diagnostic_with_timeout(&uri(&other), IO_TIMEOUT)
            .is_some(),
        "diagnostic computation must continue despite token collision"
    );
    let sync_id = RequestId::from("client-server-token-collision-sync".to_string());
    server.send_request(
        sync_id.clone(),
        "workspace/symbol",
        json!({"query": "Other"}),
    );
    let sync = server.response(&sync_id);
    assert!(
        sync.error.is_none(),
        "collision synchronization failed: {sync:?}"
    );
    // create is a JSON-RPC request, not a notification.  Check the correct
    // channel after a response synchronization so a buffered forbidden
    // request cannot hide behind a short absence window.
    server.assert_no_request("window/workDoneProgress/create");

    barrier.release();
    let response = server.response(&request_id);
    assert!(response.error.is_none(), "navigation failed: {response:?}");
    let end = server.notification("$/progress");
    assert_eq!(end["token"], "pascal-lsp-progress-1");
    assert_eq!(end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn stale_and_worker_error_results_each_end_progress_once() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let stale_id = RequestId::from("stale-progress-request".to_string());
    let mut stale_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    stale_params["workDoneToken"] = json!("stale-progress");
    server.send_request(stale_id.clone(), "textDocument/definition", stale_params);
    barrier.wait_until_entered();
    let stale_begin = server.notification("$/progress");
    assert_eq!(stale_begin["token"], "stale-progress");
    let stale_report = server.notification("$/progress");
    assert_eq!(stale_report["token"], "stale-progress");

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": main_source
            }
        }),
    );
    let sync_id = RequestId::from("stale-progress-sync".to_string());
    server.send_request(
        sync_id.clone(),
        "workspace/symbol",
        json!({"query": "PublicRoutine"}),
    );
    let sync = server.response(&sync_id);
    assert!(
        sync.error.is_none(),
        "stale synchronization failed: {sync:?}"
    );
    barrier.release();
    let stale_response = server.response(&stale_id);
    assert!(
        stale_response.error.is_some(),
        "stale result must not be delivered as success"
    );
    let stale_end = server.notification("$/progress");
    assert_eq!(stale_end["token"], "stale-progress");
    assert_eq!(stale_end["value"]["kind"], "end");
    server.assert_no_progress();

    let missing = root.join("missing.pas");
    let error_id = RequestId::from("worker-error-progress".to_string());
    server.send_request(
        error_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&missing)},
            "options": {"tabSize": 2, "insertSpaces": true},
            "workDoneToken": "worker-error-progress"
        }),
    );
    let error_begin = server.notification("$/progress");
    assert_eq!(error_begin["token"], "worker-error-progress");
    let error_report = server.notification("$/progress");
    assert_eq!(error_report["token"], "worker-error-progress");
    let error_response = server.response(&error_id);
    assert!(
        error_response.error.is_some(),
        "worker error must be returned to the request"
    );
    let error_end = server.notification("$/progress");
    assert_eq!(error_end["token"], "worker-error-progress");
    assert_eq!(error_end["value"]["kind"], "end");
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_ends_begun_client_progress_exactly_once() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("shutdown-progress-client".to_string());
    let mut params = navigation_params(&main, main_source, "PublicRoutine", 0);
    params["workDoneToken"] = json!("shutdown-progress");
    server.send_request(request_id, "textDocument/definition", params);
    barrier.wait_until_entered();
    let begin = server.notification("$/progress");
    assert_eq!(begin["token"], "shutdown-progress");
    let report = server.notification("$/progress");
    assert_eq!(report["token"], "shutdown-progress");

    let shutdown_id = RequestId::from("shutdown-progress-request".to_string());
    server.send_request(shutdown_id.clone(), "shutdown", Value::Null);
    let shutdown = server.response(&shutdown_id);
    assert!(shutdown.error.is_none(), "shutdown failed: {shutdown:?}");
    let end = server.notification("$/progress");
    assert_eq!(end["token"], "shutdown-progress");
    assert_eq!(end["value"]["kind"], "end");
    server.assert_no_progress();

    server.send_notification("exit", Value::Null);
    server.stdin.take();
    let status = server.child.wait().expect("wait for shutdown server");
    assert!(status.success(), "server exited unsuccessfully: {status}");
}

#[cfg(feature = "test-support")]
#[test]
fn client_without_server_progress_support_gets_no_indexing_progress_creation() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("Main.pas");
    let text = "unit Main;\ninterface\nimplementation\nend.\n";
    write_file(&source, text);

    let mut server = TestServer::launch();
    server.initialize(root.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source),
                "languageId": "pascal",
                "version": 1,
                "text": text
            }
        }),
    );
    let _ = server.diagnostic_with_timeout(&uri(&source), IO_TIMEOUT);
    assert!(!server.pending.iter().any(|message| {
        matches!(
            message,
            Message::Request(request) if request.method == "window/workDoneProgress/create"
        )
    }));
    assert!(!server.pending.iter().any(|message| {
        matches!(
            message,
            Message::Notification(notification) if notification.method == "$/progress"
        )
    }));
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_request_progress_closes_only_that_recipient() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    let initialize_id = RequestId::from("cancel-progress-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "capabilities": {"window": {"workDoneProgress": true}}
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));

    let request_id = RequestId::from("cancel-progress-request".to_string());
    let mut params = navigation_params(&main, main_source, "PublicRoutine", 0);
    params["workDoneToken"] = json!("cancel-me");
    server.send_request(request_id.clone(), "textDocument/definition", params);
    barrier.wait_until_entered();

    let begin = server.notification("$/progress");
    assert_eq!(begin["token"], "cancel-me");
    assert_eq!(begin["value"]["kind"], "begin");
    let report = server.notification("$/progress");
    assert_eq!(report["token"], "cancel-me");
    assert_eq!(report["value"]["kind"], "report");

    server.send_notification(
        "window/workDoneProgress/cancel",
        json!({"token": "cancel-me"}),
    );
    let response = server.response(&request_id);
    assert_eq!(response.error.expect("cancel response").code, -32800);
    let end = server.notification("$/progress");
    assert_eq!(end["token"], "cancel-me");
    assert_eq!(end["value"]["kind"], "end");

    barrier.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn coalesced_progress_tokens_remain_isolated_when_one_request_cancels() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    let initialize_id = RequestId::from("coalesced-progress-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "capabilities": {"window": {"workDoneProgress": true}}
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));

    let first_id = RequestId::from("coalesced-progress-first".to_string());
    let mut first_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    first_params["workDoneToken"] = json!("first-progress");
    server.send_request(first_id.clone(), "textDocument/definition", first_params);
    barrier.wait_until_entered();
    let first_begin = server.notification("$/progress");
    assert_eq!(first_begin["token"], "first-progress");
    let _ = server.notification("$/progress");

    let second_id = RequestId::from("coalesced-progress-second".to_string());
    let mut second_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    second_params["workDoneToken"] = json!("second-progress");
    server.send_request(second_id.clone(), "textDocument/definition", second_params);
    let second_begin = server.notification("$/progress");
    assert_eq!(second_begin["token"], "second-progress");
    let second_report = server.notification("$/progress");
    assert_eq!(second_report["token"], "second-progress");
    assert_eq!(second_report["value"]["message"], "Analysis started");

    server.send_notification("$/cancelRequest", json!({"id": second_id}));
    let second_response = server.response(&second_id);
    assert_eq!(
        second_response.error.expect("second cancellation").code,
        -32800
    );
    let second_end = server.notification("$/progress");
    assert_eq!(second_end["token"], "second-progress");
    assert_eq!(second_end["value"]["kind"], "end");

    barrier.release();
    let first_response = server.response(&first_id);
    assert!(
        first_response.error.is_none(),
        "first request failed: {first_response:?}"
    );
    let first_end = server.notification("$/progress");
    assert_eq!(first_end["token"], "first-progress");
    assert_eq!(first_end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_request_cancellation_ends_its_progress_without_dispatch() {
    run_queued_progress_cancellation(false);
}

#[cfg(feature = "test-support")]
#[test]
fn queued_work_done_progress_cancellation_ends_without_dispatch() {
    run_queued_progress_cancellation(true);
}

#[cfg(feature = "test-support")]
#[test]
fn coalesced_running_recipient_receives_started_progress() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    let initialize_id = RequestId::from("coalesced-running-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "capabilities": {"window": {"workDoneProgress": true}}
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    server.send_notification("initialized", json!({}));

    let first_id = RequestId::from("coalesced-running-first".to_string());
    let mut first_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    first_params["workDoneToken"] = json!("coalesced-running-first-progress");
    server.send_request(first_id.clone(), "textDocument/definition", first_params);
    barrier.wait_until_entered();
    let first_begin = server.notification("$/progress");
    assert_eq!(first_begin["token"], "coalesced-running-first-progress");
    let first_report = server.notification("$/progress");
    assert_eq!(first_report["token"], "coalesced-running-first-progress");
    assert_eq!(first_report["value"]["message"], "Analysis started");

    let second_id = RequestId::from("coalesced-running-second".to_string());
    let mut second_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    second_params["workDoneToken"] = json!("coalesced-running-second-progress");
    server.send_request(second_id.clone(), "textDocument/definition", second_params);
    let second_begin = server.notification("$/progress");
    assert_eq!(second_begin["token"], "coalesced-running-second-progress");
    let second_report = server.notification("$/progress");
    assert_eq!(second_report["token"], "coalesced-running-second-progress");
    assert_eq!(second_report["value"]["kind"], "report");
    assert_eq!(second_report["value"]["message"], "Analysis started");

    barrier.release();
    let first_response = server.response(&first_id);
    assert!(
        first_response.error.is_none(),
        "first request failed: {first_response:?}"
    );
    let first_end = server.notification("$/progress");
    assert_eq!(first_end["token"], "coalesced-running-first-progress");
    assert_eq!(first_end["value"]["kind"], "end");
    let second_response = server.response(&second_id);
    assert!(
        second_response.error.is_none(),
        "second request failed: {second_response:?}"
    );
    let second_end = server.notification("$/progress");
    assert_eq!(second_end["token"], "coalesced-running-second-progress");
    assert_eq!(second_end["value"]["kind"], "end");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn selection_range_cancellation_returns_the_standard_request_canceled_error() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("SelectionCancel.pas");
    let source = "unit SelectionCancel;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := 1;\nend;\nend.\n";
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_selection_barrier(temp);
    server.initialize(source_path.parent().expect("workspace root"), Value::Null);
    let id = RequestId::from("selection-cancelled".to_string());
    server.send_request(
        id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(source, "Value", 0)]
        }),
    );
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": "selection-cancelled"}));
    let error = server
        .response(&id)
        .error
        .expect("cancelled selection request must fail");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn blocked_selection_ranges_do_not_block_unrelated_lsp_requests() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("SelectionConcurrent.pas");
    let source = "unit SelectionConcurrent;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := Other.Bar[0];\nend;\nend.\n";
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_selection_barrier(environment);
    server.initialize(&root, Value::Null);
    let selection_id = RequestId::from("blocked-selection".to_string());
    server.send_request(
        selection_id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(source, "Bar", 0)]
        }),
    );
    barrier.wait_until_entered();

    let symbols_id = RequestId::from("while-selection-is-blocked".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&source_path)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none() && symbols.result.is_some(),
        "unrelated request was blocked by selection ranges: {symbols:?}"
    );

    barrier.release();
    let selection = server.response(&selection_id);
    assert!(
        selection.error.is_none() && selection.result.is_some(),
        "selection range request failed after release: {selection:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn requested_open_document_change_discards_blocked_selection_ranges() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("SelectionStale.pas");
    let first_source = "unit SelectionStale;\ninterface\nimplementation\nprocedure Run;\nbegin\n  Value := 1;\nend;\nend.\n";
    let second_source = first_source.replace("Value", "ChangedValue");
    write_file(&source_path, first_source);

    let (mut server, barrier) = TestServer::launch_with_selection_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );

    let request_id = RequestId::from("stale-selection".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "positions": [position_of(first_source, "Value", 0)]
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("requested source change must stale selection ranges");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelled_inlay_request_returns_cancellation_error() {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace");
    let source_path = root.join("CancelledInlay.pas");
    let source = "unit CancelledInlay;\ninterface\nprocedure Draw(Value: Integer);\nimplementation\nprocedure Draw(Value: Integer);\nbegin end;\nprocedure Run;\nbegin Draw(1); end;\nend.\n";
    write_file(&source_path, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    let id = RequestId::from("cancel-inlay-request".to_string());
    server.send_request(
        id.clone(),
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 0}}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": id}));
    let response = server.response(&id);
    let error = response
        .error
        .expect("cancelled inlay request must not return a result");
    assert_eq!(error.code, -32800);
    barrier.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_inlay_result_is_rejected_after_open_document_edit() {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace");
    let source_path = root.join("StaleInlay.pas");
    let original = "unit StaleInlay;\ninterface\nprocedure Draw(OldName: Integer);\nimplementation\nprocedure Draw(OldName: Integer);\nbegin end;\nprocedure Run;\nbegin Draw(1); end;\nend.\n";
    let edited = original.replace("OldName", "NewName");
    write_file(&source_path, original);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&source_path), "languageId": "pascal", "version": 1, "text": original
        }}),
    );
    let id = RequestId::from("stale-inlay-result".to_string());
    server.send_request(
        id.clone(),
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 0}}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&source_path), "version": 2},
            "contentChanges": [{"text": edited}]
        }),
    );
    barrier.release();
    let response = server.response(&id);
    let error = response
        .error
        .expect("stale inlay result must not be delivered");
    assert_eq!(error.code, -32803);
    assert!(
        error.message.contains("stale"),
        "unexpected stale-result error: {error:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_resolution_cancellation_returns_once_while_worker_is_in_flight() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("CancelledCompletion.pas");
    let source = "unit CancelledCompletion;\ninterface\n/// <summary>Cancellation documentation.</summary>\nfunction CancelledDocumented: Integer;\nimplementation\nfunction CancelledDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  CancelledDoc\nend;\nend.\n";
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_completion_resolution_barrier(environment);
    server.initialize_with_completion_resolve_properties(
        &root,
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("cancelled-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  CancelledDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("cancelled completion result");
    let item = initial["items"]
        .as_array()
        .expect("cancelled completion items")
        .iter()
        .find(|item| item["label"] == "CancelledDocumented")
        .cloned()
        .expect("cancelled documented item");

    let resolve_id = RequestId::from("cancelled-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    barrier.wait_until_entered();
    server.send_notification("$/cancelRequest", json!({"id": resolve_id.clone()}));
    let response = server.response(&resolve_id);
    let error = response.error.expect("cancelled resolve error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    barrier.release();
    server.assert_no_response(&resolve_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_resolution_rejects_stale_non_cancelled_worker_delivery() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source_path = root.join("StaleCompletion.pas");
    let source = "unit StaleCompletion;\ninterface\n/// <summary>Original documentation.</summary>\nfunction OriginalDocumented: Integer;\nimplementation\nfunction OriginalDocumented: Integer;\nbegin\n  Result := 1;\nend;\nprocedure Caller;\nbegin\n  OriginalDoc\nend;\nend.\n";
    let changed_source = source.replace("OriginalDoc", "ChangedDoc");
    write_file(&source_path, source);

    let (mut server, barrier) = TestServer::launch_with_completion_resolution_barrier(environment);
    server.initialize_with_completion_resolve_properties(
        &root,
        json!(["documentation", "detail"]),
        json!(["markdown"]),
    );
    let completion_id = RequestId::from("stale-completion".to_string());
    server.send_request(
        completion_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&source_path)},
            "position": position_after(source, "  OriginalDoc", 0)
        }),
    );
    let initial = server
        .response(&completion_id)
        .result
        .expect("stale completion result");
    let item = initial["items"]
        .as_array()
        .expect("stale completion items")
        .iter()
        .find(|item| item["label"] == "OriginalDocumented")
        .cloned()
        .expect("stale completion item");

    let resolve_id = RequestId::from("stale-completion-resolve".to_string());
    server.send_request(resolve_id.clone(), "completionItem/resolve", item);
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&source_path),
                "languageId": "pascal",
                "version": 2,
                "text": changed_source
            }
        }),
    );
    barrier.release();
    let response = server.response(&resolve_id);
    let error = response
        .error
        .expect("a changed workspace must reject the non-cancelled worker result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn completion_auto_import_resolution_rejects_provider_set_changes_during_worker() {
    let provider_source =
        "unit Provider;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n";
    let consumer_source = concat!(
        "unit Consumer;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var Value: TTarget;\n",
        "begin Value := nil; end;\n",
        "end.\n",
    );
    let negative_source = "unit Other;\ninterface\nimplementation\nend.\n";

    for (case_name, change, new_overlay, expected_resolve_error, expect_fresh_item) in [
        (
            "negative-unit",
            Some("unit Provider;\ninterface\nimplementation\nend.\n"),
            None,
            true,
            false,
        ),
        (
            "negative-symbol",
            Some("unit Other;\ninterface\ntype TTargetType = class end;\nimplementation\nend.\n"),
            None,
            true,
            false,
        ),
        (
            "new-overlay",
            None,
            Some(("New.pas", "unit Provider; interface implementation end.\n")),
            true,
            false,
        ),
        (
            "unrelated-overlay",
            Some("unit Other;\ninterface\nimplementation\nend.\n// unrelated\n"),
            None,
            false,
            true,
        ),
    ] {
        let environment = tempfile::tempdir().expect("isolated server environment");
        let root = environment.path().join("workspace");
        fs::create_dir_all(&root).expect("workspace root");
        let provider_path = root.join("Provider.pas");
        let consumer_path = root.join("Consumer.pas");
        let other_path = root.join("Other.pas");
        write_file(&provider_path, provider_source);
        write_file(&consumer_path, consumer_source);
        write_file(&other_path, negative_source);

        let (mut server, barrier) =
            TestServer::launch_with_completion_resolution_barrier(environment);
        server.initialize_with_completion_resolve_properties(
            &root,
            json!(["documentation", "detail"]),
            json!(["markdown"]),
        );
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&other_path),
                    "languageId": "pascal",
                    "version": 1,
                    "text": negative_source,
                }
            }),
        );
        let completion_id = RequestId::from(format!("provider-set-{case_name}-completion"));
        server.send_request(
            completion_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&consumer_path)},
                "position": position_after(consumer_source, "TTarget", 0),
            }),
        );
        let initial = server
            .response(&completion_id)
            .result
            .expect("provider-set completion result");
        let item = initial["items"]
            .as_array()
            .expect("provider-set completion items")
            .iter()
            .find(|item| item["label"] == "TTargetType")
            .cloned()
            .unwrap_or_else(|| panic!("provider-set candidate missing in {case_name}: {initial}"));

        let resolve_id = RequestId::from(format!("provider-set-{case_name}-resolve"));
        server.send_request(resolve_id.clone(), "completionItem/resolve", item);
        barrier.wait_until_entered();
        if let Some(changed) = change {
            server.send_notification(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri(&other_path), "version": 2},
                    "contentChanges": [{"text": changed}],
                }),
            );
        }
        if let Some((name, text)) = new_overlay {
            let path = root.join(name);
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&path),
                        "languageId": "pascal",
                        "version": 1,
                        "text": text,
                    }
                }),
            );
        }
        // An unknown request is handled synchronously by the protocol loop;
        // its response establishes that the preceding overlay notification was
        // consumed while the resolve worker is paused.
        let sync_id = RequestId::from(format!("provider-set-{case_name}-sync"));
        server.send_request(sync_id.clone(), "review/sync", json!({}));
        let sync = server.response(&sync_id);
        assert!(
            sync.error.is_some(),
            "sync probe unexpectedly succeeded: {sync:?}"
        );
        barrier.release();
        let resolved = server.response(&resolve_id);
        assert_eq!(
            resolved.error.is_some(),
            expected_resolve_error,
            "resolve result for {case_name}: {resolved:?}"
        );

        let fresh_id = RequestId::from(format!("provider-set-{case_name}-fresh"));
        server.send_request(
            fresh_id.clone(),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri(&consumer_path)},
                "position": position_after(consumer_source, "TTarget", 0),
            }),
        );
        let fresh = server
            .response(&fresh_id)
            .result
            .expect("fresh provider-set completion result");
        let has_item = fresh["items"]
            .as_array()
            .expect("fresh provider-set completion items")
            .iter()
            .any(|item| item["label"] == "TTargetType");
        assert_eq!(
            has_item, expect_fresh_item,
            "fresh result for {case_name}: {fresh}"
        );
        server.shutdown();
    }
}

#[cfg(feature = "test-support")]
#[test]
fn completion_auto_import_discovery_reports_a_bounded_catalogue_as_incomplete() {
    let environment = tempfile::tempdir().expect("isolated test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace root");
    let main_path = root.join("BoundedConsumer.pas");
    write_file(
        &root.join("BoundedProvider.pas"),
        "unit BoundedProvider;\ninterface\ntype\n  TBoundedType = class\n  end;\nimplementation\nend.\n",
    );
    let main_source = "unit BoundedConsumer;\ninterface\nimplementation\nprocedure Run;\nvar\n  Value: TBounded;\nbegin\n  Value := Value;\nend;\nend.\n";
    write_file(&main_path, main_source);

    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier_and_filename_catalogue_limit(environment, 1);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("auto-import-bounded".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri(&main_path)},
            "position": position_after(main_source, "TBounded", 0),
        }),
    );
    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "bounded completion failed: {response:?}"
    );
    let result = response.result.expect("bounded completion result");
    assert_eq!(result["isIncomplete"], true);
    assert!(
        result["items"]
            .as_array()
            .expect("bounded completion items")
            .iter()
            .all(|item| item["label"] != "TBoundedType")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn navigation_rejects_a_changed_include_overlay_before_delivery() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let shared = root.join("Shared.inc");
    let main_source = "unit Main;\ninterface\n{$I Shared.inc}\nimplementation\nprocedure Run;\nbegin\n  SharedValue := 1;\nend;\nend.\n";
    let shared_v1 = "const SharedValue = 1;\n";
    let shared_v2 = "\n\nconst SharedValue = 1;\n";
    write_file(&main, main_source);
    write_file(&shared, shared_v1);
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    for (path, source, version) in [(&shared, shared_v1, 1), (&main, main_source, 1)] {
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(path),
                    "languageId": "pascal",
                    "version": version,
                    "text": source
                }
            }),
        );
    }
    let id = RequestId::from("stale-include-navigation".to_string());
    server.send_request(
        id.clone(),
        "textDocument/declaration",
        navigation_params(&main, main_source, "SharedValue", 0),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&shared), "version": 2},
            "contentChanges": [{"text": shared_v2}]
        }),
    );
    let symbols_id = RequestId::from("synchronize-new-include-position".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "symbol synchronization failed: {symbols:?}"
    );
    assert!(
        symbols.result.is_some(),
        "symbol synchronization returned no result: {symbols:?}"
    );
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "stale include navigation must be rejected"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn configured_navigation_rejects_a_case_insensitive_competing_provider_overlay() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let competitor = root.join("PROVIDER.pas");
    let project = root.join("App.dproj");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Foo;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure Foo;\nimplementation\nprocedure Foo;\nbegin\nend;\nend.\n";
    write_file(&main, main_source);
    write_file(&provider, provider_source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": main_source
            }
        }),
    );

    let id = RequestId::from("configured-stale-provider-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Foo", 0),
    );
    barrier.wait_until_entered();

    let competitor_source = "unit Provider;\ninterface\nprocedure Foo;\nimplementation\nprocedure Foo;\nbegin\nend;\nend.\n";
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&competitor),
                "languageId": "pascal",
                "version": 1,
                "text": competitor_source
            }
        }),
    );
    let synchronization_id =
        RequestId::from("configured-provider-overlay-synchronized".to_string());
    server.send_request(
        synchronization_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(server.response(&synchronization_id).error.is_none());

    barrier.release();
    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "a stale configured-project definition must not publish the old provider: {response:?}"
    );

    let fresh_id = RequestId::from("configured-ambiguous-provider-definition".to_string());
    server.send_request(
        fresh_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Foo", 0),
    );
    assert!(result_locations(server.response(&fresh_id)).is_empty());
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn file_operation_batches_bound_uri_bytes_and_discard_on_entry_overflow() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let main = root.path().join("Main.pas");
    write_file(&main, "unit Main;\ninterface\nimplementation\nend.\n");
    let mut server = TestServer::launch();
    server.initialize_with_pull_diagnostics(root.path());

    let long_component = "a".repeat(120);
    let oversized: Vec<_> = (0..64)
        .map(|index| {
            root.path()
                .join(&long_component)
                .join(&long_component)
                .join(&long_component)
                .join(&long_component)
                .join(format!("File{index}.pas"))
        })
        .collect();
    let total_uri_bytes: usize = oversized.iter().map(|path| uri(path).as_str().len()).sum();
    assert!(total_uri_bytes > 32 * 1024);
    server.send_notification(
        "workspace/didCreateFiles",
        json!({"files":oversized.iter().map(|path| json!({"uri":uri(path)})).collect::<Vec<_>>()}),
    );
    let probe_id = RequestId::from("file-operation-byte-budget-probe".to_string());
    server.send_request(
        probe_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":null}),
    );
    let probe = server.response(&probe_id);
    assert!(
        probe.error.is_none(),
        "in-count URI-byte overflow discards derived state without fencing (TASK-97): {probe:?}"
    );
    let byte_refresh = server.request("workspace/diagnostic/refresh");
    server.send(Message::Response(Response::new_ok(
        byte_refresh.id,
        Value::Null,
    )));

    let too_many: Vec<_> = (0..65)
        .map(|index| root.path().join(format!("OverLimit{index}.pas")))
        .collect();
    server.send_notification(
        "workspace/didCreateFiles",
        json!({"files":too_many.iter().map(|path| json!({"uri":uri(path)})).collect::<Vec<_>>()}),
    );
    let count_probe_id = RequestId::from("file-operation-count-budget-probe".to_string());
    server.send_request(
        count_probe_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":null}),
    );
    let probe = server.response(&count_probe_id);
    assert!(
        probe.error.is_none(),
        "an over-limit endpoint list discards derived state without fencing: {probe:?}"
    );
    let count_refresh = server.request("workspace/diagnostic/refresh");
    server.send(Message::Response(Response::new_ok(
        count_refresh.id,
        Value::Null,
    )));

    server.send_notification(
        "workspace/didDeleteFiles",
        json!({"files":too_many.iter().map(|path| json!({"uri":uri(path)})).collect::<Vec<_>>()}),
    );
    let delete_probe_id = RequestId::from("file-delete-count-budget-probe".to_string());
    server.send_request(
        delete_probe_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":null}),
    );
    let probe = server.response(&delete_probe_id);
    assert!(
        probe.error.is_none(),
        "an oversized delete batch discards derived state without fencing: {probe:?}"
    );
    let delete_refresh = server.request("workspace/diagnostic/refresh");
    server.send(Message::Response(Response::new_ok(
        delete_refresh.id,
        Value::Null,
    )));

    let mut oversized_renames = Vec::new();
    let long_component = "r".repeat(178);
    for index in 0..5 {
        let mut old_path = root.path().join(format!("old{index}"));
        let mut new_path = root.path().join(format!("new{index}"));
        for _ in 0..20 {
            old_path.push(&long_component);
            new_path.push(&long_component);
        }
        old_path.push("From.pas");
        new_path.push("To.pas");
        oversized_renames.push(json!({"oldUri":uri(&old_path),"newUri":uri(&new_path)}));
    }
    let rename_uri_bytes: usize = oversized_renames
        .iter()
        .map(|rename| {
            rename["oldUri"].as_str().expect("old URI").len()
                + rename["newUri"].as_str().expect("new URI").len()
        })
        .sum();
    assert!(rename_uri_bytes > 32 * 1024);
    server.send_notification(
        "workspace/didRenameFiles",
        json!({"files":oversized_renames}),
    );
    let rename_probe_id = RequestId::from("file-rename-byte-budget-probe".to_string());
    server.send_request(
        rename_probe_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":null}),
    );
    let probe = server.response(&rename_probe_id);
    assert!(
        probe.error.is_none(),
        "an oversized rename URI batch discards derived state without fencing: {probe:?}"
    );
    let rename_refresh = server.request("workspace/diagnostic/refresh");
    server.send(Message::Response(Response::new_ok(
        rename_refresh.id,
        Value::Null,
    )));

    server.send_notification(
        "workspace/didCreateFiles",
        json!({"files":[{"uri":uri(&root.path().join("Duplicate.pas"))},{"uri":uri(&root.path().join("./Duplicate.pas"))}]}),
    );
    let malformed_probe_id = RequestId::from("file-operation-malformed-batch-probe".to_string());
    server.send_request(
        malformed_probe_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":null}),
    );
    let probe = server.response(&malformed_probe_id);
    assert!(
        probe.error.is_none(),
        "a duplicate-endpoint create batch is reconciled: {probe:?}"
    );
    let duplicate_create_refresh = server.request("workspace/diagnostic/refresh");
    server.send(Message::Response(Response::new_ok(
        duplicate_create_refresh.id,
        Value::Null,
    )));

    let small: Vec<_> = (0..64)
        .map(|index| root.path().join(format!("Generated{index}.pas")))
        .collect();
    server.send_notification(
        "workspace/didCreateFiles",
        json!({"files":small.iter().map(|path| json!({"uri":uri(path)})).collect::<Vec<_>>()}),
    );
    let refresh = server.request("workspace/diagnostic/refresh");
    server.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn queued_local_project_config_events_refresh_unopened_cross_project_owners() {
    for event in ["create", "change", "delete"] {
        let root = tempfile::tempdir().expect("workspace");
        let project_a = root.path().join("A");
        let project_b = root.path().join("B");
        fs::create_dir_all(&project_a).expect("project A directory");
        fs::create_dir_all(&project_b).expect("project B directory");
        let source_a = project_a.join("Main.pas");
        let source_b = project_b.join("Main.pas");
        write_file(&source_a, "unit Main; interface implementation end.\n");
        write_file(&source_b, "unit Main; interface implementation end.\n");
        for (directory, names) in [
            (&project_a, ["AOne.dproj", "ATwo.dproj"]),
            (&project_b, ["BOne.dproj", "BTwo.dproj"]),
        ] {
            for name in names {
                write_file(
                    &directory.join(name),
                    "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
                );
            }
        }
        let config = project_b.join(".delphilsp.json");
        if event != "create" {
            write_file(&config, r#"{"version":1,"project":"BOne.dproj"}"#);
        }

        let barrier_directory = root.path().join(format!("{event}-file-discovery-barrier"));
        fs::create_dir_all(&barrier_directory).expect("barrier directory");
        let barrier = TestBarrier {
            entered: barrier_directory.join("entered"),
            release: barrier_directory.join("release"),
        };
        let barrier_value = format!(
            "{}|{}",
            barrier.entered.display(),
            barrier.release.display()
        );
        let mut server = TestServer::launch_test_server_with_environment_path_and_variable(
            root.path(),
            Some("PASCAL_LSP_TEST_WORKSPACE_FILE_WORKER_BARRIER"),
            Some(&barrier_value),
        );
        server.initialize(root.path(), Value::Null);

        let select_a_id = RequestId::from(format!("{event}-select-a"));
        server.send_request(
            select_a_id.clone(),
            "pascal/selectProject",
            json!({
                "textDocument": {"uri": uri(&source_a)},
                "projectUri": uri(&project_a.join("AOne.dproj"))
            }),
        );
        assert!(
            server.response(&select_a_id).error.is_none(),
            "project A selection failed for {event}"
        );
        let initial_id = RequestId::from(format!("{event}-initial-b-context"));
        server.send_request(
            initial_id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&source_b)}}),
        );
        let initial = server.response(&initial_id);
        assert!(
            initial.error.is_none(),
            "initial B context failed: {initial:?}"
        );
        if event == "create" {
            assert!(initial.result.as_ref().unwrap()["selectedProjectUri"].is_null());
        } else {
            assert_eq!(
                initial.result.as_ref().unwrap()["selectedProjectUri"],
                uri(&project_b.join("BOne.dproj")).to_string(),
                "selection in project A must not suppress project B local config"
            );
        }

        let event_type = match event {
            "create" => {
                write_file(&config, r#"{"version":1,"project":"BOne.dproj"}"#);
                1
            }
            "change" => {
                let original_modified = fs::metadata(&config)
                    .expect("config metadata")
                    .modified()
                    .expect("config modified time");
                write_file(&config, r#"{"version":1,"project":"BTwo.dproj"}"#);
                fs::File::options()
                    .write(true)
                    .open(&config)
                    .expect("open config timestamp")
                    .set_times(std::fs::FileTimes::new().set_modified(original_modified))
                    .expect("restore config timestamp");
                2
            }
            "delete" => {
                fs::remove_file(&config).expect("delete local config");
                3
            }
            _ => unreachable!(),
        };
        server.send_notification(
            "workspace/didChangeWatchedFiles",
            json!({"changes": [{"uri": uri(&config), "type": event_type}]}),
        );
        barrier.wait_until_entered();

        let queued_id = RequestId::from(format!("{event}-queued-context"));
        server.send_request(
            queued_id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&source_b)}}),
        );
        assert!(
            server
                .response_with_timeout(&queued_id, Duration::from_millis(100))
                .is_none(),
            "project context must wait for the queued {event} worker"
        );
        barrier.release();
        let refreshed = server.response(&queued_id);
        assert!(
            refreshed.error.is_none(),
            "refreshed context failed after {event}: {refreshed:?}"
        );
        let context = refreshed.result.unwrap();
        match event {
            "create" => assert_eq!(
                context["selectedProjectUri"],
                uri(&project_b.join("BOne.dproj")).to_string()
            ),
            "change" => assert_eq!(
                context["selectedProjectUri"],
                uri(&project_b.join("BTwo.dproj")).to_string(),
                "same-stamp config bytes must refresh the unopened owner"
            ),
            "delete" => assert!(
                context["selectedProjectUri"].is_null(),
                "deleting local config must remove its owner"
            ),
            _ => unreachable!(),
        }

        let still_selected_a_id = RequestId::from(format!("{event}-still-selected-a"));
        server.send_request(
            still_selected_a_id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&source_a)}}),
        );
        let still_selected_a = server.response(&still_selected_a_id);
        assert_eq!(
            still_selected_a.result.unwrap()["selectedProjectUri"],
            uri(&project_a.join("AOne.dproj")).to_string(),
            "B metadata event must not steal A's explicit owner"
        );
        server.shutdown();
    }
}

#[test]
#[cfg(feature = "test-support")]
fn completed_navigation_is_delivered_while_a_watcher_batch_reconciles() {
    let root = tempfile::tempdir().expect("workspace");
    let main = root.path().join("Main.pas");
    let source = "unit Main;\ninterface\ntype\n  TTarget = class end;\n  TUse = TTarget;\nimplementation\nend.\n";
    write_file(&main, source);
    let batch = root.path().join("batch");
    fs::create_dir_all(&batch).expect("batch directory");
    let changes = (0..1_000)
        .map(|index| {
            let path = batch.join(format!("Batch{index}.pas"));
            write_file(
                &path,
                &format!("unit Batch{index}; interface implementation end.\n"),
            );
            json!({"uri": uri(&path), "type": 1})
        })
        .collect::<Vec<_>>();

    let barrier_directory = root.path().join("reconciliation-delivery-barriers");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let validation = TestBarrier {
        entered: barrier_directory.join("validation-entered"),
        release: barrier_directory.join("validation-release"),
    };
    let reconciliation = TestBarrier {
        entered: barrier_directory.join("reconciliation-entered"),
        release: barrier_directory.join("reconciliation-release"),
    };
    let validation_spec = format!(
        "{}|{}",
        validation.entered.display(),
        validation.release.display()
    );
    let reconciliation_spec = format!(
        "{}|{}",
        reconciliation.entered.display(),
        reconciliation.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_PARTIAL_VALIDATION_BARRIER",
                validation_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_FILE_WORKER_BARRIER",
                reconciliation_spec.as_str(),
            ),
        ],
    );
    server.initialize(root.path(), Value::Null);

    let id = RequestId::from("navigation-during-reconciliation".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TTarget", 1),
    );
    validation.wait_until_entered();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": changes}),
    );
    reconciliation.wait_until_entered();
    validation.release();

    let response = server
        .response_with_timeout(&id, IO_TIMEOUT)
        .expect("a result computed before the batch must not wait for reconciliation");
    let locations = result_locations(response);
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0]["uri"], uri(&main).to_string());

    // A batch over 64 entries still fences analysis once it commits (TASK-97).
    reconciliation.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
fn assert_navigation_is_delivered_before_file_notification_commit(name: &str, params: Value) {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace directory");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\ntype\n  TTarget = class end;\n  TAlias = TTarget;\nimplementation\nend.\n";
    write_file(&main, source);

    let barrier_directory = environment.path().join("precommit-delivery-barriers");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let navigation_completed = TestBarrier {
        entered: barrier_directory.join("navigation-completed"),
        release: barrier_directory.join("navigation-release"),
    };
    let reconciliation = TestBarrier {
        entered: barrier_directory.join("reconciliation-entered"),
        release: barrier_directory.join("reconciliation-release"),
    };
    let navigation_spec = format!(
        "{}|{}",
        navigation_completed.entered.display(),
        navigation_completed.release.display()
    );
    let reconciliation_spec = format!(
        "{}|{}",
        reconciliation.entered.display(),
        reconciliation.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        environment.path(),
        [
            (
                "PASCAL_LSP_TEST_PARTIAL_VALIDATION_BARRIER",
                navigation_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_FILE_WORKER_BARRIER",
                reconciliation_spec.as_str(),
            ),
        ],
    );
    server.initialize(&root, Value::Null);

    let id = RequestId::from(format!("precommit-fast-{name}"));
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TTarget", 1),
    );
    navigation_completed.wait_until_entered();
    server.send_notification("workspace/didRenameFiles", params);
    reconciliation.wait_until_entered();
    navigation_completed.release();

    let response = server
        .response_with_timeout(&id, IO_TIMEOUT)
        .unwrap_or_else(|| panic!("{name} navigation must be delivered while the worker is held"));
    assert!(response.error.is_none(), "{name}: {response:?}");
    let locations = result_locations(response);
    assert_eq!(locations.len(), 1, "{name}: {locations:?}");
    assert_eq!(locations[0]["uri"], uri(&main).to_string(), "{name}");
    assert!(
        !reconciliation.release.exists(),
        "{name} response arrived only while reconciliation remained held"
    );

    reconciliation.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
fn padded_file_uri(label: &str, target_len: usize) -> String {
    let mut uri = format!("file:///precommit/{label}");
    while uri.len() < target_len {
        uri.push('/');
        let component_len = (target_len - uri.len()).min(160);
        uri.extend(std::iter::repeat_n('x', component_len));
    }
    assert_eq!(uri.len(), target_len);
    uri
}

#[cfg(feature = "test-support")]
#[test]
fn bounded_rename_shapes_preserve_precommit_read_only_delivery() {
    assert_navigation_is_delivered_before_file_notification_commit(
        "empty-rename",
        json!({"files": []}),
    );
    assert_navigation_is_delivered_before_file_notification_commit(
        "noop-rename",
        json!({"files": [{"oldUri": "file:///tmp/Unchanged.pas", "newUri": "file:///tmp/Unchanged.pas"}]}),
    );
    assert_navigation_is_delivered_before_file_notification_commit(
        "ordinary-rename",
        json!({"files": [{"oldUri": "file:///tmp/Old.pas", "newUri": "file:///tmp/New.pas"}]}),
    );

    let files = (0..8)
        .map(|index| {
            json!({
                "oldUri": padded_file_uri(&format!("old-{index}"), 2_048),
                "newUri": padded_file_uri(&format!("new-{index}"), 2_048),
            })
        })
        .collect::<Vec<_>>();
    let uri_bytes = files
        .iter()
        .map(|file| file["oldUri"].as_str().unwrap().len() + file["newUri"].as_str().unwrap().len())
        .sum::<usize>();
    assert_eq!(files.len(), 8);
    assert_eq!(
        uri_bytes,
        32 * 1024,
        "exercise the inclusive existing URI cap"
    );
    assert!(files.iter().all(|file| {
        file["oldUri"].as_str().unwrap().len() <= 4_096
            && file["newUri"].as_str().unwrap().len() <= 4_096
    }));
    assert_navigation_is_delivered_before_file_notification_commit(
        "near-cap-rename",
        json!({"files": files}),
    );
}

#[test]
#[cfg(feature = "test-support")]
fn queued_hover_is_replaced_by_a_newer_hover_for_the_same_document() {
    let root = tempfile::tempdir().expect("workspace");
    let main = root.path().join("Main.pas");
    let source = "unit Main;\ninterface\ntype\n  TTarget = class end;\n  TUse = TTarget;\n  TOther = TUse;\nimplementation\nend.\n";
    write_file(&main, source);
    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier(tempfile::tempdir().expect("environment"));
    server.initialize(root.path(), Value::Null);

    let blockers = [("TTarget", 1), ("TUse", 1), ("TUse", 0)]
        .into_iter()
        .enumerate()
        .map(|(index, (needle, occurrence))| {
            let id = RequestId::from(format!("hover-replacement-blocker-{index}"));
            server.send_request(
                id.clone(),
                "textDocument/definition",
                navigation_params(&main, source, needle, occurrence),
            );
            id
        })
        .collect::<Vec<_>>();
    barrier.wait_for_entries(blockers.len());

    let older = RequestId::from("older-queued-hover".to_string());
    server.send_request(
        older.clone(),
        "textDocument/hover",
        navigation_params(&main, source, "TTarget", 0),
    );
    let newer = RequestId::from("newer-queued-hover".to_string());
    server.send_request(
        newer.clone(),
        "textDocument/hover",
        navigation_params(&main, source, "TOther", 0),
    );
    let replaced = server.response(&older);
    let error = replaced.error.expect("the older queued hover is cancelled");
    assert_eq!(error.code, -32800);
    assert!(error.message.contains("superseded"), "{error:?}");

    barrier.release();
    let answered = server.response(&newer);
    assert!(answered.error.is_none(), "{answered:?}");
    for id in blockers {
        let _ = server.response(&id);
    }
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn definition_does_not_wait_out_a_bulk_references_cache_claim() {
    let root = tempfile::tempdir().expect("workspace");
    let provider = root.path().join("Provider.pas");
    let main = root.path().join("Main.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\ntype\n  TProvided = class end;\nimplementation\nend.\n",
    );
    let source = format!(
        "unit Main;\ninterface\nuses Provider;\ntype\n  TUse = TProvided;\nimplementation\nprocedure Run;\nvar Value: TProvided;\nbegin\n{}end;\nend.\n",
        "  Value := nil;\n".repeat(20)
    );
    write_file(&main, &source);
    // The import cache is used for project-backed sources.
    write_file(
        &root.path().join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
    );

    let environment = tempfile::tempdir().expect("server environment");
    let barrier_directory = environment.path().join("import-claim-barrier");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let claim = TestBarrier {
        entered: barrier_directory.join("entered"),
        release: barrier_directory.join("release"),
    };
    let claim_spec = format!("{}|{}", claim.entered.display(), claim.release.display());
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        environment.path(),
        [("PASCAL_LSP_TEST_IMPORT_CLAIM_BARRIER", claim_spec.as_str())],
    );
    server.initialize(root.path(), Value::Null);

    let references_id = RequestId::from("bulk-references-holding-a-claim".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(&source, "TProvided", 1),
            "context": {"includeDeclaration": true}
        }),
    );
    claim.wait_until_entered();

    let definition_id = RequestId::from("definition-behind-a-bulk-claim".to_string());
    let started = Instant::now();
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&main, &source, "TProvided", 0),
    );
    let definition = server
        .response_with_timeout(&definition_id, IO_TIMEOUT)
        .expect("definition must not wait for the bulk job's cache claim");
    eprintln!(
        "definition answered in {:?} while references held a cache claim",
        started.elapsed()
    );
    let locations = result_locations(definition);
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    // No document is open, so nothing warms: the one held claim is the
    // references job's, and the definition did not take one at the barrier.
    assert_eq!(
        fs::read(&claim.entered)
            .expect("claim barrier marker")
            .len(),
        1,
        "only the references job may hold a claim at the barrier"
    );

    claim.release();
    let references = server.response(&references_id);
    assert!(references.error.is_none(), "{references:?}");
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn result_older_than_the_reconciling_revision_waits_for_the_commit() {
    let root = tempfile::tempdir().expect("workspace");
    let main = root.path().join("Main.pas");
    let other = root.path().join("Other.pas");
    let source = "unit Main;\ninterface\ntype\n  TTarget = class end;\n  TUse = TTarget;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&other, "unit Other; interface implementation end.\n");

    let barrier_directory = root.path().join("reconciliation-hold-barriers");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let validation = TestBarrier {
        entered: barrier_directory.join("validation-entered"),
        release: barrier_directory.join("validation-release"),
    };
    let reconciliation = TestBarrier {
        entered: barrier_directory.join("reconciliation-entered"),
        release: barrier_directory.join("reconciliation-release"),
    };
    let validation_spec = format!(
        "{}|{}",
        validation.entered.display(),
        validation.release.display()
    );
    let reconciliation_spec = format!(
        "{}|{}",
        reconciliation.entered.display(),
        reconciliation.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_PARTIAL_VALIDATION_BARRIER",
                validation_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_FILE_WORKER_BARRIER",
                reconciliation_spec.as_str(),
            ),
        ],
    );
    server.initialize(root.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(&main), "languageId": "pascal", "version": 1, "text": source
        }}),
    );

    let id = RequestId::from("navigation-older-than-reconciliation".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TTarget", 1),
    );
    validation.wait_until_entered();
    let changed = source.replace("TUse", "TOther");
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": changed}]
        }),
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&other), "type": 2}]}),
    );
    reconciliation.wait_until_entered();
    validation.release();
    assert!(
        server
            .response_with_timeout(&id, Duration::from_millis(500))
            .is_none(),
        "a result computed before an applied edit must wait for the reconciled workspace"
    );

    reconciliation.release();
    let response = server.response(&id);
    assert!(
        response.error.is_some(),
        "the edited overlay must reject the older result: {response:?}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn blocked_file_discovery_does_not_block_cancel_or_unrelated_protocol_messages() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let provider_source =
        "unit Provider;\ninterface\ntype TOld = record end;\nimplementation\nend.\n";
    let consumer = root.path().join("Consumer.pas");
    let consumer_source =
        "unit Consumer;\ninterface\nuses Provider;\ntype TConsumer = TOld;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    let barrier_dir = root.path().join("file-discovery-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let analysis_barrier_dir = root.path().join("workspace-symbols-barrier");
    fs::create_dir_all(&analysis_barrier_dir).expect("workspace symbols barrier directory");
    let analysis_barrier = TestBarrier {
        entered: analysis_barrier_dir.join("entered"),
        release: analysis_barrier_dir.join("release"),
    };
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let analysis_barrier_value = format!(
        "{}|{}",
        analysis_barrier.entered.display(),
        analysis_barrier.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_SYMBOLS_BARRIER",
                analysis_barrier_value.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());

    let initial_id = RequestId::from("file-discovery-initial-load".to_string());
    server.send_request(
        initial_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOld", 0),
    );
    let initial_locations = result_locations(server.response(&initial_id));
    assert_eq!(initial_locations.len(), 1, "initial provider must resolve");
    write_file(&provider, &provider_source.replace("TOld", "TNew"));

    let files: Vec<_> = (0..64)
        .map(|index| {
            let path = if index == 0 {
                provider.clone()
            } else {
                root.path().join(format!("Dependent{index}.pas"))
            };
            json!({
                "uri": uri(&path),
                "type": 2
            })
        })
        .collect();
    let symbols_id = RequestId::from("cancel-symbol-query-during-file-discovery".to_string());
    server.send_request(
        symbols_id.clone(),
        "workspace/symbol",
        json!({"query":"TOld"}),
    );
    analysis_barrier.wait_until_entered();
    server.send_notification("workspace/didChangeWatchedFiles", json!({"changes":files}));
    barrier.wait_until_entered();

    server.send_notification("$/cancelRequest", json!({"id":symbols_id}));
    let responsive = server.response_with_timeout(&symbols_id, Duration::from_millis(250));
    let response = responsive.expect("cancellation must be processed while discovery is blocked");
    assert_eq!(response.error.expect("request is cancelled").code, -32800);

    let while_busy_id = RequestId::from("query-during-file-reconciliation".to_string());
    server.send_request(
        while_busy_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOld", 0),
    );
    assert!(
        server
            .response_with_timeout(&while_busy_id, Duration::from_millis(250))
            .is_none(),
        "query must wait behind the workspace mutation rather than use stale state"
    );

    barrier.release();
    analysis_barrier.release();

    let after_release = server.response(&while_busy_id);
    assert!(
        after_release.error.is_none(),
        "deferred query failed: {after_release:?}"
    );
    assert!(
        result_locations(after_release).is_empty(),
        "deferred query must observe the provider after reconciliation"
    );

    let fresh_deadline = Instant::now() + IO_TIMEOUT;
    loop {
        let fresh_id = RequestId::from("post-file-reconciliation-definition".to_string());
        server.send_request(
            fresh_id.clone(),
            "textDocument/definition",
            navigation_params(&consumer, consumer_source, "TOld", 0),
        );
        let fresh = server.response(&fresh_id);
        if fresh
            .error
            .as_ref()
            .is_some_and(|error| error.code == -32802)
        {
            assert!(
                Instant::now() < fresh_deadline,
                "workspace never became queryable"
            );
            continue;
        }
        assert!(fresh.error.is_none(), "fresh query failed: {fresh:?}");
        assert!(
            result_locations(fresh).is_empty(),
            "pre-event provider identity must not survive reconciliation"
        );
        break;
    }
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn shutdown_cancels_and_reaps_a_held_workspace_file_worker() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let source = "unit Provider;\ninterface\nimplementation\nend.\n";
    write_file(&provider, source);
    let barrier_dir = root.path().join("shutdown-file-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let completed = barrier_dir.join("completed");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let completed_value = completed.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
                completed_value.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    let initial = RequestId::from("shutdown-file-worker-load".to_string());
    server.send_request(
        initial.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    let _ = server.response(&initial);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();

    let shutdown_id = RequestId::from("shutdown-held-file-worker".to_string());
    server.send_request(shutdown_id.clone(), "shutdown", Value::Null);
    let shutdown_response = server.response_with_timeout(&shutdown_id, Duration::from_millis(250));
    let completed_before_release = wait_for_file(&completed, Duration::from_millis(250));

    // Always release and reap the child even in the expected RED run.
    barrier.release();
    if !completed_before_release {
        assert!(
            wait_for_file(&completed, IO_TIMEOUT),
            "worker did not exit after release"
        );
    }
    server.send_notification("exit", Value::Null);
    server.stdin.take();
    let status = server.child.wait().expect("wait for LSP server");
    assert!(status.success(), "server exited unsuccessfully: {status}");
    assert!(
        shutdown_response.is_some_and(|response| response.error.is_none()),
        "shutdown should respond while the worker is held"
    );
    assert!(
        completed_before_release,
        "shutdown returned while the detached worker still owned the workspace"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn exit_cancels_and_reaps_a_held_workspace_file_worker() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\nimplementation\nend.\n",
    );
    let barrier_dir = root.path().join("exit-file-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let completed = barrier_dir.join("completed");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let completed_value = completed.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
                completed_value.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    let initial = RequestId::from("exit-file-worker-load".to_string());
    server.send_request(
        initial.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    let _ = server.response(&initial);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();
    server.send_notification("exit", Value::Null);
    let reaped_before_release = wait_for_file(&completed, Duration::from_millis(250));
    if !reaped_before_release {
        barrier.release();
    }
    let _status = server.child.wait().expect("wait after exit");
    assert!(
        reaped_before_release,
        "exit returned while the detached worker still owned the workspace"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn input_disconnect_cancels_and_reaps_a_held_workspace_file_worker() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\nimplementation\nend.\n",
    );
    let barrier_dir = root.path().join("disconnect-file-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let completed = barrier_dir.join("completed");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let completed_value = completed.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
                completed_value.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    let initial = RequestId::from("disconnect-file-worker-load".to_string());
    server.send_request(
        initial.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    let _ = server.response(&initial);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();
    server.stdin.take();
    let worker_reaped = wait_for_file(&completed, Duration::from_millis(250));
    barrier.release();
    let _ = server.child.wait().expect("wait after input disconnect");
    assert!(
        worker_reaped,
        "disconnect left the workspace worker detached"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn panicked_workspace_worker_fails_queued_requests_before_closing() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\nimplementation\nend.\n",
    );
    let barrier_dir = root.path().join("panic-file-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            ("PASCAL_LSP_TEST_PANIC_FILE_WORKER", "1"),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    let initial = RequestId::from("panic-file-worker-load".to_string());
    server.send_request(
        initial.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    let _ = server.response(&initial);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();
    let pending = RequestId::from("request-after-panicked-file-worker".to_string());
    server.send_request(
        pending.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":uri(&provider)}}),
    );
    barrier.release();
    let response = server.response_with_timeout(&pending, IO_TIMEOUT);
    let _ = server.child.wait().expect("wait after worker panic");
    assert_eq!(
        response.and_then(|response| response.error.map(|error| error.code)),
        Some(-32803),
        "queued request must fail explicitly when reconciliation panics"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn saturated_workspace_fifo_replays_overlay_changes_config_response_and_query() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\ntype TStart = record end;\nimplementation\nend.\n",
    );
    let consumer = root.path().join("Consumer.pas");
    let consumer_source =
        "unit Consumer;\ninterface\nuses Overlay;\ntype TUse = TFinal;\nimplementation\nend.\n";
    write_file(&consumer, consumer_source);
    let barrier_dir = root.path().join("saturated-workspace-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variable(
        root.path(),
        Some("PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER"),
        Some(&barrier_value),
    );
    let root_uri = uri(root.path());
    let initialize_id = RequestId::from("saturated-fifo-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {"workspace": {"configuration": true}}
        }),
    );
    assert!(server.response(&initialize_id).error.is_none());
    server.send_notification("initialized", json!({}));
    let initial_configuration = server.request("workspace/configuration");
    server.send(Message::Response(Response::new_ok(
        initial_configuration.id,
        json!([{"projectFile": null}]),
    )));
    server.send_notification("workspace/didChangeConfiguration", json!({"settings":{}}));
    let pending_configuration = server.request("workspace/configuration");

    let initial_definition = RequestId::from("saturated-fifo-initial-definition".to_string());
    server.send_request(
        initial_definition.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    let _ = server.response(&initial_definition);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();

    let mut queued = Vec::new();
    queued.push(Message::Response(Response::new_ok(
        pending_configuration.id,
        json!([{"projectFile": null}]),
    )));
    let overlay = root.path().join("Overlay.pas");
    let overlay_source = |name: &str| {
        format!("unit Overlay;\ninterface\ntype {name} = record end;\nimplementation\nend.\n")
    };
    let mut version = 1;
    let mut latest_source = overlay_source("T00000");
    queued.push(Message::Notification(Notification::new(
        "textDocument/didOpen".to_string(),
        json!({"textDocument":{"uri":uri(&overlay),"languageId":"pascal","version":version,"text":latest_source}}),
    )));
    for index in 0..63 {
        version += 1;
        let next_name = if index == 62 {
            "TFinal".to_string()
        } else {
            format!("T{index:05}")
        };
        latest_source = overlay_source(&next_name);
        queued.push(Message::Notification(Notification::new(
            "textDocument/didChange".to_string(),
            json!({"textDocument":{"uri":uri(&overlay),"version":version},"contentChanges":[{"text":latest_source}]}),
        )));
    }
    assert!(queued.len() > 64);
    let mut wire = Vec::new();
    for message in &queued {
        message.write(&mut wire).expect("encode queued message");
    }
    server
        .stdin
        .as_mut()
        .expect("server stdin")
        .write_all(&wire)
        .expect("send saturated FIFO burst");
    let pending_query = RequestId::from("saturated-fifo-final-definition".to_string());
    server.send_request(
        pending_query.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TFinal", 0),
    );

    barrier.release();
    let query_response = server.response_with_timeout(&pending_query, IO_TIMEOUT);
    if query_response.is_none() {
        // Ensure a test failure cannot leave the child blocked or unreaped.
        let _ = server.child.kill();
    }
    let query_response =
        query_response.expect("saturated notifications must replay, not terminate the session");
    let locations = result_locations(query_response);
    assert_eq!(
        locations.len(),
        1,
        "query must observe the final queued overlay"
    );
    assert_eq!(locations[0]["uri"], uri(&overlay).to_string());
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn one_oversized_deferred_notification_does_not_abort_workspace_reconciliation() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\nimplementation\nend.\n",
    );
    let barrier_dir = root.path().join("oversized-deferred-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variable(
        root.path(),
        Some("PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER"),
        Some(&barrier_value),
    );
    server.initialize(root.path(), Value::Null);
    let initial = RequestId::from("oversized-deferred-load".to_string());
    server.send_request(
        initial.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    let _ = server.response(&initial);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();

    let oversized_notification = Message::Notification(Notification::new(
        "workspace/didChangeConfiguration".to_string(),
        json!({"settings":{"ignoredPadding":"x".repeat(1024 * 1024 + 1)}}),
    ));
    let query_id = RequestId::from("after-oversized-deferred-message".to_string());
    let query = Message::Request(Request::new(
        query_id.clone(),
        "textDocument/documentSymbol".to_string(),
        json!({"textDocument":{"uri":uri(&provider)}}),
    ));
    let mut wire = Vec::new();
    oversized_notification
        .write(&mut wire)
        .expect("encode oversized notification");
    query.write(&mut wire).expect("encode query");
    server
        .stdin
        .as_mut()
        .expect("server stdin")
        .write_all(&wire)
        .expect("send oversized deferred message");
    barrier.release();
    let response = server.response_with_timeout(&query_id, IO_TIMEOUT);
    if response.is_none() {
        let _ = server.child.kill();
    }
    assert!(
        response.is_some_and(|response| response.error.is_none()),
        "a single oversized deferred message must replay without losing the session"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn high_fanout_file_batch_reports_shared_actual_work_and_replays_fresh_requests() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let shared_include = root.path().join("Shared.inc");
    let old_provider = "unit Provider;\ninterface\ntype TOld = record end;\nimplementation\nend.\n";
    write_file(&provider, old_provider);
    write_file(&shared_include, "const SharedValue = 1;\n");

    let barrier_dir = root.path().join("reconciliation-work-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let metrics = barrier_dir.join("work.json");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
                metrics_value.as_str(),
            ),
        ],
    );
    server.initialize_with_watched_registration(root.path(), Value::Null, false);

    let mut consumers = Vec::new();
    for index in 0..32 {
        let consumer = root.path().join(format!("Consumer{index:02}.pas"));
        let source = format!(
            "unit Consumer{index:02};\ninterface\nuses Provider;\ntype TUse{index:02} = TOld;\nimplementation\n{{$I Shared.inc}}\nend.\n"
        );
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":source}}),
        );
        let _initial_diagnostics = diagnostics_for_uri(&mut server, &uri(&consumer));
        consumers.push(consumer);
    }

    let original_consumer = "unit Consumer00;\ninterface\nuses Provider;\ntype TUse00 = TOld;\nimplementation\n{$I Shared.inc}\nend.\n";
    let initial_definition_id = RequestId::from("high-fanout-initial-definition".to_string());
    server.send_request(
        initial_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], original_consumer, "TOld", 0),
    );
    let initial_definition = server.response(&initial_definition_id);
    let initial_locations = result_locations(initial_definition);
    assert_eq!(
        initial_locations.len(),
        1,
        "fixture must begin with a real indexed provider: {initial_locations:?}"
    );
    assert_eq!(initial_locations[0]["uri"], uri(&provider).to_string());

    write_file(
        &provider,
        "unit Provider;\ninterface\ntype TNew = record end;\nimplementation\nend.\n",
    );
    write_file(&shared_include, "const SharedValue = 2;\n");
    let mut changes = (0..31)
        .flat_map(|_| {
            [
                json!({"uri":uri(&provider),"type":2}),
                json!({"uri":uri(&shared_include),"type":2}),
            ]
        })
        .collect::<Vec<_>>();
    changes.push(json!({"uri":uri(&shared_include),"type":2}));
    // The physical provider still exists, but this final delete is the
    // causally latest event. Budget fallback must retain its tombstone.
    changes.push(json!({"uri":uri(&provider),"type":3}));
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );
    barrier.wait_until_entered();

    let updated_consumer = consumers[0].clone();
    let updated_source = "unit Consumer00;\ninterface\nuses Provider;\ntype TUse00 = TNew;\nimplementation\n{$I Shared.inc}\nend.\n";
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri(&updated_consumer),"version":2},"contentChanges":[{"text":updated_source}]}),
    );
    let definition_id = RequestId::from("high-fanout-fresh-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&updated_consumer, updated_source, "TNew", 0),
    );
    assert!(
        server
            .response_with_timeout(&definition_id, Duration::from_millis(150))
            .is_none(),
        "workspace request must remain behind the blocked file batch"
    );

    barrier.release();
    let definition = server.response_with_timeout(&definition_id, IO_TIMEOUT);
    let first_refreshed_diagnostics = diagnostics_for_uri(&mut server, &uri(&updated_consumer));
    let refreshed_diagnostics = if first_refreshed_diagnostics["version"].is_null()
        && first_refreshed_diagnostics["diagnostics"]
            .as_array()
            .is_some_and(Vec::is_empty)
    {
        diagnostics_for_uri(&mut server, &uri(&updated_consumer))
    } else {
        first_refreshed_diagnostics
    };
    let metrics_written = wait_for_file(&metrics, IO_TIMEOUT);
    if definition.is_none() || !metrics_written {
        let _ = server.child.kill();
    }
    let definition = definition.expect("queued definition must complete after reconciliation");
    let locations = result_locations(definition);
    assert!(
        locations.is_empty(),
        "fresh query must respect the final delete tombstone rather than stale provider contents: {locations:?}"
    );
    assert_eq!(
        refreshed_diagnostics["uri"],
        uri(&updated_consumer).to_string()
    );
    assert_eq!(refreshed_diagnostics["version"], 2);
    assert!(
        metrics_written,
        "worker must publish independent work counters"
    );
    let counters: Value = serde_json::from_slice(&fs::read(metrics).expect("read work counters"))
        .expect("parse work counters");
    assert!(
        counters["filesystem_path_visits"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "processed event entries must charge path visits: {counters}"
    );
    assert!(
        counters["diagnostic_record_checks"]
            .as_u64()
            .unwrap_or_default()
            >= 2_048,
        "64 duplicate provider/include events must charge dependent-record checks: {counters}"
    );
    assert!(
        counters["budget_exceeded"].as_bool().unwrap_or(false),
        "high-fan-out batch must take the conservative work-budget fallback: {counters}"
    );
    assert!(
        counters["unique_targets"].as_u64().unwrap_or(u64::MAX) <= 33,
        "duplicate-heavy work must not duplicate output targets: {counters}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn watched_refresh_charges_nested_include_reads_to_notification_budget() {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace directory");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let outer = root.join("Outer.inc");
    let inner = root.join("Inner.inc");
    let provider_source = "unit Provider;\ninterface\n{$I Outer.inc}\nimplementation\nend.\n";
    let outer_source = "{$I Inner.inc}\n";
    let inner_source = "type TProvided = Integer;\n";
    write_file(&provider, provider_source);
    write_file(&outer, outer_source);
    write_file(&inner, inner_source);
    let consumer_source =
        "unit Consumer;\ninterface\nuses Provider;\ntype TUse = TProvided;\nimplementation\nend.\n";
    write_file(&consumer, consumer_source);

    let barrier_dir = root.join("include-budget-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let metrics = barrier_dir.join("work.json");
    let worker_completed = environment.path().join("file-worker-completed");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let metrics_value = metrics.display().to_string();
    let worker_completed_value = worker_completed.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        environment.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
                metrics_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
                worker_completed_value.as_str(),
            ),
        ],
    );
    server.initialize_with_watched_registration(&root, Value::Null, false);

    let definition_id = RequestId::from("nested-include-initial-definition".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TProvided", 0),
    );
    let initial_locations = result_locations(server.response(&definition_id));
    assert_eq!(
        initial_locations.len(),
        1,
        "provider must load through includes"
    );
    assert_eq!(initial_locations[0]["uri"], uri(&inner).to_string());

    assert!(
        !worker_completed.exists(),
        "the unique target-worker marker must not predate the watched notification"
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );
    barrier.wait_until_entered();
    barrier.release();
    assert!(
        wait_for_file(&worker_completed, IO_TIMEOUT),
        "the target file worker must complete before its metrics are read"
    );
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "worker metrics must be written"
    );
    let metrics: Value =
        serde_json::from_slice(&fs::read(&metrics).expect("read metrics")).expect("parse metrics");
    let expected_read_bytes = provider_source.len() + outer_source.len() + inner_source.len();
    assert!(
        metrics["file_bytes_read"].as_u64().unwrap_or_default() as usize >= expected_read_bytes,
        "notification read accounting must include the root, outer include, and nested include bytes ({expected_read_bytes}): {metrics}"
    );
    assert!(
        metrics["filesystem_path_visits"]
            .as_u64()
            .unwrap_or_default()
            >= 8,
        "include resolution must charge both search-directory and candidate observations, not only selected reads: {metrics}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results() {
    const PROJECTS: usize = 64;
    const PADDING_BYTES: usize = 2 * 1024 * 1024;

    let root = tempfile::tempdir().expect("temporary workspace");
    let mut project_files = Vec::with_capacity(PROJECTS);
    let mut selected_providers = Vec::with_capacity(PROJECTS);
    let mut replacement_providers = Vec::with_capacity(PROJECTS);
    let mut consumers = Vec::with_capacity(PROJECTS);
    let mut consumer_sources = Vec::with_capacity(PROJECTS);
    let mut project_descriptors = Vec::with_capacity(PROJECTS);
    for index in 0..PROJECTS {
        let project_root = root.path().join(format!("Project{index:02}"));
        fs::create_dir_all(&project_root).expect("project root");
        let project = project_root.join("App.dproj");
        let selected_provider = project_root.join(format!("A/Provider{index:02}.pas"));
        let replacement_provider = project_root.join(format!("B/Provider{index:02}.pas"));
        let consumer = project_root.join(format!("Consumer{index:02}.pas"));
        let source = format!(
            "unit Consumer{index:02};\ninterface\nuses Provider{index:02};\ntype TUse{index:02} = TBefore;\nimplementation\nend.\n"
        );
        let descriptor_prefix = format!(
            "<Project><PropertyGroup><MainSource>Consumer{index:02}.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"A/Provider{index:02}.pas\" /></ItemGroup><!--"
        );
        let descriptor_suffix = "--></Project>";
        let padding = "x".repeat(
            PADDING_BYTES.saturating_sub(descriptor_prefix.len() + descriptor_suffix.len()),
        );
        let descriptor = format!("{descriptor_prefix}{padding}{descriptor_suffix}");
        write_file(&project, &descriptor);
        write_file(
            &selected_provider,
            &format!(
                "unit Provider{index:02};\ninterface\ntype TBefore = Integer;\nimplementation\nend.\n"
            ),
        );
        write_file(
            &replacement_provider,
            &format!(
                "unit Provider{index:02};\ninterface\ntype TBefore = string;\nimplementation\nend.\n"
            ),
        );
        write_file(&consumer, &source);
        project_files.push(project);
        selected_providers.push(selected_provider);
        replacement_providers.push(replacement_provider);
        consumers.push(consumer);
        consumer_sources.push(source);
        project_descriptors.push(descriptor);
    }

    let metrics = root.path().join("reconciliation-work.json");
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [(
            "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
            metrics_value.as_str(),
        )],
    );
    server.initialize_with_pull_diagnostics(root.path());
    for index in 0..PROJECTS {
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri(&consumers[index]),"languageId":"pascal","version":1,"text":consumer_sources[index]}}),
        );
    }

    let initial_id = RequestId::from("metadata-budget-initial-pull".to_string());
    server.send_request(
        initial_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[0])},"previousResultId":null}),
    );
    let initial = server.response(&initial_id);
    assert!(initial.error.is_none(), "initial pull: {initial:?}");
    let previous_result_id = initial.result.as_ref().unwrap()["resultId"]
        .as_str()
        .expect("initial result ID")
        .to_string();

    let closed_initial_id = RequestId::from("metadata-budget-closed-initial-pull".to_string());
    server.send_request(
        closed_initial_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[1])},"previousResultId":null}),
    );
    let closed_initial = server.response(&closed_initial_id);
    assert!(
        closed_initial.error.is_none(),
        "closed-owner initial pull: {closed_initial:?}"
    );
    let closed_previous_result_id = closed_initial.result.as_ref().unwrap()["resultId"]
        .as_str()
        .expect("closed-owner initial result ID")
        .to_string();
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument":{"uri":uri(&consumers[1])}}),
    );

    let initial_definition_id = RequestId::from("metadata-budget-initial-provider".to_string());
    server.send_request(
        initial_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &consumer_sources[0], "TBefore", 0),
    );
    let initial_locations = result_locations(server.response(&initial_definition_id));
    assert_eq!(
        initial_locations
            .first()
            .and_then(|location| location["uri"].as_str()),
        Some(uri(&selected_providers[0]).as_str()),
        "initial descriptor must select provider A"
    );

    for index in 0..PROJECTS {
        let stamp = fs::metadata(&project_files[index]).expect("descriptor stamp before edit");
        let replacement = project_descriptors[index].replace("A/Provider", "B/Provider");
        assert_eq!(replacement.len(), project_descriptors[index].len());
        write_file(&project_files[index], &replacement);
        restore_mtime(&project_files[index], &stamp);
    }

    let changes = project_files
        .iter()
        .map(|project| json!({"uri":uri(project),"type":2}))
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 64);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );

    let fresh_definition_id = RequestId::from("metadata-budget-fresh-provider".to_string());
    server.send_request(
        fresh_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &consumer_sources[0], "TBefore", 0),
    );
    let fresh_locations = result_locations(server.response(&fresh_definition_id));
    assert_eq!(
        fresh_locations
            .first()
            .and_then(|location| location["uri"].as_str()),
        Some(uri(&replacement_providers[0]).as_str()),
        "a refused recovery must discard the over-cap project observations and answer from the edited descriptor"
    );
    let closed_definition_id = RequestId::from("metadata-budget-closed-provider".to_string());
    server.send_request(
        closed_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[1], &consumer_sources[1], "TBefore", 0),
    );
    let closed_locations = result_locations(server.response(&closed_definition_id));
    assert_eq!(
        closed_locations
            .first()
            .and_then(|location| location["uri"].as_str()),
        Some(uri(&replacement_providers[1]).as_str()),
        "cached closed owners must be rebuilt from the edited descriptor too"
    );

    let refreshed_id = RequestId::from("metadata-budget-refreshed-pull".to_string());
    server.send_request(
        refreshed_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[0])},"previousResultId":previous_result_id}),
    );
    let refreshed = server.response(&refreshed_id);
    assert!(refreshed.error.is_none(), "refreshed pull: {refreshed:?}");
    assert_ne!(
        refreshed.result.as_ref().unwrap()["resultId"].as_str(),
        Some(previous_result_id.as_str()),
        "pull diagnostics must not reuse the result computed before the discarded state: {refreshed:?}"
    );
    let closed_refreshed_id = RequestId::from("metadata-budget-closed-refreshed-pull".to_string());
    server.send_request(
        closed_refreshed_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[1])},"previousResultId":closed_previous_result_id}),
    );
    let closed_refreshed = server.response(&closed_refreshed_id);
    assert!(
        closed_refreshed.error.is_none(),
        "closed-owner refreshed pull: {closed_refreshed:?}"
    );
    assert_ne!(
        closed_refreshed.result.as_ref().unwrap()["resultId"].as_str(),
        Some(closed_previous_result_id.as_str()),
        "closed-owner pull diagnostics must not reuse the stale result: {closed_refreshed:?}"
    );
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "worker metrics must be written"
    );
    let metrics: Value =
        serde_json::from_slice(&fs::read(&metrics).expect("read metrics")).expect("parse metrics");
    assert!(
        metrics["budget_exceeded"].as_bool().unwrap_or(false),
        "64 large project metadata rediscoveries must consume one shared notification byte budget: {metrics}"
    );
    assert!(
        metrics["file_bytes_read"].as_u64().unwrap_or_default() >= 12 * 1024 * 1024,
        "project descriptor payload reads must be part of the shared worker accounting: {metrics}"
    );
    assert!(
        metrics["file_bytes_read"].as_u64().unwrap_or_default() <= 16 * 1024 * 1024,
        "metadata reads must stop at the shared notification byte ceiling: {metrics}"
    );
    assert!(
        metrics["diagnostic_record_checks"]
            .as_u64()
            .unwrap_or_default()
            <= 2_048,
        "metadata changes must remain within the separate diagnostic-work ceiling: {metrics}"
    );
    assert!(
        metrics["recovery_preflight_bytes"]
            .as_u64()
            .unwrap_or_default()
            <= (128 * 1024 * 1024) as u64,
        "preflight accounting must never exceed its byte envelope even when refusing: {metrics}"
    );
    assert_eq!(metrics["recovery_refused"], true);
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn diagnostic_candidate_observation_budget_fallback_drops_stale_provider_and_pull_result() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let consumer = root.path().join("Consumer.pas");
    let consumer_source =
        "unit Consumer;\ninterface\nuses Provider;\ntype TUse = TBefore;\nimplementation\nend.\n";
    write_file(
        &provider,
        "unit Provider;\ninterface\ntype TBefore = Integer;\nimplementation\nend.\n",
    );
    write_file(&consumer, consumer_source);

    let metrics = root.path().join("diagnostic-candidate-reconciliation.json");
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            ("PASCAL_LSP_TEST_DIAGNOSTIC_CANDIDATE_OBSERVATIONS", "3072"),
            (
                "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
                metrics_value.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":consumer_source}}),
    );
    let initial_pull_id = RequestId::from("candidate-observation-initial-pull".to_string());
    server.send_request(
        initial_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":null}),
    );
    let initial_pull = server.response(&initial_pull_id);
    assert!(
        initial_pull.error.is_none(),
        "initial pull: {initial_pull:?}"
    );
    let previous_result_id = initial_pull.result.as_ref().unwrap()["resultId"]
        .as_str()
        .expect("initial result ID")
        .to_owned();

    let initial_definition_id =
        RequestId::from("candidate-observation-initial-definition".to_string());
    server.send_request(
        initial_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TBefore", 0),
    );
    let initial_locations = result_locations(server.response(&initial_definition_id));
    assert_eq!(
        initial_locations
            .first()
            .and_then(|location| location["uri"].as_str()),
        Some(uri(&provider).as_str()),
        "fixture starts with a live provider definition"
    );

    write_file(
        &provider,
        "unit Provider;\ninterface\ntype TAfter = string;\nimplementation\nend.\n",
    );
    let changes = (0..64)
        .map(|index| {
            json!({
                "uri": uri(&root.path().join(format!("Unrelated{index:02}.pas"))),
                "type": 3
            })
        })
        .collect::<Vec<_>>();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );

    let current_definition_id =
        RequestId::from("candidate-observation-current-definition".to_string());
    server.send_request(
        current_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TBefore", 0),
    );
    let current_locations = result_locations(server.response(&current_definition_id));
    assert!(
        current_locations.is_empty(),
        "budget fallback must not leave the old provider as a current definition: {current_locations:?}"
    );

    let refreshed_pull_id = RequestId::from("candidate-observation-refreshed-pull".to_string());
    server.send_request(
        refreshed_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":previous_result_id}),
    );
    let refreshed_pull = server.response(&refreshed_pull_id);
    assert!(
        refreshed_pull.error.is_none(),
        "refreshed pull: {refreshed_pull:?}"
    );
    assert_ne!(
        refreshed_pull.result.as_ref().unwrap()["kind"],
        "unchanged",
        "fallback must invalidate the previous pull result even when refusal occurs inside candidate matching"
    );
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "worker metrics must be written"
    );
    let metrics: Value =
        serde_json::from_slice(&fs::read(metrics).expect("read metrics")).expect("parse metrics");
    assert!(
        metrics["budget_exceeded"].as_bool().unwrap_or(false),
        "candidate comparison exhaustion must take the conservative fallback: {metrics}"
    );
    assert!(
        metrics["project_path_key_bytes"]
            .as_u64()
            .unwrap_or_default()
            > 12 * 1024 * 1024,
        "path-byte accounting must show candidate comparisons as the exhausted work: {metrics}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn recovery_push_clears_remain_bounded_under_writer_backpressure() {
    const ROOTS: usize = 64;
    let environment = tempfile::tempdir().expect("workspace and server environment");
    let root = environment.path().to_path_buf();
    let mut expected_uris = HashSet::new();
    let shared_target = root.join("Shared.inc");
    write_file(&shared_target, "{ shared test-only publication target }\n");
    let shared_target_uri = uri(&shared_target);
    expected_uris.insert(shared_target_uri.clone());
    let mut sources = Vec::with_capacity(ROOTS);
    let mut files = Vec::with_capacity(ROOTS);
    for index in 0..ROOTS {
        let source = format!(
            "unit Root{index:02};\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n"
        );
        let file = root.join(format!("Root{index:02}.pas"));
        write_file(&file, &source);
        expected_uris.insert(uri(&file));
        sources.push(source);
        files.push(file);
    }

    let barrier_directory = root.join("outbound-recovery-barrier");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let writer_barrier = OutboundWriterBarrier {
        armed: barrier_directory.join("armed"),
        entered: barrier_directory.join("entered"),
        release: barrier_directory.join("release"),
        control_limit: 8,
    };
    let barrier_value = format!(
        "{}|{}|{}|{}",
        writer_barrier.armed.display(),
        writer_barrier.entered.display(),
        writer_barrier.release.display(),
        writer_barrier.control_limit
    );
    let metrics = root.join("recovery-work.json");
    let metrics_value = metrics.display().to_string();
    let staling_metrics = root.join("publication-staling.json");
    let staling_metrics_value = staling_metrics.display().to_string();
    let shared_target_value = shared_target_uri.as_str().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        &root,
        [
            (
                "PASCAL_LSP_TEST_OUTBOUND_WRITER_BARRIER",
                barrier_value.as_str(),
            ),
            ("PASCAL_LSP_TEST_DIAGNOSTIC_CANDIDATE_OBSERVATIONS", "8"),
            (
                "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
                metrics_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_PUBLICATION_STALING_RESULT",
                staling_metrics_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_SHARED_PUBLICATION_TARGET_URI",
                shared_target_value.as_str(),
            ),
            ("PASCAL_LSP_TEST_ALLOW_PULL_DIAGNOSTICS_REQUESTS", "1"),
        ],
    );
    server._environment = Some(environment);
    server.initialize(&root, Value::Null);
    for (index, file) in files.iter().enumerate() {
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri(file),"languageId":"pascal","version":1,"text":sources[index]}}),
        );
    }
    let mut initial_publications = HashSet::new();
    let initial_deadline = Instant::now() + Duration::from_secs(60);
    while initial_publications.len() < expected_uris.len() {
        assert!(
            Instant::now() < initial_deadline,
            "only got {} initial diagnostic publications of {ROOTS}",
            initial_publications.len()
        );
        let message = match server.messages.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => panic!("server exited before initial diagnostics"),
            Ok(Err(error)) => panic!("server output read failed: {error}"),
            Err(error) => panic!(
                "initial diagnostics stalled after {} roots; server status: {:?}; {error}",
                initial_publications.len(),
                server.child.try_wait().expect("inspect LSP server")
            ),
        };
        match message {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                let published_uri = Url::parse(
                    notification.params["uri"]
                        .as_str()
                        .expect("initial diagnostic URI"),
                )
                .expect("valid initial diagnostic URI");
                if expected_uris.contains(&published_uri) {
                    initial_publications.insert(published_uri);
                }
            }
            _ => {}
        }
    }
    assert_eq!(initial_publications, expected_uris);

    let initial_pull_id = RequestId::from("shared-target-pull-initial".to_string());
    server.send_request(
        initial_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&files[0])}}),
    );
    let initial_pull = server.response(&initial_pull_id);
    assert!(
        initial_pull.error.is_none(),
        "initial pull failed: {initial_pull:?}"
    );
    let previous_result_id = initial_pull.result.as_ref().unwrap()["resultId"]
        .as_str()
        .expect("initial diagnostic result ID")
        .to_string();

    fs::write(&writer_barrier.armed, b"pause").expect("arm writer barrier");
    let changes = (0..64)
        .map(|index| {
            json!({
                "uri": uri(&root.join(format!("Unrelated{index:02}.pas"))),
                "type": 2
            })
        })
        .collect::<Vec<_>>();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );

    let request_id = RequestId::from("request-during-recovery-push-clear".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":uri(&files[0])}}),
    );
    writer_barrier.wait_until_entered();
    thread::sleep(Duration::from_millis(300));
    let process_status = server.child.try_wait().expect("inspect server status");
    writer_barrier.release();
    assert!(
        process_status.is_none(),
        "server must remain alive while recovery clears are backpressured: {process_status:?}"
    );

    let mut clear_counts = HashMap::<Url, usize>::new();
    let mut request_response = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while clear_counts.len() < expected_uris.len() || request_response.is_none() {
        assert!(
            Instant::now() < deadline,
            "bounded recovery output did not drain: cleared={}, response={:?}",
            clear_counts.len(),
            request_response
        );
        let next_message = match server.messages.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => panic!("server exited during recovery push cleanup"),
            Ok(Err(error)) => panic!("server output read failed: {error}"),
            Err(error) => panic!(
                "recovery output stalled: clears={}, response={:?}, metrics={:?}, server={:?}, {error}",
                clear_counts.len(),
                request_response,
                fs::read(&metrics)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()),
                server.child.try_wait().expect("inspect LSP server")
            ),
        };
        match next_message {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                let published_uri = Url::parse(
                    notification.params["uri"]
                        .as_str()
                        .expect("recovery diagnostic URI"),
                )
                .expect("valid recovery diagnostic URI");
                if expected_uris.contains(&published_uri)
                    && notification.params["diagnostics"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
                {
                    *clear_counts.entry(published_uri).or_default() += 1;
                }
            }
            Message::Response(response) if response.id == request_id => {
                request_response = Some(response);
            }
            _ => {}
        }
    }
    assert!(clear_counts.values().all(|count| *count == 1));
    let response = request_response.expect("unrelated request response");
    assert!(
        response.error.is_none(),
        "an unrelated request must still be serviced after recovery: {response:?}"
    );

    assert!(wait_for_file(&staling_metrics, IO_TIMEOUT));
    let staling_metrics: Value = serde_json::from_slice(
        &fs::read(&staling_metrics).expect("read publication staling metrics"),
    )
    .expect("parse publication staling metrics");
    assert_eq!(staling_metrics["queue_scans"], 1);
    assert!(
        staling_metrics["queue_messages_scanned"]
            .as_u64()
            .unwrap_or_default()
            <= 2 * 32_768,
        "one stale completion must scan only the admitted pending/deferred queues: {staling_metrics}"
    );
    assert!(
        staling_metrics["queue_bytes_scanned"]
            .as_u64()
            .unwrap_or_default()
            <= 16 * 1024 * 1024 + 64 * 1024 * 1024 + 8 * 1024 * 1024,
        "queue scan bytes must stay within pending plus deferred admission: {staling_metrics}"
    );

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&files[0]),
                "languageId": "pascal",
                "version": 2,
                "text": "unit Root00;\ninterface\nimplementation\nend.\n",
            }
        }),
    );
    loop {
        match server.receive_until(Instant::now() + IO_TIMEOUT) {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics"
                    && notification.params["uri"] == uri(&files[0]).as_str() =>
            {
                assert_eq!(clear_counts.len(), expected_uris.len());
                assert!(
                    notification.params["diagnostics"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                break;
            }
            _ => {}
        }
    }

    let refreshed_pull_id = RequestId::from("shared-target-pull-refreshed".to_string());
    server.send_request(
        refreshed_pull_id.clone(),
        "textDocument/diagnostic",
        json!({
            "textDocument":{"uri":uri(&files[0])},
            "previousResultId":previous_result_id,
        }),
    );
    let refreshed_pull = server.response(&refreshed_pull_id);
    assert!(
        refreshed_pull.error.is_none(),
        "refreshed pull failed: {refreshed_pull:?}"
    );
    let refreshed_report = refreshed_pull.result.expect("refreshed pull result");
    assert_eq!(refreshed_report["kind"], "full");
    assert_ne!(refreshed_report["resultId"], previous_result_id);
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "worker metrics must be written"
    );
    let metrics: Value = serde_json::from_slice(&fs::read(metrics).expect("read worker metrics"))
        .expect("parse worker metrics");
    assert!(metrics["budget_exceeded"].as_bool().unwrap_or(false));
    assert_eq!(metrics["recovery_target_reserve"], ROOTS);
    assert!(
        metrics["recovery_visits"].as_u64().unwrap_or_default() > ROOTS as u64,
        "retained source/open-owner entries must be invalidated under the recovery account: {metrics}"
    );
    assert!(
        metrics["recovery_visits"].as_u64().unwrap_or_default()
            <= metrics["recovery_visit_reserve"]
                .as_u64()
                .unwrap_or_default(),
        "recovery must stay within its preflight envelope: {metrics}"
    );
    assert_eq!(metrics["recovery_refused"], false);

    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn watched_override_refresh_rebinds_same_stamp_provider_and_invalidates_pull_result() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let sdk = root.path().join("SDK");
    let provider_a = sdk.join("A/Provider.pas");
    let provider_b = sdk.join("B/Provider.pas");
    let consumer = root.path().join("Consumer.pas");
    let project = root.path().join("App.dproj");
    let override_file = root.path().join(".delphi-tools.local.toml");
    let source =
        "unit Consumer; interface uses Provider; type TUse = TBefore; implementation end.\n";
    let provider_source = "unit Provider; interface type TBefore = Integer; implementation end.\n";
    write_file(&provider_a, provider_source);
    write_file(&provider_b, provider_source);
    write_file(&consumer, source);
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    let config_for = |target: &Path| {
        format!(
            "[[path_mappings]]\nfrom = 'C:\\\\SDK'\nto = '{}'\n",
            target.display()
        )
    };
    let initial_config = config_for(&sdk.join("A"));
    let replacement_config = config_for(&sdk.join("B"));
    assert_eq!(initial_config.len(), replacement_config.len());
    write_file(&override_file, &initial_config);
    let original_stamp = fs::metadata(&override_file).expect("override stamp");

    let metrics = root.path().join("reconciliation-work.json");
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [(
            "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
            metrics_value.as_str(),
        )],
    );
    server.initialize_with_pull_diagnostics(root.path());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":source}}),
    );
    let initial_pull_id = RequestId::from("override-initial-pull".to_string());
    server.send_request(
        initial_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":null}),
    );
    let initial_pull = server.response(&initial_pull_id);
    let previous_result_id = initial_pull.result.as_ref().unwrap()["resultId"]
        .as_str()
        .expect("initial pull result ID")
        .to_string();
    let initial_definition_id = RequestId::from("override-initial-provider".to_string());
    server.send_request(
        initial_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, source, "TBefore", 0),
    );
    assert_eq!(
        result_locations(server.response(&initial_definition_id))[0]["uri"],
        uri(&provider_a).to_string()
    );

    write_file(&override_file, &replacement_config);
    restore_mtime(&override_file, &original_stamp);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&override_file),"type":2}]}),
    );
    let fresh_definition_id = RequestId::from("override-fresh-provider".to_string());
    server.send_request(
        fresh_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, source, "TBefore", 0),
    );
    let fresh_definition = server.response(&fresh_definition_id);
    assert_eq!(
        result_locations(fresh_definition)[0]["uri"],
        uri(&provider_b).to_string(),
        "the watched override event must use the newly captured mapping"
    );
    let refreshed_pull_id = RequestId::from("override-refreshed-pull".to_string());
    server.send_request(
        refreshed_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":previous_result_id}),
    );
    let refreshed_pull = server.response(&refreshed_pull_id);
    assert_ne!(
        refreshed_pull.result.as_ref().unwrap()["kind"],
        "unchanged",
        "changed override configuration cannot retain the old pull result"
    );
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "worker metrics are produced"
    );
    let metrics: Value =
        serde_json::from_slice(&fs::read(&metrics).expect("read metrics")).expect("parse metrics");
    assert!(
        metrics["file_bytes_read"].as_u64().unwrap_or_default() >= replacement_config.len() as u64,
        "override bytes must be charged to the notification account: {metrics}"
    );
    assert!(
        metrics["filesystem_path_visits"]
            .as_u64()
            .unwrap_or_default()
            >= 2,
        "override candidate/open probes must be charged: {metrics}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn late_override_budget_refusal_retries_after_sixty_four_event_fallback() {
    const PROJECT_COUNT: usize = 7;
    const DESCRIPTOR_BYTES: usize = 2 * 1024 * 1024;
    const BATCH_ENTRIES: usize = 64;
    const CONFIG_BYTES: usize = 4 * 1024 * 1024;

    let root = tempfile::tempdir().expect("temporary workspace");
    let sdk = root.path().join("SDK");
    let provider_a = sdk.join("A/Provider.pas");
    let provider_b = sdk.join("B/Provider.pas");
    write_file(
        &provider_a,
        "unit Provider; interface type TSelected = Integer; implementation end.\n",
    );
    write_file(
        &provider_b,
        "unit Provider; interface type TSelected = string; implementation end.\n",
    );

    let override_file = root.path().join(".delphi-tools.local.toml");
    let config_for = |target: &Path| {
        let prefix = format!(
            "[[path_mappings]]\nfrom = 'C:\\\\SDK'\nto = '{}'\n#",
            target.display()
        );
        assert!(prefix.len() < CONFIG_BYTES);
        format!("{prefix}{}", "x".repeat(CONFIG_BYTES - prefix.len()))
    };
    let initial_config = config_for(&sdk.join("A"));
    let replacement_config = config_for(&sdk.join("B"));
    assert_eq!(initial_config.len(), CONFIG_BYTES);
    assert_eq!(replacement_config.len(), CONFIG_BYTES);
    assert_eq!(initial_config.len(), replacement_config.len());
    write_file(&override_file, &initial_config);

    let mut project_files = Vec::with_capacity(PROJECT_COUNT);
    let mut consumers = Vec::with_capacity(PROJECT_COUNT);
    let mut sources = Vec::with_capacity(PROJECT_COUNT);
    for index in 0..PROJECT_COUNT {
        let project_root = root.path().join(format!("Project{index:02}"));
        let project = project_root.join("App.dproj");
        let consumer = project_root.join(format!("Consumer{index:02}.pas"));
        let source = format!(
            "unit Consumer{index:02};\ninterface\nuses Provider;\ntype TUse{index:02} = TSelected;\nimplementation\nend.\n"
        );
        let descriptor_prefix = format!(
            "<Project><PropertyGroup><MainSource>Consumer{index:02}.pas</MainSource><DCC_UnitSearchPath>C:\\\\SDK</DCC_UnitSearchPath></PropertyGroup><!--"
        );
        let descriptor_suffix = "--></Project>";
        let descriptor = format!(
            "{descriptor_prefix}{}{descriptor_suffix}",
            "x".repeat(
                DESCRIPTOR_BYTES.saturating_sub(descriptor_prefix.len() + descriptor_suffix.len())
            )
        );
        write_file(&project, &descriptor);
        write_file(&consumer, &source);
        project_files.push(project);
        consumers.push(consumer);
        sources.push(source);
    }

    let metrics = root.path().join("reconciliation-work.json");
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [(
            "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
            metrics_value.as_str(),
        )],
    );
    server.initialize_with_pull_diagnostics(root.path());
    for index in 0..PROJECT_COUNT {
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri(&consumers[index]),"languageId":"pascal","version":1,"text":sources[index]}}),
        );
        let definition_id = RequestId::from(format!("late-override-initial-{index}"));
        server.send_request(
            definition_id.clone(),
            "textDocument/definition",
            navigation_params(&consumers[index], &sources[index], "TSelected", 0),
        );
        assert_eq!(
            result_locations(server.response(&definition_id))[0]["uri"],
            uri(&provider_a).to_string(),
            "each initial context must select provider A"
        );
    }
    let pull_id = RequestId::from("late-override-initial-pull".to_string());
    server.send_request(
        pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[0])},"previousResultId":null}),
    );
    let initial_pull = server.response(&pull_id);
    let previous_result_id = initial_pull.result.as_ref().unwrap()["resultId"]
        .as_str()
        .expect("initial result ID")
        .to_owned();

    for project in &project_files {
        let stamp = fs::metadata(project).expect("project descriptor stamp");
        let original = fs::read(project).expect("project descriptor bytes");
        write_file(
            project,
            std::str::from_utf8(&original).expect("UTF-8 descriptor"),
        );
        restore_mtime(project, &stamp);
    }
    let override_stamp = fs::metadata(&override_file).expect("override stamp before edit");
    write_file(&override_file, &replacement_config);
    restore_mtime(&override_file, &override_stamp);

    let mut changes = project_files
        .iter()
        .map(|project| json!({"uri":uri(project),"type":2}))
        .collect::<Vec<_>>();
    while changes.len() < BATCH_ENTRIES - 1 {
        let path = root
            .path()
            .join(format!("ignored-event-{}.pas", changes.len()));
        changes.push(json!({"uri":uri(&path),"type":2}));
    }
    changes.push(json!({"uri":uri(&override_file),"type":2}));
    assert_eq!(changes.len(), BATCH_ENTRIES);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );

    let fresh_definition_id = RequestId::from("late-override-fresh-provider".to_string());
    server.send_request(
        fresh_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &sources[0], "TSelected", 0),
    );
    let fresh_locations = result_locations(server.response(&fresh_definition_id));
    assert_eq!(
        fresh_locations
            .first()
            .and_then(|location| location["uri"].as_str()),
        Some(uri(&provider_b).as_str()),
        "after budget fallback the dirty override must be retried, not leave provider A captured"
    );
    let refreshed_pull_id = RequestId::from("late-override-refreshed-pull".to_string());
    server.send_request(
        refreshed_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[0])},"previousResultId":previous_result_id}),
    );
    let refreshed_pull = server.response(&refreshed_pull_id);
    assert!(
        refreshed_pull.error.is_none(),
        "pull retry: {refreshed_pull:?}"
    );
    assert_ne!(
        refreshed_pull.result.as_ref().unwrap()["kind"],
        "unchanged",
        "fallback must not preserve the pre-event pull result ID"
    );
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "worker metrics are produced"
    );
    let metrics: Value =
        serde_json::from_slice(&fs::read(&metrics).expect("read metrics")).expect("parse metrics");
    assert!(
        metrics["budget_exceeded"].as_bool().unwrap_or(false),
        "late config read should exceed the shared budget and trigger fallback: {metrics}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn package_descriptor_batch_fallback_rebinds_provider_and_pull_result() {
    const OWNER_COUNT: usize = 9;
    const PACKAGE_BYTES: usize = 2 * 1024 * 1024;
    const BATCH_ENTRIES: usize = 64;
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider_a = root.path().join("units/ProviderA.pas");
    let provider_b = root.path().join("units/ProviderB.pas");
    let provider_source = |value: &str| {
        format!("unit Provider; interface type TSelected = {value}; implementation end.\n")
    };
    write_file(&provider_a, &provider_source("Integer"));
    write_file(&provider_b, &provider_source("string "));
    assert_eq!(
        fs::metadata(&provider_a).unwrap().len(),
        fs::metadata(&provider_b).unwrap().len()
    );

    let mut projects = Vec::new();
    let mut packages = Vec::new();
    let mut consumers = Vec::new();
    let mut sources = Vec::new();
    for index in 0..OWNER_COUNT {
        let directory = root.path().join(format!("Project{index:02}"));
        let project = directory.join("App.dproj");
        let consumer = directory.join(format!("Consumer{index:02}.pas"));
        let source = format!(
            "unit Consumer{index:02};\ninterface\nuses Provider;\ntype TUse{index:02} = TSelected;\nimplementation\nend.\n"
        );
        for flavor in ["A", "B"] {
            let package = root.path().join(format!("Package{flavor}{index:02}.dpk"));
            let mapped_provider = if flavor == "A" {
                "ProviderA"
            } else {
                "ProviderB"
            };
            let mut contents = format!(
                "package Package{flavor}{index:02};\ncontains\n  Provider in 'units/{mapped_provider}.pas';\n"
            );
            for candidate in 0..256 {
                contents.push_str(&format!(
                    "  Catalog{candidate:03} in 'catalogue/Catalog{candidate:03}.pas';\n"
                ));
            }
            contents.push_str("end.\n");
            write_file(&package, &contents);
            packages.push(package);
        }
        let selected_package = if index == 0 { "A" } else { "B" };
        let prefix = format!(
            "<Project><PropertyGroup><MainSource>Consumer{index:02}.pas</MainSource><DCC_UsePackage>Package{selected_package}{index:02}</DCC_UsePackage></PropertyGroup><!--"
        );
        let suffix = "--></Project>";
        let descriptor = format!(
            "{prefix}{}{suffix}",
            "p".repeat(PACKAGE_BYTES - prefix.len() - suffix.len())
        );
        assert_eq!(descriptor.len(), PACKAGE_BYTES);
        write_file(&project, &descriptor);
        write_file(&consumer, &source);
        projects.push(project);
        consumers.push(consumer);
        sources.push(source);
    }
    let metrics = root.path().join("package-work.json");
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [(
            "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
            metrics_value.as_str(),
        )],
    );
    server.initialize_with_pull_diagnostics(root.path());
    for index in 0..OWNER_COUNT {
        let id = RequestId::from(format!("pkg-start-{index}"));
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&consumers[index], &sources[index], "TSelected", 0),
        );
        let expected = if index == 0 { &provider_a } else { &provider_b };
        assert_eq!(
            result_locations(server.response(&id))[0]["uri"],
            uri(expected).to_string()
        );
    }
    for index in 0..OWNER_COUNT {
        server.send_notification("textDocument/didOpen", json!({"textDocument":{"uri":uri(&consumers[index]),"languageId":"pascal","version":1,"text":sources[index]}}));
    }
    let pull_id = RequestId::from("pkg-prior-pull".to_string());
    server.send_request(
        pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[0])},"previousResultId":null}),
    );
    let prior = server.response(&pull_id).result.unwrap()["resultId"]
        .as_str()
        .unwrap()
        .to_owned();

    let stamp = fs::metadata(&packages[0]).expect("package stamp");
    let old = fs::read_to_string(&packages[0]).expect("package bytes");
    let changed = old.replace("ProviderA", "ProviderB");
    assert_eq!(old.len(), changed.len());
    write_file(&packages[0], &changed);
    restore_mtime(&packages[0], &stamp);
    let mut changes = packages
        .iter()
        .map(|path| json!({"uri":uri(path),"type":2}))
        .chain(
            projects
                .iter()
                .map(|path| json!({"uri":uri(path),"type":2})),
        )
        .collect::<Vec<_>>();
    while changes.len() < BATCH_ENTRIES {
        let path = root.path().join(format!("other-{}.pas", changes.len()));
        changes.push(json!({"uri":uri(&path),"type":2}));
    }
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );

    let fresh_id = RequestId::from("pkg-fresh-definition".to_string());
    server.send_request(
        fresh_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &sources[0], "TSelected", 0),
    );
    assert_eq!(
        result_locations(server.response(&fresh_id))[0]["uri"],
        uri(&provider_b).to_string(),
        "no stale provider A binding may survive the batch"
    );
    let pull2 = RequestId::from("pkg-new-pull".to_string());
    server.send_request(
        pull2.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumers[0])},"previousResultId":prior}),
    );
    assert_ne!(server.response(&pull2).result.unwrap()["kind"], "unchanged");
    assert!(wait_for_file(&metrics, IO_TIMEOUT));
    let metrics: Value = serde_json::from_slice(&fs::read(&metrics).unwrap()).unwrap();
    assert!(
        metrics["budget_exceeded"].as_bool().unwrap_or(false),
        "package batch must exercise fallback: {metrics}"
    );
    assert!(
        metrics["package_path_visits"].as_u64().unwrap_or_default() > 0,
        "notification shared account must meter package descriptor event paths: {metrics}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn package_descriptor_batch_fallback_clears_then_pushes_fresh_provider_diagnostics() {
    const OWNER_COUNT: usize = 4;
    const PACKAGE_BYTES: usize = 2 * 1024 * 1024;
    const BATCH_ENTRIES: usize = 64;
    let environment = tempfile::tempdir().expect("temporary workspace");
    let root = environment.path();
    let provider_a = root.join("units/ProviderA.pas");
    let provider_b = root.join("units/ProviderB.pas");
    let provider_source = |member: &str| {
        format!(
            "unit Provider;\ninterface\ntype TSelected = record\n  {member}: Integer;\nend;\nimplementation\nend.\n"
        )
    };
    write_file(&provider_a, &provider_source("Value "));
    write_file(&provider_b, &provider_source("Other "));
    assert_eq!(
        fs::metadata(&provider_a).unwrap().len(),
        fs::metadata(&provider_b).unwrap().len()
    );

    let mut projects = Vec::new();
    let mut packages = Vec::new();
    let mut consumers = Vec::new();
    let mut sources = Vec::new();
    for index in 0..OWNER_COUNT {
        let directory = root.join(format!("Project{index:02}"));
        let project = directory.join("App.dproj");
        let consumer = directory.join(format!("Consumer{index:02}.pas"));
        let source = format!(
            "unit Consumer{index:02};\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar Item: TSelected;\nbegin\n  Item.Value := 1;\nend;\nend.\n"
        );
        for flavor in ["A", "B"] {
            let package = root.join(format!("Package{flavor}{index:02}.dpk"));
            let mapped_provider = if flavor == "A" {
                "ProviderA"
            } else {
                "ProviderB"
            };
            let mut contents = format!(
                "package Package{flavor}{index:02};\ncontains\n  Provider in 'units/{mapped_provider}.pas';\n"
            );
            for candidate in 0..256 {
                contents.push_str(&format!(
                    "  Catalog{candidate:03} in 'catalogue/Catalog{candidate:03}.pas';\n"
                ));
            }
            contents.push_str("end.\n");
            write_file(&package, &contents);
            packages.push(package);
        }
        let selected_package = if index == 0 { "A" } else { "B" };
        let prefix = format!(
            "<Project><PropertyGroup><MainSource>Consumer{index:02}.pas</MainSource><DCC_UsePackage>Package{selected_package}{index:02}</DCC_UsePackage></PropertyGroup><!--"
        );
        let suffix = "--></Project>";
        let descriptor = format!(
            "{prefix}{}{suffix}",
            "p".repeat(PACKAGE_BYTES - prefix.len() - suffix.len())
        );
        assert_eq!(descriptor.len(), PACKAGE_BYTES);
        write_file(&project, &descriptor);
        write_file(&consumer, &source);
        projects.push(project);
        consumers.push(consumer);
        sources.push(source);
    }

    let barrier_environment = tempfile::tempdir().expect("temporary writer barrier");
    let barrier_dir = barrier_environment
        .path()
        .join("package-push-writer-barrier");
    assert!(
        !barrier_dir.starts_with(root),
        "writer barrier control files must be outside the analyzed workspace root"
    );
    fs::create_dir_all(&barrier_dir).expect("writer barrier directory");
    let writer_barrier = OutboundWriterBarrier {
        armed: barrier_dir.join("armed"),
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
        control_limit: 8,
    };
    let barrier_spec = format!(
        "{}|{}|{}|{}",
        writer_barrier.armed.display(),
        writer_barrier.entered.display(),
        writer_barrier.release.display(),
        writer_barrier.control_limit
    );
    let metrics = root.join("package-push-work.json");
    let metrics_spec = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root,
        [
            (
                "PASCAL_LSP_TEST_OUTBOUND_WRITER_BARRIER",
                barrier_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
                metrics_spec.as_str(),
            ),
        ],
    );
    server.initialize(root, Value::Null);
    for index in 0..OWNER_COUNT {
        let id = RequestId::from(format!("pkg-push-old-definition-{index}"));
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&consumers[index], &sources[index], "TSelected", 0),
        );
        let expected = if index == 0 { &provider_a } else { &provider_b };
        assert_eq!(
            result_locations(server.response(&id))[0]["uri"],
            uri(expected).to_string()
        );
    }
    for index in 0..OWNER_COUNT {
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri(&consumers[index]),"languageId":"pascal","version":1,"text":sources[index]}}),
        );
    }
    let mut old_publications = HashMap::new();
    for consumer in &consumers {
        let params = server
            .diagnostic_with_timeout(&uri(consumer), IO_TIMEOUT)
            .expect("old full push diagnostic");
        assert!(params["diagnostics"].is_array());
        old_publications.insert(uri(consumer).to_string(), params);
    }
    assert!(
        old_publications[&uri(&consumers[0]).to_string()]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|diagnostic| diagnostic["message"] != "missing member 'Value'")
    );

    let stamp = fs::metadata(&packages[0]).expect("package stamp");
    let old = fs::read_to_string(&packages[0]).expect("package bytes");
    let changed = old.replace("ProviderA", "ProviderB");
    assert_eq!(old.len(), changed.len());
    write_file(&packages[0], &changed);
    restore_mtime(&packages[0], &stamp);
    let mut changes = packages
        .iter()
        .map(|path| json!({"uri":uri(path),"type":2}))
        .chain(
            projects
                .iter()
                .map(|path| json!({"uri":uri(path),"type":2})),
        )
        .collect::<Vec<_>>();
    while changes.len() < BATCH_ENTRIES {
        let path = root.join(format!("unrelated-{}.pas", changes.len()));
        changes.push(json!({"uri":uri(&path),"type":2}));
    }
    assert_eq!(changes.len(), BATCH_ENTRIES);
    fs::write(&writer_barrier.armed, b"pause").expect("arm writer barrier");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":changes}),
    );
    assert!(
        wait_for_file(&metrics, IO_TIMEOUT),
        "notification worker must finish package fallback accounting"
    );
    let work: Value = serde_json::from_slice(&fs::read(&metrics).expect("read package metrics"))
        .expect("parse package metrics");
    assert!(
        work["budget_exceeded"].as_bool().unwrap_or(false),
        "package event must hit fallback: {work}"
    );
    assert!(
        work["package_path_visits"].as_u64().unwrap_or_default() > 0,
        "package descriptor work must be charged in this notification's shared account: {work}"
    );
    assert!(
        work["filesystem_path_visits"].as_u64().unwrap_or_default()
            >= work["package_path_visits"].as_u64().unwrap_or_default(),
        "package path charges contribute to the same notification filesystem-path account: {work}"
    );
    let processed_event_paths = work["file_event_path_visits"].as_u64().unwrap_or_default();
    assert!(
        processed_event_paths > 0 && processed_event_paths <= BATCH_ENTRIES as u64,
        "package and watched-file path work is charged within the 64-entry notification account before fallback: {work}"
    );
    writer_barrier.wait_until_entered();

    let new_definition_id = RequestId::from("pkg-push-new-definition".to_string());
    server.send_request(
        new_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &sources[0], "TSelected", 0),
    );
    thread::sleep(Duration::from_millis(100));
    let process_status = server.child.try_wait().expect("inspect server");
    assert!(
        server
            .response_with_timeout(&new_definition_id, Duration::from_millis(100))
            .is_none(),
        "new navigation response remains behind writer backpressure until release"
    );
    writer_barrier.release();
    assert!(
        process_status.is_none(),
        "server stays alive under writer backpressure: {process_status:?}"
    );

    let mut clear_uris = HashSet::new();
    let mut saw_new_full = false;
    let mut new_definition_response = None;
    let deadline = Instant::now() + IO_TIMEOUT;
    while !saw_new_full {
        let message = server.receive_until(deadline);
        match message {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                let uri_value = notification.params["uri"]
                    .as_str()
                    .expect("diagnostic URI")
                    .to_string();
                let diagnostics = notification.params["diagnostics"]
                    .as_array()
                    .expect("diagnostics array");
                if diagnostics.is_empty() {
                    clear_uris.insert(uri_value);
                } else if uri_value == uri(&consumers[0]).as_str() {
                    assert!(
                        diagnostics
                            .iter()
                            .any(|diagnostic| diagnostic["message"] == "missing member 'Value'"),
                        "new full diagnostic must reflect provider B, not stale provider A: {notification:?}"
                    );
                    saw_new_full = true;
                }
            }
            Message::Response(response) if response.id == new_definition_id => {
                new_definition_response = Some(response);
            }
            other => server.pending.push_back(other),
        }
    }
    assert!(
        clear_uris.contains(uri(&consumers[0]).as_str()),
        "fallback must publish bounded empty clear for stale consumer diagnostics: {clear_uris:?}"
    );
    assert!(
        !clear_uris.is_empty(),
        "push fallback must emit an empty clear"
    );
    assert!(
        clear_uris.len() <= OWNER_COUNT,
        "clear stream stays within retained roots: {}",
        clear_uris.len()
    );
    let new_definition =
        new_definition_response.unwrap_or_else(|| server.response(&new_definition_id));
    assert_eq!(
        result_locations(new_definition)[0]["uri"],
        uri(&provider_b).to_string()
    );
    server.shutdown();
    drop(barrier_environment);
}

#[test]
#[cfg(feature = "test-support")]
fn include_owner_catalogue_protocol_reports_actual_candidate_work() {
    const DECOYS: usize = 256;
    const BATCH: usize = 64;
    let root = tempfile::tempdir().expect("temporary workspace");
    let main = root.path().join("Main.pas");
    let include = root.path().join("Use.inc");
    let main_source = "unit Main;\ninterface\nconst RootValue = 1;\nimplementation\nprocedure Run;\nbegin\n{$I Use.inc}\nend;\nend.\n";
    let include_source = "Log(RootValue);\n";
    write_file(&main, main_source);
    write_file(&include, include_source);
    for index in 0..DECOYS {
        write_file(
            &root.path().join(format!("Decoy{index:03}.pas")),
            &format!("unit Decoy{index:03}; interface implementation end.\n"),
        );
    }
    let metrics = root.path().with_extension("include-query-work.json");
    let metrics_value = metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [(
            "PASCAL_LSP_TEST_QUERY_RECONCILIATION_WORK_RESULT",
            metrics_value.as_str(),
        )],
    );
    server.initialize_with_pull_diagnostics(root.path());
    let events = (0..BATCH)
        .map(|index| {
            let path = root.path().join(format!("batch-{index:02}.pas"));
            write_file(&path, "unit Batch; interface implementation end.\n");
            json!({"uri":uri(&path),"type":1})
        })
        .collect::<Vec<_>>();
    server.send_notification("workspace/didChangeWatchedFiles", json!({"changes":events}));
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&main),"languageId":"pascal","version":1,"text":main_source}}),
    );
    let references_id = RequestId::from("include-owner-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&include)},
            "position": position_of(include_source, "RootValue", 0),
            "context": {"includeDeclaration": true}
        }),
    );
    let references = server.response(&references_id);
    assert!(
        references.error.is_none(),
        "include references: {references:?}"
    );
    assert_eq!(
        references.result.unwrap().as_array().unwrap().len(),
        2,
        "the include owner must recover both the include and root declaration"
    );
    let id = RequestId::from("include-owner-definition".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&include, include_source, "RootValue", 0),
    );
    let _include_definition = result_locations(server.response(&id));
    assert!(wait_for_file(&metrics, IO_TIMEOUT));
    let metrics: Value = serde_json::from_slice(&fs::read(&metrics).unwrap()).unwrap();
    assert!(
        metrics["include_path_visits"].as_u64().unwrap_or_default() >= DECOYS as u64,
        "include-owner scan counts actual candidates: {metrics}"
    );
    assert!(
        metrics["filesystem_path_visits"]
            .as_u64()
            .unwrap_or_default()
            >= metrics["include_path_visits"].as_u64().unwrap_or_default(),
        "include candidate visits are included in the shared request-account total: {metrics}"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn include_owner_catalogue_cancellation_limit_and_fresh_retry_are_conservative() {
    const DECOYS: usize = 257;
    let root = tempfile::tempdir().expect("temporary workspace");
    let main = root.path().join("Main.pas");
    let include = root.path().join("Use.inc");
    let disk_source = "unit Main;\ninterface\nconst RootValue = 1;\nimplementation\nprocedure Run;\nbegin\n{$I Use.inc}\nend;\nend.\n";
    let overlay_source = disk_source.replace("= 1", "= 2");
    assert_eq!(disk_source.len(), overlay_source.len());
    let include_source = "Log(RootValue);\n";
    write_file(&main, disk_source);
    write_file(&include, include_source);
    let mut decoys = Vec::with_capacity(DECOYS);
    for index in 0..DECOYS {
        let path = root.path().join(format!("Decoy{index:03}.pas"));
        write_file(
            &path,
            &format!("unit Decoy{index:03}; interface implementation end.\n"),
        );
        decoys.push(path);
    }

    let barrier_dir = root.path().join("include-catalogue-cancel-barrier");
    fs::create_dir_all(&barrier_dir).expect("catalogue barrier directory");
    let barrier_entered = barrier_dir.join("entered.json");
    let barrier_release = barrier_dir.join("release");
    let barrier_spec = format!(
        "{}|{}",
        barrier_entered.display(),
        barrier_release.display()
    );
    let references_entered = barrier_dir.join("references-entered");
    let references_release = barrier_dir.join("references-release");
    let references_spec = format!(
        "{}|{}",
        references_entered.display(),
        references_release.display()
    );
    let rename_entered = barrier_dir.join("rename-entered");
    let rename_release = barrier_dir.join("rename-release");
    fs::write(&rename_release, b"release").expect("initially release navigation barrier");
    let navigation_spec = format!("{}|{}", rename_entered.display(), rename_release.display());
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_INCLUDE_CATALOGUE_BARRIER",
                barrier_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_REFERENCES_BARRIER",
                references_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                navigation_spec.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&main),"languageId":"pascal","version":1,"text":overlay_source}}),
    );
    let prior_pull_id = RequestId::from("include-catalogue-prior-pull".to_string());
    server.send_request(
        prior_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":null}),
    );
    let prior_pull = server.response(&prior_pull_id);
    assert!(
        prior_pull.error.is_none(),
        "prior pull diagnostics: {prior_pull:?}"
    );
    let prior_result_id = prior_pull.result.unwrap()["resultId"]
        .as_str()
        .expect("prior result ID")
        .to_owned();
    assert!(
        !barrier_entered.exists(),
        "the include-owner iterator barrier must be attributable to the following navigation request"
    );

    let cancelled_definition_id =
        RequestId::from("include-catalogue-cancelled-definition".to_string());
    server.send_request(
        cancelled_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&include, include_source, "RootValue", 0),
    );
    assert!(
        wait_for_file(&barrier_entered, IO_TIMEOUT),
        "request must rendezvous after an actual charged filename-catalogue iterator step"
    );
    let barrier_evidence: Value =
        serde_json::from_slice(&fs::read(&barrier_entered).expect("catalogue barrier evidence"))
            .expect("catalogue barrier JSON");
    assert!(
        barrier_evidence["charged_visits"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    server.send_notification("$/cancelRequest", json!({"id":cancelled_definition_id}));
    let unrelated_id = RequestId::from("include-catalogue-control-request".to_string());
    server.send_request(
        unrelated_id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument":{"uri":uri(&main)}}),
    );
    let unrelated = server.response(&unrelated_id);
    assert!(
        unrelated.error.is_none(),
        "unrelated control request must be served during catalogue cancellation: {unrelated:?}"
    );
    let cancelled = server.response(&cancelled_definition_id);
    assert_eq!(
        cancelled.error.expect("cancelled definition response").code,
        -32800
    );
    assert!(
        cancelled.result.is_none(),
        "cancelled definition cannot return partial locations"
    );

    fs::remove_file(&rename_entered).ok();
    fs::remove_file(&rename_release).expect("arm navigation barrier for rename cancellation");
    let cancelled_rename_id = RequestId::from("include-catalogue-cancelled-rename".to_string());
    server.send_request(
        cancelled_rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument":{"uri":uri(&main)},
            "position":position_of(&overlay_source,"RootValue",0),
            "newName":"CancelledRootValue"
        }),
    );
    assert!(
        wait_for_file(&rename_entered, IO_TIMEOUT),
        "rename request barrier reached"
    );
    server.send_notification("$/cancelRequest", json!({"id":cancelled_rename_id}));
    let rename_control_id = RequestId::from("include-catalogue-rename-control".to_string());
    server.send_request(
        rename_control_id.clone(),
        "textDocument/foldingRange",
        json!({"textDocument":{"uri":uri(&main)}}),
    );
    let rename_control = server.response(&rename_control_id);
    assert!(
        rename_control.error.is_none(),
        "unrelated control request during rename cancellation: {rename_control:?}"
    );
    let rename_cancelled = server.response(&cancelled_rename_id);
    assert_eq!(
        rename_cancelled
            .error
            .expect("cancelled rename response")
            .code,
        -32800
    );
    assert!(
        rename_cancelled.result.is_none(),
        "cancelled rename cannot return partial workspace edits"
    );
    fs::write(&rename_release, b"release").expect("release navigation barrier after cancellation");

    let limited_definition_id = RequestId::from("include-catalogue-limited-definition".to_string());
    server.send_request(
        limited_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&include, include_source, "RootValue", 0),
    );
    let limited_definition = server.response(&limited_definition_id);
    assert!(
        limited_definition.error.is_some()
            || limited_definition.result.as_ref().is_none_or(|result| {
                result.is_null() || result.as_array().is_some_and(Vec::is_empty)
            }),
        "candidate-limit definition must fail closed without partial locations: {limited_definition:?}"
    );

    fs::remove_file(&references_entered).ok();
    fs::remove_file(&references_release).ok();
    let references_id = RequestId::from("include-catalogue-cancelled-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument":{"uri":uri(&include)},
            "position":position_of(include_source,"RootValue",0),
            "context":{"includeDeclaration":true}
        }),
    );
    assert!(
        wait_for_file(&references_entered, IO_TIMEOUT),
        "reference request barrier reached"
    );
    server.send_notification("$/cancelRequest", json!({"id":references_id}));
    let references = server.response(&references_id);
    assert_eq!(
        references.error.expect("cancelled reference response").code,
        -32800
    );
    assert!(
        references.result.is_none(),
        "cancelled references cannot return partial locations"
    );
    fs::write(&references_release, b"release")
        .expect("release references barrier after cancellation");

    let main_stamp = fs::metadata(&main).expect("main disk stamp before same-size edit");
    let changed_disk_source = disk_source.replace("RootValue", "DiskValue");
    assert_eq!(disk_source.len(), changed_disk_source.len());
    write_file(&main, &changed_disk_source);
    restore_mtime(&main, &main_stamp);
    let changed_include_source = "Log(UnknownValue);\n";
    write_file(&include, changed_include_source);
    for decoy in &decoys {
        fs::remove_file(decoy).expect("remove owner-catalogue decoy");
    }
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[
            {"uri":uri(&decoys[0]),"type":3},
            {"uri":uri(&include),"type":2}
        ]}),
    );
    let repaired_pull_id = RequestId::from("include-catalogue-repaired-pull".to_string());
    server.send_request(
        repaired_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&main)},"previousResultId":prior_result_id}),
    );
    let repaired_pull = server.response(&repaired_pull_id);
    assert!(
        repaired_pull.error.is_none(),
        "repaired pull diagnostics: {repaired_pull:?}"
    );
    assert_ne!(
        repaired_pull.result.as_ref().unwrap()["kind"],
        "unchanged",
        "relevant watcher event must invalidate the prior pull result ID"
    );
    write_file(&include, include_source);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&include),"type":2}]}),
    );
    let fresh_definition_id = RequestId::from("include-catalogue-fresh-definition".to_string());
    server.send_request(
        fresh_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&include, include_source, "RootValue", 0),
    );
    assert_eq!(
        result_locations(server.response(&fresh_definition_id))[0]["uri"],
        uri(&main).to_string(),
        "complete retry must select the open owner overlay, not same-stamp disk contents"
    );
    let fresh_references_id = RequestId::from("include-catalogue-fresh-references".to_string());
    server.send_request(
        fresh_references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument":{"uri":uri(&include)},
            "position":position_of(include_source,"RootValue",0),
            "context":{"includeDeclaration":true}
        }),
    );
    let fresh_references = server.response(&fresh_references_id);
    assert!(
        fresh_references.error.is_none(),
        "fresh owner references: {fresh_references:?}"
    );
    assert_eq!(
        fresh_references.result.unwrap().as_array().unwrap().len(),
        2
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn shutdown_cancels_workspace_worker_while_diagnostic_fanout_is_blocked() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\ntype TItem = record end;\nimplementation\nend.\n",
    );
    let consumer = root.path().join("Consumer.pas");
    write_file(
        &consumer,
        "unit Consumer;\ninterface\nuses Provider;\ntype TUse = TItem;\nimplementation\nend.\n",
    );
    let barrier_dir = root.path().join("diagnostic-work-cancel-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let completed = barrier_dir.join("completed");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let completed_value = completed.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_DIAGNOSTIC_WORK_BARRIER",
                barrier_value.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
                completed_value.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":"unit Consumer;\ninterface\nuses Provider;\ntype TUse = TItem;\nimplementation\nend.\n"}}),
    );
    let diagnostics_id = RequestId::from("diagnostic-work-cancel-load".to_string());
    server.send_request(
        diagnostics_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":null}),
    );
    assert!(server.response(&diagnostics_id).error.is_none());
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":2}]}),
    );

    let entered = wait_for_file(&barrier.entered, Duration::from_millis(250));
    let shutdown_response = if entered {
        let shutdown_id = RequestId::from("shutdown-diagnostic-work-held".to_string());
        server.send_request(shutdown_id.clone(), "shutdown", Value::Null);
        let response = server.response_with_timeout(&shutdown_id, Duration::from_millis(500));
        let completed_before_response = wait_for_file(&completed, Duration::from_millis(50));
        barrier.release();
        assert!(
            completed_before_response,
            "shutdown must join worker before responding"
        );
        response
    } else {
        barrier.release();
        None
    };
    if shutdown_response.is_some() {
        server.send_notification("exit", Value::Null);
        server.stdin.take();
        let _ = server.child.wait().expect("wait for shutdown child");
    } else {
        server.shutdown();
    }
    assert!(
        entered,
        "diagnostic dependent enumeration must expose a deterministic work barrier"
    );
    assert!(
        shutdown_response.is_some_and(|response| response.error.is_none()),
        "shutdown must remain responsive while diagnostic work is held"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn shutdown_bypasses_full_workspace_fifo_and_overflow_without_releasing_worker_barrier() {
    assert_saturated_control_bypasses_workspace_fifo(false, 0, false, None, false, false);
}

#[test]
#[cfg(feature = "test-support")]
fn exit_bypasses_full_workspace_fifo_and_overflow_without_releasing_worker_barrier() {
    assert_saturated_control_bypasses_workspace_fifo(true, 0, false, None, false, false);
}

#[test]
#[cfg(feature = "test-support")]
fn shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline() {
    assert_saturated_control_bypasses_workspace_fifo(false, 1, false, Some(30_000), false, false);
}

#[test]
#[cfg(all(feature = "test-support", target_os = "linux"))]
fn saturated_workspace_fifo_replays_watched_and_document_events_in_order() {
    const FILLER_NOTIFICATIONS: usize = 56;
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let moved_provider = root.path().join("Provider.pas.moved");
    let consumer = root.path().join("Consumer.pas");
    let unrelated = root.path().join("Unrelated.pas");
    let old_disk = "unit Provider;\ninterface\ntype TBefore = Integer;\nimplementation\nend.\n";
    let recreated_disk =
        "unit Provider;\ninterface\ntype TOnDisk = Integer;\nimplementation\nend.\n";
    let changed_overlay =
        "unit Provider;\ninterface\ntype TChanged = Integer;\nimplementation\nend.\n";
    let reopened_overlay =
        "unit Provider;\ninterface\ntype TFinal = Integer;\nimplementation\nend.\n";
    assert_eq!(old_disk.len(), recreated_disk.len());
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\ntype TBeforeUse = Provider.TBefore;\ntype TChangedUse = Provider.TChanged;\ntype TDiskUse = Provider.TOnDisk;\ntype TFinalUse = Provider.TFinal;\nimplementation\nend.\n";
    write_file(&provider, old_disk);
    write_file(&consumer, consumer_source);
    let unrelated_source =
        "unit Unrelated;\ninterface\ntype TLocal = Integer;\nimplementation\nend.\n";
    write_file(&unrelated, unrelated_source);

    let barrier_dir = root.path().join("saturated-order-barriers");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let discovery_barrier = TestBarrier {
        entered: barrier_dir.join("discovery-entered"),
        release: barrier_dir.join("discovery-release"),
    };
    let navigation_barrier = TestBarrier {
        entered: barrier_dir.join("navigation-entered"),
        release: barrier_dir.join("navigation-release"),
    };
    let fifo_state = barrier_dir.join("fifo-state.json");
    let reader_pending = barrier_dir.join("reader-pending.json");
    let discovery_spec = format!(
        "{}|{}",
        discovery_barrier.entered.display(),
        discovery_barrier.release.display()
    );
    let navigation_spec = format!(
        "{}|{}",
        navigation_barrier.entered.display(),
        navigation_barrier.release.display()
    );
    let fifo_probe_spec = format!(
        "{}|{}|{}|{}",
        fifo_state.display(),
        reader_pending.display(),
        discovery_barrier.entered.display(),
        discovery_barrier.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                discovery_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                navigation_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_FIFO_PROBE",
                fifo_probe_spec.as_str(),
            ),
        ],
    );
    server.initialize_with_pull_diagnostics(root.path());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":consumer_source}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":1,"text":old_disk}}),
    );
    let prior_pull_id = RequestId::from("saturated-order-prior-pull".to_string());
    server.send_request(
        prior_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":null}),
    );
    let prior_pull = server.response(&prior_pull_id);
    assert!(prior_pull.error.is_none(), "initial pull: {prior_pull:?}");
    let prior_result_id = prior_pull.result.expect("initial pull result")["resultId"]
        .as_str()
        .expect("initial result ID")
        .to_owned();

    let trigger = root.path().join("block-discovery.pas");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&trigger),"type":2}]}),
    );
    discovery_barrier.wait_until_entered();

    let mut sent_fifo_frames = Vec::new();
    for index in 0..FILLER_NOTIFICATIONS {
        let path = root.path().join(format!("Queued{index:02}.pas"));
        let message = Message::Notification(Notification::new(
            "textDocument/didOpen".to_string(),
            json!({"textDocument":{"uri":uri(&path),"languageId":"pascal","version":1,"text":"unit Queued; interface implementation end."}}),
        ));
        server.send_tracked(message, &mut sent_fifo_frames);
    }

    let cancel_target_id = RequestId::from("saturated-order-unrelated-cancel-target".to_string());
    server.send_tracked(
        Message::Request(Request::new(
            cancel_target_id.clone(),
            "textDocument/definition".to_string(),
            navigation_params(&unrelated, unrelated_source, "TLocal", 0),
        )),
        &mut sent_fifo_frames,
    );

    // FIFO entries 58–64: old provider delete is reported before unlink;
    // create follows a same-size disk replacement with restored mtime. Then
    // didChange/request/didClose/request/didOpen establish observable order.
    server.send_tracked(
        Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_string(),
            json!({"changes":[{"uri":uri(&provider),"type":3}]}),
        )),
        &mut sent_fifo_frames,
    );
    let old_stamp = fs::metadata(&provider).expect("old provider stamp");
    fs::rename(&provider, &moved_provider).expect("client move before watched create");
    write_file(&provider, recreated_disk);
    restore_mtime(&provider, &old_stamp);
    server.send_tracked(
        Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_string(),
            json!({"changes":[{"uri":uri(&provider),"type":1}]}),
        )),
        &mut sent_fifo_frames,
    );
    server.send_tracked(
        Message::Notification(Notification::new(
            "textDocument/didChange".to_string(),
            json!({"textDocument":{"uri":uri(&provider),"version":2},"contentChanges":[{"text":changed_overlay}]}),
        )),
        &mut sent_fifo_frames,
    );
    let changed_pull_id = RequestId::from("saturated-order-pull-after-change".to_string());
    server.send_tracked(
        Message::Request(Request::new(
            changed_pull_id.clone(),
            "textDocument/diagnostic".to_string(),
            json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":prior_result_id}),
        )),
        &mut sent_fifo_frames,
    );
    server.send_tracked(
        Message::Notification(Notification::new(
            "textDocument/didClose".to_string(),
            json!({"textDocument":{"uri":uri(&provider)}}),
        )),
        &mut sent_fifo_frames,
    );
    let close_pull_id = RequestId::from("saturated-order-pull-after-close".to_string());
    server.send_tracked(
        Message::Request(Request::new(
            close_pull_id.clone(),
            "textDocument/diagnostic".to_string(),
            json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":prior_result_id}),
        )),
        &mut sent_fifo_frames,
    );
    server.send_tracked(
        Message::Notification(Notification::new(
            "textDocument/didOpen".to_string(),
            json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":3,"text":reopened_overlay}}),
        )),
        &mut sent_fifo_frames,
    );
    // Entry 65 is the retained overflow slot; entry 66 is the final request.
    let pull_after_id = RequestId::from("saturated-order-after-pull".to_string());
    let overflow_message = Message::Request(Request::new(
        pull_after_id.clone(),
        "textDocument/diagnostic".to_string(),
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":prior_result_id}),
    ));
    sent_fifo_frames.push(overflow_message.clone());
    server.send(overflow_message);
    // Entry 66 is an ordinary request behind the full FIFO and overflow slot.
    // It must remain unanswered until the held worker is explicitly released.
    let final_definition_id = RequestId::from("saturated-order-after-open".to_string());
    let final_message = Message::Request(Request::new(
        final_definition_id.clone(),
        "textDocument/definition".to_string(),
        navigation_params(&consumer, consumer_source, "Provider.TFinal", 0),
    ));
    assert_eq!(sent_fifo_frames.len(), 65, "64 FIFO frames plus overflow");
    assert_eq!(FILLER_NOTIFICATIONS + 1 + 7, 64, "fill the FIFO exactly");
    assert_eq!(
        sent_fifo_frames.len() + 1,
        66,
        "66th frame is ordinary input"
    );

    assert!(
        wait_until_json_bool(&fifo_state, "full", true, IO_TIMEOUT),
        "must observe actual full FIFO/overflow admission while discovery remains held"
    );
    let full_state: Value = serde_json::from_slice(&fs::read(&fifo_state).expect("FIFO state"))
        .expect("valid FIFO probe JSON");
    let fifo_bytes = sent_fifo_frames[..64]
        .iter()
        .map(|message| {
            serde_json::to_vec(message)
                .expect("serialize admitted FIFO message")
                .len()
        })
        .sum::<usize>();
    let overflow_bytes = serde_json::to_vec(&sent_fifo_frames[64])
        .expect("serialize overflow message")
        .len();
    assert_eq!(
        full_state["queued_count"], 64,
        "probe snapshot: {full_state}"
    );
    assert_eq!(full_state["queued_bytes"], fifo_bytes);
    assert_eq!(full_state["overflow_occupied"], true);
    assert_eq!(full_state["overflow_bytes"], overflow_bytes);
    assert_eq!(full_state["worker_held"], true);
    assert_eq!(full_state["reader_pending"], false);
    assert_eq!(full_state["full"], true);
    assert!(fifo_bytes + overflow_bytes <= 1024 * 1024);
    let final_frame_bytes = serde_json::to_vec(&final_message)
        .expect("serialize 66th request")
        .len();

    assert!(
        !discovery_barrier.release.exists(),
        "worker remains blocked while requests are deferred"
    );
    server.send(final_message);
    assert!(
        wait_until_json_bool(&fifo_state, "reader_pending", true, IO_TIMEOUT),
        "66th frame must be pending at reader rendezvous"
    );
    let pending_state: Value =
        serde_json::from_slice(&fs::read(&fifo_state).expect("reader-pending FIFO state"))
            .expect("valid reader-pending JSON");
    assert_eq!(pending_state["reader_pending"], true);
    assert_eq!(
        pending_state["reader_pending_method"],
        "textDocument/definition"
    );
    assert_eq!(pending_state["reader_pending_bytes"], final_frame_bytes);
    assert_eq!(pending_state["worker_held"], true);
    assert!(wait_for_file(&reader_pending, IO_TIMEOUT));
    let reader_state: Value =
        serde_json::from_slice(&fs::read(&reader_pending).expect("reader pending marker"))
            .expect("valid reader marker");
    assert_eq!(reader_state["reader_pending"], true);
    assert_eq!(reader_state["method"], "textDocument/definition");
    assert_eq!(reader_state["frame_bytes"], final_frame_bytes);
    assert_eq!(reader_state["worker_held"], true);
    assert!(fifo_bytes + overflow_bytes + final_frame_bytes <= 1024 * 1024);
    assert!(
        server
            .response_with_timeout(&final_definition_id, Duration::from_millis(100))
            .is_none(),
        "66th request remains unread by event loop while full FIFO holds the worker"
    );
    server.send_notification("$/cancelRequest", json!({"id":cancel_target_id}));
    discovery_barrier.release();

    navigation_barrier.wait_until_entered();
    let cancelled = server.response(&cancel_target_id);
    assert_eq!(cancelled.error.expect("cancel response").code, -32800);

    for (request_id, stage) in [
        (&changed_pull_id, "didChange"),
        (&close_pull_id, "didClose"),
        (&pull_after_id, "didOpen"),
    ] {
        let response = server.response_acknowledging_diagnostic_refresh(request_id);
        assert!(
            response.error.is_some()
                || response
                    .result
                    .as_ref()
                    .is_some_and(|result| result["kind"] != "unchanged"),
            "{stage} must not reuse the pre-event pull result ID: {response:?}"
        );
    }
    navigation_barrier.release();
    let final_response = server.response_acknowledging_diagnostic_refresh(&final_definition_id);
    assert!(
        final_response.error.is_none(),
        "final request: {final_response:?}"
    );
    let final_locations = result_locations(final_response);
    assert_eq!(final_locations[0]["uri"], uri(&provider).to_string());
    let fresh_pull_id = RequestId::from("saturated-order-fresh-pull-after-release".to_string());
    server.send_request(
        fresh_pull_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&consumer)},"previousResultId":prior_result_id}),
    );
    let fresh_pull = server.response_acknowledging_diagnostic_refresh(&fresh_pull_id);
    assert!(
        fresh_pull.error.is_none(),
        "fresh post-release pull: {fresh_pull:?}"
    );
    assert_ne!(
        fresh_pull.result.as_ref().unwrap()["kind"],
        "unchanged",
        "fresh pull must not reuse the pre-event result ID"
    );

    server.shutdown();
}

#[test]
#[cfg(all(feature = "test-support", target_os = "linux"))]
fn blocked_watched_delete_rejects_old_result_and_pushes_fresh_provider_diagnostics() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let moved_provider = root.path().join("Provider.pas.moved");
    let consumer = root.path().join("Consumer.pas");
    let blocker = root.path().join("Blocker.pas");
    let old_disk = "unit Provider;\ninterface\ntype TBefore = Integer;\nimplementation\nend.\n";
    let recreated_disk =
        "unit Provider;\ninterface\ntype TOnDisk = Integer;\nimplementation\nend.\n";
    let changed_overlay =
        "unit Provider;\ninterface\ntype TChanged = Integer;\nimplementation\nend.\n";
    let reopened_overlay =
        "unit Provider;\ninterface\ntype TNew = Integer;\nimplementation\nend.\n";
    let consumer_v1 = "unit Consumer;\ninterface\nuses Provider;\nconst badConst = 1;\ntype TUseNew = Provider.TNew;\ntype TOldUse = Provider.TBefore;\nimplementation\nend.\n";
    let consumer_v2 = "unit Consumer;\ninterface\nuses Provider;\nconst BAD_CONST = 2;\ntype TUseNew = Provider.TNew;\nimplementation\nend.\n";
    let consumer_v3 = "unit Consumer;\ninterface\nuses Provider;\nconst badConst = 3;\ntype TUseNew = Provider.TNew;\nimplementation\nend.\n";
    assert_eq!(old_disk.len(), recreated_disk.len());
    write_file(&provider, old_disk);
    write_file(&consumer, consumer_v1);
    write_file(
        &blocker,
        "unit Blocker;\ninterface\ntype TBlock = Integer;\nimplementation\nend.\n",
    );

    let barrier_dir = root.path().join("provider-freshness-barriers");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let discovery_barrier = TestBarrier {
        entered: barrier_dir.join("discovery-entered"),
        release: barrier_dir.join("discovery-release"),
    };
    let navigation_barrier = TestBarrier {
        entered: barrier_dir.join("navigation-entered"),
        release: barrier_dir.join("navigation-release"),
    };
    let discovery_spec = format!(
        "{}|{}",
        discovery_barrier.entered.display(),
        discovery_barrier.release.display()
    );
    let navigation_spec = format!(
        "{}|{}",
        navigation_barrier.entered.display(),
        navigation_barrier.release.display()
    );
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        [
            (
                "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
                discovery_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                navigation_spec.as_str(),
            ),
        ],
    );
    server.initialize(root.path(), Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":consumer_v1}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":1,"text":old_disk}}),
    );
    let initial = server
        .diagnostic_with_timeout(&uri(&consumer), IO_TIMEOUT)
        .expect("initial push diagnostics for the lower-case constant");
    assert!(
        !initial["diagnostics"].as_array().unwrap().is_empty(),
        "initial consumer report must have a real naming diagnostic"
    );
    let blocker_symbols = RequestId::from("provider-freshness-load-blocker".to_string());
    server.send_request(
        blocker_symbols.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":uri(&blocker)}}),
    );
    assert!(server.response(&blocker_symbols).error.is_none());

    let old_query_id = RequestId::from("provider-freshness-old-definition".to_string());
    server.send_request(
        old_query_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_v1, "Provider.TBefore", 0),
    );
    assert!(
        server
            .response_with_timeout(&old_query_id, Duration::from_millis(100))
            .is_none(),
        "old provider navigation must still be in flight before the barrier is inspected"
    );
    navigation_barrier.wait_until_entered();

    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&blocker),"type":2}]}),
    );
    discovery_barrier.wait_until_entered();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":3}]}),
    );
    let old_stamp = fs::metadata(&provider).expect("old provider mtime");
    fs::rename(&provider, &moved_provider).expect("client move after Deleted notification");
    write_file(&provider, recreated_disk);
    restore_mtime(&provider, &old_stamp);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&provider),"type":1}]}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri(&provider),"version":2},"contentChanges":[{"text":changed_overlay}]}),
    );
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument":{"uri":uri(&provider)}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":3,"text":reopened_overlay}}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri(&consumer),"version":2},"contentChanges":[{"text":consumer_v2}]}),
    );
    discovery_barrier.release();

    let clear = server
        .diagnostic_with_timeout(&uri(&consumer), IO_TIMEOUT)
        .expect("clean current diagnostics after provider and consumer replay");
    assert!(
        clear["diagnostics"].as_array().is_some_and(Vec::is_empty),
        "fresh v2 publication must clear the prior naming diagnostic: {clear}"
    );
    assert_eq!(clear["version"], 2);
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri(&consumer),"version":3},"contentChanges":[{"text":consumer_v3}]}),
    );
    let fresh = server
        .diagnostic_with_timeout(&uri(&consumer), IO_TIMEOUT)
        .expect("fresh push diagnostics after the v3 consumer edit");
    assert_eq!(
        fresh["version"], 3,
        "fresh report must match the latest consumer overlay"
    );
    assert!(
        !fresh["diagnostics"].as_array().unwrap().is_empty(),
        "fresh report must reflect the current naming diagnostic"
    );

    let final_query_id = RequestId::from("provider-freshness-new-definition".to_string());
    server.send_request(
        final_query_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_v3, "Provider.TNew", 0),
    );
    navigation_barrier.release();
    let old_response = server.response(&old_query_id);
    assert!(
        old_response.error.is_some() || result_locations(old_response).is_empty(),
        "in-flight result against the old provider identity must be discarded"
    );
    let new_response = server.response(&final_query_id);
    let new_locations = result_locations(new_response);
    assert_eq!(new_locations.len(), 1);
    assert_eq!(
        new_locations[0]["uri"],
        uri(&provider).to_string(),
        "fresh navigation must resolve to the recreated, currently-open provider"
    );
    server.shutdown();
}

#[test]
#[cfg(feature = "test-support")]
fn exit_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline() {
    assert_saturated_control_bypasses_workspace_fifo(true, 1, false, Some(30_000), false, false);
}

#[test]
#[cfg(feature = "test-support")]
fn cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline() {
    assert_saturated_control_bypasses_workspace_fifo(false, 1, true, Some(30_000), false, false);
}

#[test]
#[cfg(feature = "test-support")]
fn worker_deadline_fallback_restores_unprocessed_delete_tombstone() {
    assert_saturated_control_bypasses_workspace_fifo(false, 1, false, Some(30_000), true, true);
}

#[test]
#[cfg(feature = "test-support")]
fn budget_fallback_rejects_transferred_rename_overlay_and_invalidates_pull_result() {
    assert_rename_overlay_is_invalidated_on_reconciliation_fallback(None);
}

#[test]
#[cfg(feature = "test-support")]
fn deadline_fallback_rejects_transferred_rename_overlay_and_invalidates_pull_result() {
    assert_rename_overlay_is_invalidated_on_reconciliation_fallback(Some(30_000));
}

#[cfg(feature = "test-support")]
#[cfg(target_os = "linux")]
#[test]
fn unattributable_malformed_watcher_discards_without_fencing_queued_and_inflight_queries() {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace directory");
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let unrelated = root.join("Unrelated.pas");
    let provider_source =
        "unit Provider;\ninterface\ntype TOldThing = class end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\ntype TAlias = Provider.TOldThing;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&unrelated, "unit Unrelated; interface end.\n");
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let inflight_id = RequestId::from("malformed-watcher-inflight-before".to_string());
    server.send_request(
        inflight_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOldThing", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[
            {"uri":uri(&unrelated),"type":1},
            {"uri":17,"type":3}
        ]}),
    );
    let queued_id = RequestId::from("malformed-watcher-queued-after".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOldThing", 0),
    );
    // TASK-97: the discard rebuilds from disk, where the provider is
    // unchanged; no query is refused by a fence.
    barrier.release();
    let queued = result_locations(server.response(&queued_id));
    assert_eq!(queued.len(), 1, "queued query answers from disk");
    assert_eq!(queued[0]["uri"], uri(&provider).to_string());
    let inflight = server.response(&inflight_id);
    // The in-flight query was computed before the discard: it may be
    // refused as stale, but it must never name anything but the provider.
    if inflight.error.is_none() {
        let locations = result_locations(inflight);
        assert_eq!(locations.len(), 1, "in-flight query result");
        assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    }
    server.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
#[cfg(feature = "test-support")]
fn worker_file_notification_error_after_mutation_recovers_staling_and_overlay_state() {
    for pull_diagnostics in [false, true] {
        let environment = tempfile::tempdir().expect("isolated server environment");
        let root = environment.path().join("workspace");
        fs::create_dir_all(&root).expect("workspace root");
        let provider = root.join("Provider.pas");
        let sibling = root.join("Sibling.pas");
        let old_consumer = root.join("OldConsumer.pas");
        let new_consumer = root.join("NewConsumer.pas");
        let old_provider_source = "unit Provider;\ninterface\nconst\n  badConst = 1;\ntype TOldThing = class end;\nimplementation\nend.\n";
        let new_provider_source = old_provider_source
            .replace("badConst", "GOODNAME")
            .replace("TOldThing", "TNewThing");
        let old_consumer_source = "unit OldConsumer;\ninterface\nuses Provider;\ntype TAlias = Provider.TOldThing;\nimplementation\nend.\n";
        let new_consumer_source = "unit NewConsumer;\ninterface\nuses Provider;\ntype TAlias = Provider.TNewThing;\nimplementation\nend.\n";
        let main_source =
            "unit Main;\ninterface\nuses OldConsumer, NewConsumer;\nimplementation\nend.\n";
        write_file(&provider, old_provider_source);
        write_file(&sibling, "unit Sibling;\ninterface\nimplementation\nend.\n");
        write_file(&old_consumer, old_consumer_source);
        write_file(&new_consumer, new_consumer_source);
        write_file(&root.join("Main.pas"), main_source);
        write_file(
            &root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        );

        let mut server = TestServer::launch_with_file_event_failure(&environment, &uri(&sibling));
        if pull_diagnostics {
            server.initialize_with_pull_diagnostics(&root);
        } else {
            server.initialize_with_watched_registration(&root, Value::Null, false);
        }
        if !pull_diagnostics {
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&provider),
                        "languageId": "pascal",
                        "version": 1,
                        "text": old_provider_source,
                    }
                }),
            );
        }

        let old_definition_id =
            RequestId::from(format!("worker-error-old-before-{pull_diagnostics}"));
        server.send_request(
            old_definition_id.clone(),
            "textDocument/definition",
            navigation_params(&old_consumer, old_consumer_source, "TOldThing", 0),
        );
        let old_locations = result_locations(server.response(&old_definition_id));
        assert_eq!(old_locations.len(), 1);
        assert_eq!(old_locations[0]["uri"], uri(&provider).to_string());

        let new_definition_id =
            RequestId::from(format!("worker-error-new-negative-{pull_diagnostics}"));
        server.send_request(
            new_definition_id.clone(),
            "textDocument/definition",
            navigation_params(&new_consumer, new_consumer_source, "TNewThing", 0),
        );
        assert!(result_locations(server.response(&new_definition_id)).is_empty());

        let previous_result_id = if pull_diagnostics {
            let id = RequestId::from(format!("worker-error-pull-before-{pull_diagnostics}"));
            server.send_request(
                id.clone(),
                "textDocument/diagnostic",
                json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
            );
            let response = server.response(&id);
            assert!(
                response.error.is_none(),
                "initial pull failed: {response:?}"
            );
            let report = response.result.expect("initial pull result");
            assert!(report["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| item["code"] == "constant-naming")
            }));
            Some(
                report["resultId"]
                    .as_str()
                    .expect("initial pull result ID")
                    .to_string(),
            )
        } else {
            let initial = server
                .diagnostic_with_timeout(&uri(&provider), IO_TIMEOUT)
                .expect("open overlay must publish initial diagnostics");
            assert!(initial["diagnostics"].as_array().is_some_and(|items| {
                items.iter().any(|item| item["code"] == "constant-naming")
            }));
            None
        };

        let old_metadata = fs::metadata(&provider).expect("provider metadata before edit");
        write_file(&provider, &new_provider_source);
        restore_mtime(&provider, &old_metadata);
        server.send_notification(
            "workspace/didCreateFiles",
            json!({"files":[{"uri":uri(&provider)},{"uri":uri(&sibling)}]}),
        );

        if pull_diagnostics {
            let refresh = server
                .request_with_timeout("workspace/diagnostic/refresh", IO_TIMEOUT)
                .expect("partially mutating worker error must refresh pull diagnostics");
            server.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
            let id = RequestId::from(format!("worker-error-pull-after-{pull_diagnostics}"));
            server.send_request(
                id.clone(),
                "textDocument/diagnostic",
                json!({
                    "textDocument":{"uri":uri(&provider)},
                    "previousResultId":previous_result_id,
                }),
            );
            let response = server.response(&id);
            assert!(
                response.error.is_none(),
                "post-error pull failed: {response:?}"
            );
            assert_eq!(response.result.as_ref().unwrap()["kind"], "full");
            assert!(
                response.result.as_ref().unwrap()["items"]
                    .as_array()
                    .is_some_and(|items| items
                        .iter()
                        .all(|item| item["code"] != "constant-naming"))
            );
        } else {
            let clear = server
                .diagnostic_with_timeout(&uri(&provider), IO_TIMEOUT)
                .expect("partially mutating worker error must clear stale push diagnostics");
            assert!(clear["diagnostics"].as_array().is_some_and(Vec::is_empty));
        }

        if !pull_diagnostics {
            // This worker notification is not a rename: the provider overlay
            // stays attached to its original URI and hides changed disk bytes.
            let overlay_definition_id =
                RequestId::from("worker-error-overlay-not-transferred-push".to_string());
            server.send_request(
                overlay_definition_id.clone(),
                "textDocument/definition",
                navigation_params(&old_consumer, old_consumer_source, "TOldThing", 0),
            );
            let overlay_locations = result_locations(server.response(&overlay_definition_id));
            assert_eq!(overlay_locations.len(), 1);
            assert_eq!(overlay_locations[0]["uri"], uri(&provider).to_string());
            let still_negative_id =
                RequestId::from("worker-error-overlay-hides-disk-push".to_string());
            server.send_request(
                still_negative_id.clone(),
                "textDocument/definition",
                navigation_params(&new_consumer, new_consumer_source, "TNewThing", 0),
            );
            assert!(result_locations(server.response(&still_negative_id)).is_empty());
        } else {
            let old_after_error_id =
                RequestId::from("worker-error-old-disk-identity-after-fallback".to_string());
            server.send_request(
                old_after_error_id.clone(),
                "textDocument/definition",
                navigation_params(&old_consumer, old_consumer_source, "TOldThing", 0),
            );
            assert!(result_locations(server.response(&old_after_error_id)).is_empty());
            let new_after_error_id =
                RequestId::from("worker-error-new-disk-identity-after-fallback".to_string());
            server.send_request(
                new_after_error_id.clone(),
                "textDocument/definition",
                navigation_params(&new_consumer, new_consumer_source, "TNewThing", 0),
            );
            let locations = result_locations(server.response(&new_after_error_id));
            assert_eq!(locations.len(), 1);
            assert_eq!(locations[0]["uri"], uri(&provider).to_string());
        }

        // A later valid event after closing the overlay proves recovery can
        // resume from fresh disk state rather than remaining permanently fenced.
        if !pull_diagnostics {
            server.send_notification(
                "textDocument/didClose",
                json!({"textDocument":{"uri":uri(&provider)}}),
            );
        }
        server.send_notification(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":uri(&provider),"type":2}]}),
        );
        if pull_diagnostics {
            let refresh = server
                .request_with_timeout("workspace/diagnostic/refresh", IO_TIMEOUT)
                .expect("repair event must refresh pull diagnostics");
            server.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
        }
        let old_after_repair_id =
            RequestId::from(format!("worker-error-old-after-repair-{pull_diagnostics}"));
        server.send_request(
            old_after_repair_id.clone(),
            "textDocument/definition",
            navigation_params(&old_consumer, old_consumer_source, "TOldThing", 0),
        );
        assert!(result_locations(server.response(&old_after_repair_id)).is_empty());
        let new_after_repair_id =
            RequestId::from(format!("worker-error-new-after-repair-{pull_diagnostics}"));
        server.send_request(
            new_after_repair_id.clone(),
            "textDocument/definition",
            navigation_params(&new_consumer, new_consumer_source, "TNewThing", 0),
        );
        let new_locations = result_locations(server.response(&new_after_repair_id));
        assert_eq!(new_locations.len(), 1);
        assert_eq!(new_locations[0]["uri"], uri(&provider).to_string());
        server.shutdown();
    }
}

#[test]
#[cfg(feature = "test-support")]
fn malformed_file_batch_with_unretainable_endpoint_discards_without_fencing() {
    let environment = tempfile::tempdir().expect("temporary workspace");
    let root = environment.path().to_path_buf();
    let provider = root.join("Provider.pas");
    let consumer = root.join("Consumer.pas");
    let provider_source = "unit Provider;\ninterface\nconst badConst = 1;\ntype TThing = class end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\ntype TAlias = Provider.TThing;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    let (mut server, writer_barrier) = TestServer::launch_with_outbound_writer_barrier(environment);
    server.initialize_with_watched_registration(&root, Value::Null, false);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":1,"text":provider_source}}),
    );
    let initial = server
        .diagnostic_with_timeout(&uri(&provider), IO_TIMEOUT)
        .expect("open provider should publish before the malformed batch");
    assert!(!initial["diagnostics"].as_array().unwrap().is_empty());

    let mut overlong_endpoint = root.clone();
    for index in 0..24 {
        overlong_endpoint.push(format!("endpoint-{index}-{}", "z".repeat(190)));
    }
    overlong_endpoint.push("Provider.pas");
    let overlong_uri = uri(&overlong_endpoint);
    assert!(overlong_uri.as_str().len() > 4096);
    fs::write(&writer_barrier.armed, b"pause").expect("arm slow writer");
    server.send_notification(
        "workspace/didDeleteFiles",
        json!({"files":[{"uri":overlong_uri},{"badEntry":"unattributable"}]}),
    );
    writer_barrier.wait_until_entered();
    writer_barrier.release();
    let clear = server
        .diagnostic_with_timeout(&uri(&provider), Duration::from_secs(2))
        .expect("unretainable endpoint recovery must deliver a bounded push clear");
    assert!(clear["diagnostics"].as_array().is_some_and(Vec::is_empty));
    let definition_id = RequestId::from("unretainable-endpoint-analysis-answers".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TThing", 0),
    );
    let locations = result_locations(server.response(&definition_id));
    assert_eq!(
        locations.len(),
        1,
        "analysis answers from disk and overlays"
    );
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
#[cfg(target_os = "linux")]
fn mixed_malformed_rename_rejects_moved_overlay_for_queued_and_inflight_queries() {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace directory");
    let provider = root.join("Provider.pas");
    let renamed_provider = root.join("Renamed.pas");
    let consumer = root.join("Consumer.pas");
    let noise = root.join("Noise.pas");
    let other_noise = root.join("OtherNoise.pas");
    let provider_source =
        "unit Provider;\ninterface\ntype TOldThing = class end;\nimplementation\nend.\n";
    let renamed_source =
        "unit Provider;\ninterface\ntype TNewThing = class end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\ntype TAlias = Provider.TOldThing;\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&consumer, consumer_source);
    write_file(&noise, "unit Noise; interface end.\n");
    write_file(&other_noise, "unit OtherNoise; interface end.\n");

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":1,"text":provider_source}}),
    );
    let inflight_id = RequestId::from("unattributable-rename-inflight-before".to_string());
    server.send_request(
        inflight_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOldThing", 0),
    );
    barrier.wait_until_entered();

    fs::rename(&provider, &renamed_provider).expect("client-owned provider move");
    write_file(&renamed_provider, renamed_source);
    server.send_notification(
        "workspace/didRenameFiles",
        json!({"files":[
            {"oldUri":uri(&noise),"newUri":uri(&other_noise)},
            {"oldUri":"not-a-uri","newUri":uri(&renamed_provider)}
        ]}),
    );
    let queued_id = RequestId::from("unattributable-rename-queued-after".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOldThing", 0),
    );
    barrier.release();
    // TASK-97: the moved provider overlay is rejected (its file is gone).
    let queued_response = server.response(&queued_id);
    assert!(
        queued_response.error.is_none(),
        "queued query must answer: {queued_response:?}"
    );
    assert!(
        result_locations(queued_response).is_empty(),
        "queued post-transition work must not see the rejected overlay"
    );
    let inflight_response = server.response(&inflight_id);
    assert!(
        inflight_response.error.is_some() || result_locations(inflight_response.clone()).is_empty(),
        "pre-discard in-flight work must not deliver a stale definition: {inflight_response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
#[cfg(target_os = "linux")]
fn uri_byte_overflow_rename_rejects_inflight_and_queued_queries() {
    let environment = tempfile::tempdir().expect("test environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace directory");
    let provider = root.join("Provider.pas");
    let renamed_provider = root.join("Renamed.pas");
    let consumer = root.join("Consumer.pas");
    let old_source =
        "unit Provider;\ninterface\ntype TOldThing = class end;\nimplementation\nend.\n";
    let new_source =
        "unit Provider;\ninterface\ntype TNewThing = class end;\nimplementation\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\ntype TAlias = Provider.TOldThing;\nimplementation\nend.\n";
    assert_eq!(old_source.len(), new_source.len());
    write_file(&provider, old_source);
    write_file(&consumer, consumer_source);

    let barrier_directory = environment.path().join("uri-overflow-barriers");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let navigation = TestBarrier {
        entered: barrier_directory.join("navigation-entered"),
        release: barrier_directory.join("navigation-release"),
    };
    let navigation_completed = TestBarrier {
        entered: barrier_directory.join("navigation-completed"),
        release: barrier_directory.join("navigation-completion-release"),
    };
    let reconciliation = TestBarrier {
        entered: barrier_directory.join("reconciliation-entered"),
        release: barrier_directory.join("reconciliation-release"),
    };
    let fifo_state = barrier_directory.join("fifo-state.json");
    let reader_pending = barrier_directory.join("reader-pending.json");
    let query_metrics = barrier_directory.join("navigation-work.json");
    let navigation_spec = format!(
        "{}|{}",
        navigation.entered.display(),
        navigation.release.display()
    );
    let navigation_completed_spec = format!(
        "{}|{}",
        navigation_completed.entered.display(),
        navigation_completed.release.display()
    );
    let reconciliation_spec = format!(
        "{}|{}",
        reconciliation.entered.display(),
        reconciliation.release.display()
    );
    let fifo_probe_spec = format!(
        "{}|{}|{}|{}",
        fifo_state.display(),
        reader_pending.display(),
        reconciliation.entered.display(),
        reconciliation.release.display()
    );
    let query_metrics_spec = query_metrics.display().to_string();
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        environment.path(),
        [
            (
                "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
                navigation_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_PARTIAL_VALIDATION_BARRIER",
                navigation_completed_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_FILE_WORKER_BARRIER",
                reconciliation_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_WORKSPACE_FIFO_PROBE",
                fifo_probe_spec.as_str(),
            ),
            (
                "PASCAL_LSP_TEST_QUERY_RECONCILIATION_WORK_RESULT",
                query_metrics_spec.as_str(),
            ),
        ],
    );
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":1,"text":old_source}}),
    );
    let inflight_id = RequestId::from("oversized-unplanned-rename-inflight".to_string());
    server.send_request(
        inflight_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOldThing", 0),
    );
    navigation.wait_until_entered();

    let metadata = fs::metadata(&provider).expect("old provider metadata");
    fs::rename(&provider, &renamed_provider).expect("client-owned move without will plan");
    write_file(&renamed_provider, new_source);
    restore_mtime(&renamed_provider, &metadata);
    let component = "q".repeat(120);
    let mut files: Vec<Value> = (0..62)
        .map(|index| {
            let mut path = root.join(format!("noise{index}"));
            for _ in 0..5 {
                path.push(&component);
            }
            json!({"oldUri":uri(&path.join("Old.pas")),"newUri":uri(&path.join("New.pas"))})
        })
        .collect();
    files.push(json!({"oldUri":uri(&provider),"newUri":uri(&renamed_provider)}));
    assert_eq!(files.len(), 63);
    let uri_bytes: usize = files
        .iter()
        .map(|pair| pair["oldUri"].as_str().unwrap().len() + pair["newUri"].as_str().unwrap().len())
        .sum();
    assert!(uri_bytes > 32 * 1024);
    server.send_notification("workspace/didRenameFiles", json!({"files":files}));
    reconciliation.wait_until_entered();

    let queued_id = RequestId::from("oversized-unplanned-rename-queued".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/definition",
        navigation_params(&consumer, consumer_source, "TOldThing", 0),
    );
    assert!(
        wait_until_json_bool(&fifo_state, "worker_held", true, IO_TIMEOUT),
        "the post-rename query must enter the deferred workspace FIFO while its worker is held"
    );
    let fifo: Value = serde_json::from_slice(&fs::read(&fifo_state).expect("workspace FIFO state"))
        .expect("valid workspace FIFO state");
    assert_eq!(fifo["worker_held"], true, "{fifo}");
    assert!(
        fifo["queued_count"].as_u64().unwrap_or_default() >= 1,
        "queued query must be admitted before navigation resumes: {fifo}"
    );

    navigation.release();
    navigation_completed.wait_until_entered();
    assert!(
        wait_for_file(&query_metrics, IO_TIMEOUT),
        "navigation computation must complete before the file worker is released"
    );
    navigation_completed.release();
    assert!(
        server
            .response_with_timeout(&inflight_id, IO_TIMEOUT)
            .is_none(),
        "unsafe rename must hold the completed in-flight response until reconciliation commits"
    );

    reconciliation.release();
    // TASK-97: the moved overlay at the known old endpoint is rejected; the
    // queued query answers without it instead of hitting a fence.
    let queued = server.response(&queued_id);
    assert!(queued.error.is_none(), "{queued:?}");
    assert!(
        result_locations(queued).is_empty(),
        "queued query must not see the rejected old provider overlay"
    );
    let inflight = server.response(&inflight_id);
    assert!(
        inflight.error.is_some() || result_locations(inflight.clone()).is_empty(),
        "in-flight query must not deliver a stale old-provider definition: {inflight:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn code_lens_queued_resolution_cancellation_returns_no_locations() {
    let environment = tempfile::tempdir().expect("server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    write_file(
        &provider,
        "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n",
    );
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let discover_id = RequestId::from("code-lens-cancel-discovery".to_string());
    server.send_request(
        discover_id.clone(),
        "textDocument/codeLens",
        json!({"textDocument": {"uri": uri(&provider)}}),
    );
    let response = server.response(&discover_id);
    let lens = response
        .result
        .expect("discovered lenses")
        .as_array()
        .expect("lens array")[0]
        .clone();
    for number in 0..3 {
        let blocker = root.join(format!("Blocker{number}.pas"));
        write_file(
            &blocker,
            &format!("unit Blocker{number};\ninterface\nimplementation\nend.\n"),
        );
        server.send_request(
            RequestId::from(format!("code-lens-blocker-{number}")),
            "textDocument/definition",
            json!({"textDocument": {"uri": uri(&blocker)}, "position": {"line": 0, "character": 5}}),
        );
    }
    barrier.wait_for_entries(3);
    let resolve_id = RequestId::from("code-lens-cancel-resolve".to_string());
    server.send_request(resolve_id.clone(), "codeLens/resolve", lens);
    server.send_notification("$/cancelRequest", json!({"id": resolve_id}));
    let cancelled = server.response(&resolve_id);
    assert_eq!(
        cancelled.error.expect("queued resolve cancellation").code,
        -32800
    );
    barrier.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn code_lens_computed_discovery_rejects_late_overlay() {
    let environment = tempfile::tempdir().expect("server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    write_file(&provider, source);
    let (mut server, barrier) = TestServer::launch_with_partial_validation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&provider), "languageId": "pascal", "version": 1, "text": source}}),
    );
    let id = RequestId::from("code-lens-queued-discovery".to_string());
    server.send_request(
        id.clone(),
        "textDocument/codeLens",
        json!({"textDocument": {"uri": uri(&provider)}}),
    );
    barrier.wait_for_entries(1);
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&provider), "version": 2}, "contentChanges": [{"text": source.replace("PublicRoutine", "ChangedRoutine") }]}),
    );
    let fence_id = RequestId::from("code-lens-queued-overlay-fence".to_string());
    server.send_request(
        fence_id.clone(),
        "workspace/willCreateFiles",
        json!({"files": []}),
    );
    assert!(server.response(&fence_id).error.is_none());
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.error.is_some()
            || response
                .result
                .as_ref()
                .is_some_and(|value| value.as_array().is_some_and(Vec::is_empty)),
        "old source lenses escaped delivery freshness: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn change_signature_queued_source_edit_rejects_old_call_set() {
    let environment = tempfile::tempdir().expect("server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Sample.pas");
    let source = "unit Sample;\ninterface\nimplementation\nprocedure Run;\n  procedure Pair(A: Integer; B: Integer);\n  begin\n  end;\nbegin\n  Pair(1, 2);\nend;\nend.\n";
    write_file(&main, source);
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    for number in 0..3 {
        let blocker = root.join(format!("Blocker{number}.pas"));
        write_file(
            &blocker,
            &format!("unit Blocker{number};\ninterface\nimplementation\nend.\n"),
        );
        server.send_request(
            RequestId::from(format!("signature-blocker-{number}")),
            "textDocument/definition",
            json!({"textDocument": {"uri": uri(&blocker)}, "position": {"line": 0, "character": 5}}),
        );
    }
    barrier.wait_for_entries(3);
    let id = RequestId::from("queued-change-signature".to_string());
    server.send_request(
        id.clone(),
        "textDocument/codeAction",
        json!({"textDocument": {"uri": uri(&main)}, "range": {"start": {"line": 4, "character": 12}, "end": {"line": 4, "character": 16}}, "context": {"diagnostics": [], "only": ["refactor.rewrite.changeSignature"]}}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&main), "version": 2}, "contentChanges": [{"text": source.replace("Pair(1, 2)", "Pair(Next(), 2)")}]}),
    );
    let fence_id = RequestId::from("signature-queued-overlay-fence".to_string());
    server.send_request(
        fence_id.clone(),
        "workspace/willCreateFiles",
        json!({"files": []}),
    );
    assert!(server.response(&fence_id).error.is_none());
    barrier.release();
    let response = server.response(&id);
    assert!(
        response.error.is_some()
            || response
                .result
                .as_ref()
                .is_some_and(|value| value.as_array().is_some_and(Vec::is_empty)),
        "stale signature edit published: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn extract_queued_source_change_cannot_publish_old_edit() {
    let environment = tempfile::tempdir().expect("server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Sample.pas");
    let source = "unit Sample;\ninterface\nimplementation\nprocedure Run;\nvar Target: Integer;\nbegin\n  Target := 42;\nend;\nend.\n";
    write_file(&main, source);
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": source}}),
    );
    for number in 0..3 {
        let blocker = root.join(format!("Blocker{number}.pas"));
        write_file(
            &blocker,
            &format!("unit Blocker{number};\ninterface\nimplementation\nend.\n"),
        );
        server.send_request(
            RequestId::from(format!("extract-blocker-{number}")),
            "textDocument/definition",
            json!({"textDocument": {"uri": uri(&blocker)}, "position": {"line": 0, "character": 5}}),
        );
    }
    barrier.wait_for_entries(3);
    let extraction_id = RequestId::from("queued-extract-variable".to_string());
    server.send_request(
        extraction_id.clone(),
        "textDocument/codeAction",
        json!({"textDocument": {"uri": uri(&main)}, "range": {"start": {"line": 6, "character": 12}, "end": {"line": 6, "character": 14}}, "context": {"diagnostics": [], "only": ["refactor.extract.variable"]}}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri(&main), "version": 2}, "contentChanges": [{"text": source.replace("42", "Compute()") }]}),
    );
    let fence_id = RequestId::from("extract-queued-overlay-fence".to_string());
    server.send_request(
        fence_id.clone(),
        "workspace/willCreateFiles",
        json!({"files": []}),
    );
    assert!(server.response(&fence_id).error.is_none());
    barrier.release();
    let response = server.response(&extraction_id);
    assert!(
        response.error.is_some()
            || response
                .result
                .as_ref()
                .is_some_and(|value| value.as_array().is_some_and(Vec::is_empty)),
        "stale extraction published an edit: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn blocked_navigation_does_not_block_unrelated_lsp_requests() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let navigation_id = RequestId::from("blocked-navigation".to_string());
    server.send_request(
        navigation_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    let symbols_id = RequestId::from("while-navigation-is-blocked".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "unrelated request was blocked by navigation: {symbols:?}"
    );
    assert!(
        symbols.result.is_some(),
        "document symbols must be returned"
    );

    barrier.release();
    let locations = result_locations(server.response(&navigation_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_open_document_change_does_not_discard_blocked_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let unrelated = root.join("Unrelated.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let unrelated_source = "unit Unrelated;\ninterface\nimplementation\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);
    write_file(&unrelated, unrelated_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("unrelated-open-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated),
                "languageId": "pascal",
                "version": 1,
                "text": unrelated_source
            }
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&unrelated), "version": 2},
            "contentChanges": [{"text": format!("{unrelated_source}{{$IFDEF UNRELATED}}\n") }]
        }),
    );

    barrier.release();
    let locations = result_locations(server.response(&request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn requested_open_document_change_discards_blocked_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    let second_source =
        first_source.replace("procedure Run;", "procedure Changed;\nprocedure Run;");
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );

    let request_id = RequestId::from("requested-open-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, first_source, "Run", 0),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("requested source change must stale the result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn changed_imported_provider_discards_blocked_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let changed_provider_source = provider_source.replace("PublicRoutine", "ChangedRoutine");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("changed-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    write_file(&provider, &changed_provider_source);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&provider), "type": 2}]}),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("changed provider must stale the result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn absent_provider_overlay_invalidates_blocked_empty_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("AbsentProvider.pas");
    let main_source = "unit Main;\ninterface\nuses AbsentProvider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let provider_source = "unit AbsentProvider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("absent-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("fulfilled negative provider lookup must stale the old result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );

    let fresh_request_id = RequestId::from("absent-provider-navigation-fresh".to_string());
    server.send_request(
        fresh_request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    let locations = result_locations(server.response(&fresh_request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn nested_absent_provider_overlay_invalidates_blocked_empty_navigation_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let nested_root = root.join("nested");
    let main = root.join("Main.pas");
    let provider = nested_root.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let provider_source = "unit ReviewTask12Provider;\ninterface\nprocedure ReviewTask12Routine;\nimplementation\nprocedure ReviewTask12Routine;\nbegin\nend;\nend.\n";
    fs::create_dir_all(&nested_root).expect("existing nested source root");
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("nested-absent-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("nested fulfilled negative provider lookup must stale the old result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );

    let fresh_request_id = RequestId::from("nested-absent-provider-navigation-fresh".to_string());
    server.send_request(
        fresh_request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    let locations = result_locations(server.response(&fresh_request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn incomplete_filename_catalogue_invalidates_absent_nested_provider_overlay() {
    const CATALOGUE_LIMIT: usize = 8;

    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let existing_directory = root.join("crates");
    let main = root.join("Main.pas");
    let provider = existing_directory.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let provider_source = "unit ReviewTask12Provider;\ninterface\nprocedure ReviewTask12Routine;\nimplementation\nprocedure ReviewTask12Routine;\nbegin\nend;\nend.\n";
    fs::create_dir_all(&existing_directory).expect("existing source directory");
    write_file(&main, main_source);
    for index in 0..CATALOGUE_LIMIT {
        fs::write(
            existing_directory.join(format!("CataloguePadding{index:05}.txt")),
            [],
        )
        .expect("catalogue padding file");
    }

    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier_and_filename_catalogue_limit(
            environment,
            CATALOGUE_LIMIT,
        );
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("incomplete-catalogue-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&provider),
                "languageId": "pascal",
                "version": 1,
                "text": provider_source
            }
        }),
    );

    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("incomplete catalogue provider overlay must stale the old result");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );

    let fresh_request_id =
        RequestId::from("incomplete-catalogue-provider-navigation-fresh".to_string());
    server.send_request(
        fresh_request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    let locations = result_locations(server.response(&fresh_request_id));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_nested_and_outside_provider_changes_do_not_stale_blocked_navigation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let nested_root = root.join("nested");
    let outside_root = environment.path().join("outside");
    let main = root.join("Main.pas");
    let unrelated_nested = nested_root.join("DifferentProvider.pas");
    let outside_provider = outside_root.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let unrelated_source = "unit DifferentProvider;\ninterface\nimplementation\nend.\n";
    fs::create_dir_all(&nested_root).expect("existing nested source root");
    fs::create_dir_all(&outside_root).expect("existing outside source root");
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let request_id = RequestId::from("unrelated-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated_nested),
                "languageId": "pascal",
                "version": 1,
                "text": unrelated_source
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&outside_provider), "type": 1}]}),
    );
    let unrelated_ack = root.join("UnrelatedAck.pas");
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&unrelated_ack),
                "languageId": "pascal",
                "version": 1,
                "text": "unit UnrelatedAck;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unrelated provider changes must not stale the result: {response:?}"
    );
    assert!(result_locations(response).is_empty());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn excluded_nested_provider_change_does_not_stale_blocked_navigation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let excluded_root = root.join("excluded");
    let main = root.join("Main.pas");
    let provider = excluded_root.join("ReviewTask12Provider.pas");
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    fs::create_dir_all(&excluded_root).expect("existing excluded source root");
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, json!({"exclude": ["excluded/**"]}));

    let request_id = RequestId::from("excluded-provider-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ReviewTask12Routine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&provider), "type": 1}]}),
    );
    let excluded_ack = root.join("ExcludedAck.pas");
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&excluded_ack),
                "languageId": "pascal",
                "version": 1,
                "text": "unit ExcludedAck;\ninterface\nimplementation\nend.\n"
            }
        }),
    );
    let _ = server.notification("textDocument/publishDiagnostics");

    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "excluded provider changes must not stale the result: {response:?}"
    );
    assert!(result_locations(response).is_empty());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn native_case_distinct_provider_changes_preserve_stale_invalidation_across_event_orders() {
    let main_source = "unit Main;\ninterface\nuses ReviewTask12Provider;\nimplementation\nprocedure Run;\nbegin\n  ReviewTask12Routine;\nend;\nend.\n";
    let provider_source = "unit ReviewTask12Provider;\ninterface\nprocedure ReviewTask12Routine;\nimplementation\nprocedure ReviewTask12Routine;\nbegin\nend;\nend.\n";

    for (label, allowed_first) in [("allowed-first", true), ("excluded-first", false)] {
        let environment = tempfile::tempdir().expect("isolated server environment");
        let root = environment.path().join("workspace");
        let nested_root = root.join("nested");
        let main = root.join("Main.pas");
        let allowed_provider = nested_root.join("ReviewTask12Provider.pas");
        let excluded_provider = nested_root.join("REVIEWTASK12PROVIDER.pas");
        fs::create_dir_all(&nested_root).expect("existing nested source root");
        write_file(&main, main_source);

        let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
        server.initialize(
            &root,
            json!({
                "exclude": ["nested/REVIEWTASK12PROVIDER.pas"]
            }),
        );

        let request_id = RequestId::from(format!("case-distinct-provider-{label}"));
        server.send_request(
            request_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "ReviewTask12Routine", 0),
        );
        barrier.wait_until_entered();

        if allowed_first {
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&allowed_provider),
                        "languageId": "pascal",
                        "version": 1,
                        "text": provider_source
                    }
                }),
            );
            let _ = server.notification("textDocument/publishDiagnostics");
            server.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": [{"uri": uri(&excluded_provider), "type": 1}]
                }),
            );
        } else {
            server.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": [{"uri": uri(&excluded_provider), "type": 1}]
                }),
            );
            server.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri(&allowed_provider),
                        "languageId": "pascal",
                        "version": 1,
                        "text": provider_source
                    }
                }),
            );
            let _ = server.notification("textDocument/publishDiagnostics");
        }

        let acknowledgement = root.join(format!("{label}-ack.pas"));
        server.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri(&acknowledgement),
                    "languageId": "pascal",
                    "version": 1,
                    "text": "unit Acknowledgement;\ninterface\nimplementation\nend.\n"
                }
            }),
        );
        let _ = server.notification("textDocument/publishDiagnostics");

        barrier.release();
        let response = server.response(&request_id);
        let error = match response.error {
            Some(error) => error,
            None => panic!("{label}: native allowed change was lost: {response:?}"),
        };
        assert_eq!(error.code, -32803, "{label}");
        assert_eq!(
            error.message, "analysis result became stale; retry the request",
            "{label}"
        );

        let fresh_request_id = RequestId::from(format!("case-distinct-provider-{label}-fresh"));
        server.send_request(
            fresh_request_id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "ReviewTask12Routine", 0),
        );
        let locations = result_locations(server.response(&fresh_request_id));
        assert_eq!(locations.len(), 1, "{label}");
        assert_eq!(
            locations[0]["uri"],
            uri(&allowed_provider).to_string(),
            "{label}"
        );
        server.shutdown();
    }
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_configuration_change_does_not_discard_blocked_formatting_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let unrelated = root.join("unrelated");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    let unrelated_config = unrelated.join(".lint4d.toml");
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("unrelated-configuration-formatting".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();

    write_file(&unrelated_config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&unrelated_config), "type": 1}]}),
    );

    barrier.release();
    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "unrelated configuration must not stale formatting: {response:?}"
    );
    assert!(response.result.is_some());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn blocked_formatting_does_not_block_unrelated_lsp_requests() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);

    let formatting_id = RequestId::from("blocked-formatting".to_string());
    server.send_request(
        formatting_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();

    let symbols_id = RequestId::from("while-formatting-is-blocked".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "unrelated request was blocked by formatting: {symbols:?}"
    );
    assert!(
        symbols.result.is_some(),
        "document symbols must be returned"
    );

    barrier.release();
    let formatting = server.response(&formatting_id);
    assert!(
        formatting.error.is_none(),
        "formatting request failed: {formatting:?}"
    );
    assert!(
        formatting.result.is_some(),
        "formatting result must be returned"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn diagnostics_drop_a_stale_blocked_result_after_a_newer_document_version() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let changed_start = position_of(first_source, "badConst", 0);
    let changed_end = position_after(first_source, "badConst", 0);
    write_file(&main, first_source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{
                "range": {"start": changed_start, "end": changed_end},
                "rangeLength": 8,
                "text": "GoodConst"
            }]
        }),
    );
    barrier.release();

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(diagnostics["version"], 2);
    assert!(
        diagnostics["diagnostics"].as_array().unwrap().is_empty(),
        "stale diagnostics from version 1 were published: {diagnostics}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn type_diagnostics_drop_a_stale_blocked_result_after_a_newer_document_version() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = concat!(
        "unit Main;\n",
        "interface\n",
        "implementation\n",
        "procedure Run;\n",
        "var I: Integer; B: Boolean;\n",
        "begin\n",
        "  B := I;\n",
        "end;\n",
        "end.\n",
    );
    let changed_source = first_source.replace("B := I", "B := True");
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source,
            }
        }),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": changed_source}],
        }),
    );
    barrier.release();

    let publication = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(publication["version"], 2);
    assert!(
        publication["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .all(|diagnostic| diagnostic["code"] != "pascal-type-mismatch"),
        "stale type diagnostics from version 1 were published: {publication}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn full_analysis_queue_returns_explicit_overflow_without_starting_an_extra_worker() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let first_id = RequestId::from("full-queue-first".to_string());
    let second_id = RequestId::from("full-queue-second".to_string());
    server.send_request(
        first_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        second_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let queued_ids = (0..31)
        .map(|index| RequestId::from(format!("full-queue-{index}")))
        .collect::<Vec<_>>();
    for (index, id) in queued_ids.iter().cloned().enumerate() {
        server.send_request(
            id,
            "workspace/symbol",
            json!({"query": format!("MissingSymbol{index}")}),
        );
    }
    let overflow_id = RequestId::from("full-queue-overflow".to_string());
    server.send_request(
        overflow_id.clone(),
        "workspace/symbol",
        json!({"query": "OverflowSymbol"}),
    );
    let overflow = server.response(&overflow_id);
    let error = overflow
        .error
        .expect("full queue must reject the overflow request");
    assert_eq!(error.code, -32803);
    assert_eq!(error.message, "analysis queue is full; retry the request");

    barrier.release();
    let first_locations = result_locations(server.response(&first_id));
    let second_locations = result_locations(server.response(&second_id));
    assert_eq!(first_locations.len(), 1);
    assert_eq!(second_locations.len(), 1);
    for id in queued_ids {
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "queued request failed: {response:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn diagnostics_use_reserved_capacity_when_the_client_queue_is_full() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, navigation, diagnostics, dispatch) =
        TestServer::launch_with_navigation_and_diagnostics_barriers_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);
    for (id, occurrence) in [
        ("diagnostic-capacity-blocker-first", 0),
        ("diagnostic-capacity-blocker-second", 1),
    ] {
        server.send_request(
            RequestId::from(id.to_string()),
            "textDocument/definition",
            navigation_params(&main, source, "Run", occurrence),
        );
    }
    navigation.wait_for_entries(2);
    navigation.occupy_interactive_slot(&mut server, &main);

    let queued_ids = (0..31)
        .map(|index| RequestId::from(format!("diagnostic-capacity-client-{index}")))
        .collect::<Vec<_>>();
    for (index, id) in queued_ids.iter().cloned().enumerate() {
        server.send_request(
            id,
            "workspace/symbol",
            json!({"query": format!("DiagnosticCapacitySymbol{index}")}),
        );
    }
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    thread::sleep(Duration::from_millis(350));

    navigation.release();
    let dispatches = dispatch.wait_for_entries(4);
    assert_eq!(
        &dispatches[..3],
        b"III",
        "the three blocking requests must dispatch first"
    );
    assert_eq!(
        dispatches[3], b'D',
        "diagnostics must occupy the reserved queue slot before client work"
    );
    diagnostics.wait_until_entered();
    diagnostics.release();

    let published = server
        .diagnostic_with_timeout(&uri(&main), IO_TIMEOUT)
        .expect("diagnostics must progress from reserved queue capacity");
    assert_eq!(published["version"], 1);
    assert!(
        published["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming"),
        "expected the configured naming diagnostic: {published}"
    );
    for id in queued_ids {
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "reserved-capacity client request failed: {response:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn reusing_a_cancelled_request_id_keeps_attached_work_and_response_ids_exact() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, navigation, formatting, dispatch) =
        TestServer::launch_with_navigation_and_formatting_barriers_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);

    let reused_id = RequestId::from("reused-request-id".to_string());
    let attached_id = RequestId::from("attached-request-id".to_string());
    server.send_request(
        reused_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    navigation.wait_until_entered();
    server.send_request(
        attached_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    assert!(
        server
            .response_with_timeout(&attached_id, Duration::from_millis(100))
            .is_none(),
        "an attached request must wait for the shared worker"
    );

    server.send_notification("$/cancelRequest", json!({"id": reused_id.clone()}));
    let cancelled = server.response(&reused_id);
    assert_eq!(cancelled.id, reused_id);
    assert_eq!(cancelled.error.expect("cancellation error").code, -32800);

    // Reuse the canceled ID for a distinct client-backed computation while
    // the old computation remains alive for the attached request.
    server.send_request(
        reused_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    formatting.wait_until_entered();
    // One navigation worker is already blocked, so this fills the remaining
    // slot alongside the formatting worker.
    server.send_request(
        RequestId::from("id-reuse-extra-blocker".to_string()),
        "textDocument/declaration",
        navigation_params(&main, main_source, "Run", 0),
    );
    navigation.wait_for_entries(2);

    let queued_id = RequestId::from("request-after-id-reuse".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(
        server
            .response_with_timeout(&queued_id, Duration::from_millis(100))
            .is_none(),
        "a fourth computation must remain queued while three workers are active"
    );
    assert_eq!(
        fs::read(&dispatch.path).expect("dispatch log before release"),
        b"IBI",
        "request ID reuse must not dispatch an extra worker"
    );

    // Complete the reused-ID computation first. The attached navigation must
    // still be retained and must not be mistaken for this result.
    formatting.release();
    let formatting_response = server.response(&reused_id);
    assert_eq!(formatting_response.id, reused_id);
    assert!(
        formatting_response.error.is_none(),
        "reused request ID formatting failed: {formatting_response:?}"
    );
    let queued_response = server.response(&queued_id);
    assert_eq!(queued_response.id, queued_id);
    assert!(
        queued_response.error.is_none(),
        "request after ID reuse failed: {queued_response:?}"
    );

    navigation.release();
    let attached_response = server
        .response_with_timeout(&attached_id, Duration::from_secs(1))
        .expect("attached request must receive the old computation result");
    assert_eq!(attached_response.id, attached_id);
    let locations = result_locations(attached_response);
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], uri(&provider).to_string());
    server.assert_no_response(&attached_id);
    server.assert_no_response(&reused_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn running_shared_client_recipients_are_bounded_and_cancellation_frees_capacity() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let blocker_id = RequestId::from("running-recipient-blocker".to_string());
    server.send_request(
        blocker_id.clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    let primary_id = RequestId::from("running-recipient-primary".to_string());
    server.send_request(
        primary_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let attachments = (0..31)
        .map(|index| RequestId::from(format!("running-recipient-attachment-{index}")))
        .collect::<Vec<_>>();
    for id in &attachments {
        server.send_request(
            id.clone(),
            "textDocument/definition",
            navigation_params(&main, main_source, "Run", 0),
        );
    }
    let overflow_same = RequestId::from("running-recipient-overflow-same".to_string());
    server.send_request(
        overflow_same.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_same, Duration::from_secs(1))
            .expect("same-query recipient overflow response"),
    );

    let overflow_distinct = RequestId::from("running-recipient-overflow-distinct".to_string());
    server.send_request(
        overflow_distinct.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_distinct, Duration::from_secs(1))
            .expect("distinct recipient overflow response"),
    );

    let cancelled_id = attachments[0].clone();
    server.send_notification("$/cancelRequest", json!({"id": cancelled_id.clone()}));
    let cancelled = server.response(&cancelled_id);
    assert_eq!(
        cancelled.error.expect("attachment cancellation").code,
        -32800
    );

    let replacement_id = RequestId::from("running-recipient-replacement".to_string());
    server.send_request(
        replacement_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );

    barrier.release();
    assert!(
        server.response(&blocker_id).error.is_none(),
        "running blocker failed"
    );
    assert!(
        server.response(&primary_id).error.is_none(),
        "running shared primary failed"
    );
    for id in attachments.into_iter().skip(1) {
        assert!(
            server.response(&id).error.is_none(),
            "running shared attachment failed: {id:?}"
        );
    }
    assert!(
        server.response(&replacement_id).error.is_none(),
        "replacement recipient failed"
    );
    server.assert_no_response(&cancelled_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_shared_client_recipients_are_bounded_and_cancellation_frees_capacity() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let blocker_ids = [
        RequestId::from("queued-recipient-blocker-provider".to_string()),
        RequestId::from("queued-recipient-blocker-main".to_string()),
    ];
    server.send_request(
        blocker_ids[0].clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    server.send_request(
        blocker_ids[1].clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let primary_id = RequestId::from("queued-recipient-primary".to_string());
    let target = navigation_params(&main, main_source, "Run", 0);
    server.send_request(
        primary_id.clone(),
        "textDocument/definition",
        target.clone(),
    );
    let attachments = (0..30)
        .map(|index| RequestId::from(format!("queued-recipient-attachment-{index}")))
        .collect::<Vec<_>>();
    for id in &attachments {
        server.send_request(id.clone(), "textDocument/definition", target.clone());
    }
    let overflow_same = RequestId::from("queued-recipient-overflow-same".to_string());
    server.send_request(
        overflow_same.clone(),
        "textDocument/definition",
        target.clone(),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_same, Duration::from_secs(1))
            .expect("queued same-query recipient overflow response"),
    );

    let overflow_distinct = RequestId::from("queued-recipient-overflow-distinct".to_string());
    server.send_request(
        overflow_distinct.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&overflow_distinct, Duration::from_secs(1))
            .expect("queued distinct recipient overflow response"),
    );

    let cancelled_id = attachments[0].clone();
    server.send_notification("$/cancelRequest", json!({"id": cancelled_id.clone()}));
    let cancelled = server.response(&cancelled_id);
    assert_eq!(
        cancelled
            .error
            .expect("queued attachment cancellation")
            .code,
        -32800
    );

    let replacement_id = RequestId::from("queued-recipient-replacement".to_string());
    server.send_request(replacement_id.clone(), "textDocument/definition", target);

    barrier.release();
    for id in blocker_ids {
        assert!(
            server.response(&id).error.is_none(),
            "queued blocker failed: {id:?}"
        );
    }
    assert!(
        server.response(&primary_id).error.is_none(),
        "queued shared primary failed"
    );
    for id in attachments.into_iter().skip(1) {
        assert!(
            server.response(&id).error.is_none(),
            "queued shared attachment failed: {id:?}"
        );
    }
    assert!(
        server.response(&replacement_id).error.is_none(),
        "queued replacement recipient failed"
    );
    server.assert_no_response(&cancelled_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn full_client_queue_rejects_coalesced_recipients_without_unbounded_shutdown_responses() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);

    let blocker_ids = [
        RequestId::from("full-recipient-blocker-provider".to_string()),
        RequestId::from("full-recipient-blocker-main".to_string()),
    ];
    server.send_request(
        blocker_ids[0].clone(),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    server.send_request(
        blocker_ids[1].clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let primary_id = RequestId::from("full-recipient-primary".to_string());
    let target = navigation_params(&main, main_source, "Run", 0);
    server.send_request(
        primary_id.clone(),
        "textDocument/definition",
        target.clone(),
    );
    let bulk_ids = (0..30)
        .map(|index| RequestId::from(format!("full-recipient-bulk-{index}")))
        .collect::<Vec<_>>();
    for (index, id) in bulk_ids.iter().cloned().enumerate() {
        server.send_request(
            id,
            "workspace/symbol",
            json!({"query": format!("FullRecipientMissing{index}")}),
        );
    }

    let attachment_ids = (0..1000)
        .map(|index| RequestId::from(format!("full-recipient-attachment-{index}")))
        .collect::<Vec<_>>();
    for id in &attachment_ids {
        server.send_request(id.clone(), "textDocument/definition", target.clone());
    }
    for id in attachment_ids {
        assert_queue_overflow(
            server
                .response_with_timeout(&id, Duration::from_secs(1))
                .expect("every excess coalesced recipient must be rejected"),
        );
    }

    let distinct_overflow = RequestId::from("full-recipient-distinct-overflow".to_string());
    server.send_request(
        distinct_overflow.clone(),
        "workspace/symbol",
        json!({"query": "FullRecipientOverflow"}),
    );
    assert_queue_overflow(
        server
            .response_with_timeout(&distinct_overflow, Duration::from_secs(1))
            .expect("full client queue distinct overflow response"),
    );

    barrier.release();
    for id in blocker_ids {
        assert!(
            server.response(&id).error.is_none(),
            "full client queue blocker failed: {id:?}"
        );
    }
    assert!(
        server.response(&primary_id).error.is_none(),
        "full client queue primary failed"
    );
    for id in bulk_ids {
        assert!(
            server.response(&id).error.is_none(),
            "full client queue bulk request failed: {id:?}"
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn interactive_worker_resolves_navigation_while_two_bulk_workers_are_blocked() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Run(aDatabase: Integer);\nbegin\n  aDatabase := 1;\nend;\nend.\n";
    let paths = [
        root.join("Main.pas"),
        root.join("Other.pas"),
        root.join("Queued.pas"),
    ];
    for path in &paths {
        write_file(path, source);
    }
    let (mut server, navigation, formatting, _) =
        TestServer::launch_with_navigation_and_formatting_barriers_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);
    for (index, path) in paths.iter().enumerate() {
        server.send_request(
            RequestId::from(format!("bulk-{index}")),
            "textDocument/formatting",
            json!({"textDocument": {"uri": uri(path)}, "options": {"tabSize": 2, "insertSpaces": true}}),
        );
    }
    formatting.wait_for_entries(2);
    navigation.release();
    let id = RequestId::from("interactive-navigation".to_string());
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(&paths[0], source, "aDatabase", 1),
    );
    let response = server
        .response_with_timeout(&id, IO_TIMEOUT)
        .expect("navigation must complete without releasing either bulk worker");
    let locations = result_locations(response);
    assert_eq!(locations.len(), 1);
    assert_eq!(
        locations[0]["range"]["start"],
        json!({"line": 3, "character": 14})
    );
    assert!(
        server
            .response_with_timeout(
                &RequestId::from("bulk-2".to_string()),
                Duration::from_millis(100)
            )
            .is_none(),
        "bulk work must not use the additional interactive slot"
    );
    assert_eq!(fs::read(&formatting.entered).unwrap().len(), 2);
    formatting.release();
    for index in 0..3 {
        assert!(
            server
                .response(&RequestId::from(format!("bulk-{index}")))
                .error
                .is_none()
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn interactive_worker_is_bounded_and_cancellation_releases_its_capacity() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Run(aDatabase: Integer);\nbegin\n  aDatabase := 1;\nend;\nend.\n";
    let paths = [root.join("Main.pas"), root.join("Other.pas")];
    for path in &paths {
        write_file(path, source);
    }
    let (mut server, navigation, formatting, dispatch) =
        TestServer::launch_with_navigation_and_formatting_barriers_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);
    for (index, path) in paths.iter().enumerate() {
        server.send_request(
            RequestId::from(format!("bulk-{index}")),
            "textDocument/formatting",
            json!({"textDocument": {"uri": uri(path)}, "options": {"tabSize": 2, "insertSpaces": true}}),
        );
    }
    formatting.wait_for_entries(2);
    let first = RequestId::from("interactive-first".to_string());
    server.send_request(
        first.clone(),
        "textDocument/definition",
        navigation_params(&paths[0], source, "aDatabase", 1),
    );
    navigation.wait_until_entered();
    let next = RequestId::from("interactive-next".to_string());
    server.send_request(
        next.clone(),
        "textDocument/hover",
        navigation_params(&paths[0], source, "aDatabase", 1),
    );
    assert!(
        server
            .response_with_timeout(&next, Duration::from_millis(100))
            .is_none(),
        "a fourth worker must not be spawned"
    );
    assert_eq!(fs::read(&dispatch.path).unwrap(), b"BBI");
    server.send_notification("$/cancelRequest", json!({"id": first}));
    assert_eq!(
        server
            .response(&first)
            .error
            .expect("cancelled navigation")
            .code,
        -32800
    );
    let response = server.response(&next);
    assert!(
        response.error.is_none(),
        "interactive capacity must be reusable after cancellation: {response:?}"
    );
    assert!(!response.result.unwrap().is_null());
    assert_eq!(fs::read(&formatting.entered).unwrap().len(), 2);
    formatting.release();
    for index in 0..2 {
        assert!(
            server
                .response(&RequestId::from(format!("bulk-{index}")))
                .error
                .is_none()
        );
    }
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn bounded_analysis_queue_prioritizes_interactive_work_over_bulk_work() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier, dispatch) =
        TestServer::launch_with_navigation_barrier_and_dispatch_log(environment);
    server.initialize(&root, Value::Null);

    let first_id = RequestId::from("priority-first".to_string());
    let second_id = RequestId::from("priority-second".to_string());
    server.send_request(
        first_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        second_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let bulk_id = RequestId::from("priority-bulk".to_string());
    server.send_request(
        bulk_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let interactive_id = RequestId::from("priority-interactive".to_string());
    server.send_request(
        interactive_id.clone(),
        "textDocument/hover",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    assert!(
        server
            .response_with_timeout(&bulk_id, Duration::from_millis(100))
            .is_none(),
        "bulk work must remain queued while all workers are occupied"
    );
    assert!(
        server
            .response_with_timeout(&interactive_id, Duration::from_millis(100))
            .is_none(),
        "interactive work must remain queued while all workers are occupied"
    );

    barrier.release();
    let dispatches = dispatch.wait_for_entries(5);
    assert!(
        dispatches.len() >= 5,
        "all queued requests must be dispatched: {dispatches:?}"
    );
    assert_eq!(
        &dispatches[..3],
        b"III",
        "the three blockers must dispatch first"
    );
    assert_eq!(dispatches[3], b'I', "interactive work must dispatch first");
    assert_eq!(
        dispatches[4], b'B',
        "bulk work must dispatch after interactive work"
    );
    let bulk_response = server.response(&bulk_id);
    let interactive_response = server.response(&interactive_id);
    assert!(
        bulk_response.error.is_none(),
        "bulk response failed: {bulk_response:?}"
    );
    assert!(
        interactive_response.error.is_none(),
        "interactive response failed: {interactive_response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_a_queued_request_removes_it_without_starting_a_worker() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_request(
        RequestId::from("queued-cancel-first".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        RequestId::from("queued-cancel-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let queued_id = RequestId::from("queued-cancelled".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(
        server
            .response_with_timeout(&queued_id, Duration::from_millis(100))
            .is_none(),
        "queued request must not respond before cancellation"
    );
    server.send_notification("$/cancelRequest", json!({"id": queued_id.clone()}));
    let cancelled = server
        .response_with_timeout(&queued_id, Duration::from_secs(1))
        .expect("queued cancellation response");
    assert_eq!(cancelled.error.expect("cancellation error").code, -32800);

    barrier.release();
    let _ = server.response(&RequestId::from("queued-cancel-first".to_string()));
    let _ = server.response(&RequestId::from("queued-cancel-second".to_string()));
    server.assert_no_response(&queued_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn identical_queued_observations_share_one_worker_but_distinct_positions_do_not() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut same_server, same_barrier) = TestServer::launch_with_navigation_barrier(environment);
    same_server.initialize(&root, Value::Null);
    same_server.send_request(
        RequestId::from("coalesce-first".to_string()),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    same_server.send_request(
        RequestId::from("coalesce-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    same_barrier.wait_for_entries(2);
    same_barrier.occupy_interactive_slot(&mut same_server, &main);
    let first = RequestId::from("coalesced-query-first".to_string());
    let second = RequestId::from("coalesced-query-second".to_string());
    for id in [first.clone(), second.clone()] {
        same_server.send_request(
            id,
            "textDocument/definition",
            navigation_params(&main, main_source, "PublicRoutine", 0),
        );
    }
    same_barrier.release();
    same_barrier.wait_for_entries(4);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read(&same_barrier.entered)
            .expect("coalesced barrier entries")
            .len(),
        4,
        "identical observations must share one dispatched worker"
    );
    let first_response = same_server.response(&first);
    let second_response = same_server.response(&second);
    assert!(
        first_response.error.is_none(),
        "first coalesced response failed"
    );
    assert!(
        second_response.error.is_none(),
        "second coalesced response failed"
    );
    same_server.assert_no_response(&first);
    same_server.assert_no_response(&second);
    same_server.shutdown();

    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    write_file(&provider, provider_source);
    write_file(&main, main_source);
    let (mut distinct_server, distinct_barrier) =
        TestServer::launch_with_navigation_barrier(environment);
    distinct_server.initialize(&root, Value::Null);
    distinct_server.send_request(
        RequestId::from("distinct-first".to_string()),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    distinct_server.send_request(
        RequestId::from("distinct-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Main", 0),
    );
    distinct_barrier.wait_for_entries(2);
    distinct_barrier.occupy_interactive_slot(&mut distinct_server, &main);
    let first = RequestId::from("distinct-position-first".to_string());
    let second = RequestId::from("distinct-position-second".to_string());
    distinct_server.send_request(
        first.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    distinct_server.send_request(
        second.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    distinct_barrier.release();
    distinct_barrier.wait_for_entries(5);
    assert!(
        distinct_server.response(&first).error.is_none(),
        "first distinct-position response failed"
    );
    assert!(
        distinct_server.response(&second).error.is_none(),
        "second distinct-position response failed"
    );
    distinct_server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_analysis_dispatches_against_the_latest_document_snapshot() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source = "unit Main;\ninterface\nprocedure OldThing;\nimplementation\nend.\n";
    let second_source = "unit Main;\ninterface\nprocedure NewThing;\nimplementation\nend.\n";
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );
    server.send_request(
        RequestId::from("latest-snapshot-first".to_string()),
        "textDocument/definition",
        navigation_params(&main, first_source, "OldThing", 0),
    );
    server.send_request(
        RequestId::from("latest-snapshot-second".to_string()),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": {"line": 0, "character": 0}
        }),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let symbols_id = RequestId::from("latest-snapshot-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );
    assert!(
        server
            .response_with_timeout(&symbols_id, Duration::from_millis(100))
            .is_none(),
        "queued analysis must wait for a worker slot"
    );

    barrier.release();
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "latest snapshot request failed: {symbols:?}"
    );
    let symbols = symbols.result.expect("document symbols");
    let symbols = symbols.as_array().expect("document symbol array");
    assert!(
        symbols.iter().any(|symbol| symbol["name"] == "NewThing"),
        "queued analysis returned the obsolete source snapshot: {symbols:?}"
    );
    assert!(
        symbols.iter().all(|symbol| symbol["name"] != "OldThing"),
        "queued analysis retained an obsolete declaration: {symbols:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn newer_document_version_supersedes_a_queued_observation_once() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nprocedure ChangedRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nprocedure ChangedRoutine;\nbegin\nend;\nend.\n";
    let first_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    let second_source = first_source.replace("PublicRoutine", "ChangedRoutine");
    write_file(&provider, provider_source);
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": first_source
            }
        }),
    );
    server.send_request(
        RequestId::from("supersede-blocker-main".to_string()),
        "textDocument/definition",
        navigation_params(&main, first_source, "Run", 0),
    );
    server.send_request(
        RequestId::from("supersede-blocker-provider".to_string()),
        "textDocument/definition",
        navigation_params(&provider, provider_source, "PublicRoutine", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let old_id = RequestId::from("superseded-old".to_string());
    let mut old_params = navigation_params(&main, first_source, "PublicRoutine", 0);
    old_params["workDoneToken"] = json!("superseded-old-progress");
    server.send_request(old_id.clone(), "textDocument/definition", old_params);
    let old_begin = server.notification("$/progress");
    assert_eq!(old_begin["token"], "superseded-old-progress");
    assert_eq!(old_begin["value"]["message"], "Queued for analysis");
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );
    let new_id = RequestId::from("superseded-new".to_string());
    let mut new_params = navigation_params(&main, &second_source, "ChangedRoutine", 0);
    new_params["workDoneToken"] = json!("superseded-new-progress");
    server.send_request(new_id.clone(), "textDocument/definition", new_params);

    let old_response = server
        .response_with_timeout(&old_id, Duration::from_secs(1))
        .expect("superseded request response");
    assert_eq!(old_response.error.expect("superseded error").code, -32800);
    server.assert_no_response(&old_id);
    let old_end = server.notification("$/progress");
    assert_eq!(old_end["token"], "superseded-old-progress");
    assert_eq!(old_end["value"]["kind"], "end");
    let new_begin = server.notification("$/progress");
    assert_eq!(new_begin["token"], "superseded-new-progress");
    assert_eq!(new_begin["value"]["message"], "Queued for analysis");

    barrier.release();
    let new_report = server.notification("$/progress");
    assert_eq!(new_report["token"], "superseded-new-progress");
    assert_eq!(new_report["value"]["message"], "Analysis started");
    let new_response = server.response(&new_id);
    assert!(
        new_response.error.is_none(),
        "new request failed: {new_response:?}"
    );
    assert_eq!(
        result_locations(new_response).len(),
        1,
        "newer version must still be dispatched"
    );
    let new_end = server.notification("$/progress");
    assert_eq!(new_end["token"], "superseded-new-progress");
    assert_eq!(new_end["value"]["kind"], "end");
    server.assert_no_progress();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_cancels_queued_requests_without_dispatching_them() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_request(
        RequestId::from("shutdown-first".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    server.send_request(
        RequestId::from("shutdown-second".to_string()),
        "textDocument/definition",
        navigation_params(&main, main_source, "Run", 0),
    );
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);
    let queued_id = RequestId::from("shutdown-queued".to_string());
    server.send_request(
        queued_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let shutdown_id = RequestId::from("shutdown-request".to_string());
    server.send_request(shutdown_id.clone(), "shutdown", Value::Null);

    let queued = server.response(&queued_id);
    assert_eq!(
        queued.error.expect("queued shutdown cancellation").code,
        -32800
    );
    let shutdown = server.response(&shutdown_id);
    assert!(shutdown.error.is_none(), "shutdown failed: {shutdown:?}");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read(&barrier.entered)
            .expect("shutdown barrier entries")
            .len(),
        3,
        "shutdown must not dispatch a queued request"
    );
    server.send_notification("exit", Value::Null);
    server.stdin.take();
    let status = server.child.wait().expect("wait for shutdown server");
    assert!(status.success(), "server exited unsuccessfully: {status}");
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_blocked_navigation_returns_one_request_cancelled_response() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancelled-blocked-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-blocked-navigation"}),
    );

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("cancelled navigation must return an error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.assert_no_response(&request_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_blocked_formatting_returns_one_request_cancelled_response() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancelled-blocked-formatting".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-blocked-formatting"}),
    );

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("cancelled formatting must return an error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.assert_no_response(&request_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_blocked_range_formatting_returns_request_cancelled() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source =
        "unit Main;\ninterface\nimplementation\nprocedure Run;\nbegin\nX:=1;\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("cancelled-blocked-range-formatting".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rangeFormatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 5}},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "$/cancelRequest",
        json!({"id": "cancelled-blocked-range-formatting"}),
    );

    let response = server.response(&request_id);
    let error = response
        .error
        .expect("cancelled range formatting must return an error");
    assert_eq!(error.code, -32800);
    assert_eq!(error.message, "request cancelled");
    server.assert_no_response(&request_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn changed_open_snapshot_discards_blocked_range_formatting_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let first_source =
        "unit Main;\ninterface\nimplementation\nprocedure Run;\nbegin\nX:=1;\nend;\nend.\n";
    let second_source = first_source.replace("X:=1;", "X:=2;");
    write_file(&main, first_source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&main), "languageId": "pascal", "version": 1, "text": first_source}}),
    );
    let _ = server.notification("textDocument/publishDiagnostics");
    let request_id = RequestId::from("stale-blocked-range-formatting".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rangeFormatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 5}},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": second_source}]
        }),
    );
    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .expect("newer overlay must stale range formatting");
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn changed_formatter_config_discards_blocked_range_formatting_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let config = root.join(".fmt4d.toml");
    let source =
        "unit Main;\ninterface\nimplementation\nprocedure Run;\nbegin\nX:=1;\nend;\nend.\n";
    write_file(&main, source);
    write_file(&config, "[format]\nindent_size = 2\n");

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("stale-blocked-range-config".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/rangeFormatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 5}},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
    write_file(&config, "[format]\nindent_size = 4\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    let fence_id = RequestId::from("formatter-config-event-fence".to_string());
    server.send_request(
        fence_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(server.response(&fence_id).error.is_none());
    barrier.release();
    let response = server.response(&request_id);
    let error = response
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("changed formatter config must stale result: {response:?}"));
    assert_eq!(error.code, -32803);
    assert_eq!(
        error.message,
        "analysis result became stale; retry the request"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn configuration_invalidation_discards_an_in_flight_diagnostic_result() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &config,
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    barrier.wait_until_entered();

    write_file(&config, "[rules]\nconstant-naming = \"off\"\n");
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );
    barrier.release();

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(diagnostics["version"], 1);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .all(|diagnostic| diagnostic["code"] != "constant-naming"),
        "diagnostics from the invalidated configuration were published: {diagnostics}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn rapid_document_changes_coalesce_to_one_final_diagnostic_computation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let config = root.join(".lint4d.toml");
    let source_v1 = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
    let source_v2 = source_v1.replace("badConst", "anotherConst");
    let source_v3 = source_v1.replace("badConst", "GoodConst");
    write_file(&main, source_v1);
    write_file(
        &config,
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source_v1
            }
        }),
    );
    barrier.wait_until_entered();
    barrier.release();
    let initial = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(initial["version"], 1);

    fs::remove_file(&barrier.entered).expect("reset diagnostic barrier entry");
    fs::remove_file(&barrier.release).expect("reset diagnostic barrier release");
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": source_v2}]
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 3},
            "contentChanges": [{"text": source_v3}]
        }),
    );
    barrier.wait_until_entered();
    let entries = fs::read(&barrier.entered).expect("read diagnostic barrier entries");
    assert_eq!(
        entries.len(),
        1,
        "rapid changes must not start more than one final diagnostic computation"
    );
    barrier.release();

    let diagnostics = diagnostics_for_uri(&mut server, &uri(&main));
    assert_eq!(diagnostics["version"], 3);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_cancels_a_blocked_analysis_worker_before_exiting() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("shutdown-blocked-navigation".to_string());
    server.send_request(
        request_id,
        "textDocument/definition",
        navigation_params(&main, main_source, "PublicRoutine", 0),
    );
    barrier.wait_until_entered();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_open_notification_does_not_cancel_main_diagnostics() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let other = root.join("Other.pas");
    let source = "unit Main;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    let other_source = "unit Other;\ninterface\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(&other, other_source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    barrier.wait_until_entered();

    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&other),
                "languageId": "pascal",
                "version": 1,
                "text": other_source
            }
        }),
    );
    barrier.release();

    let diagnostics = server
        .diagnostic_with_timeout(&uri(&main), Duration::from_secs(5))
        .expect("Main diagnostics after unrelated open");
    assert_eq!(diagnostics["version"], 1);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unknown_notification_does_not_cancel_main_diagnostics() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nconst badConst = 1;\nimplementation\nend.\n";
    write_file(&main, source);
    write_file(
        &root.join(".lint4d.toml"),
        "[rules]\nconstant-naming = \"warning\"\n[rules.naming]\nconstant_style = \"PascalCase\"\n",
    );

    let (mut server, barrier) = TestServer::launch_with_diagnostics_barrier(environment);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    barrier.wait_until_entered();
    server.send_notification("$/setTrace", json!({"value": "off"}));
    barrier.release();

    let diagnostics = server
        .diagnostic_with_timeout(&uri(&main), Duration::from_secs(5))
        .expect("Main diagnostics after unrelated trace notification");
    assert_eq!(diagnostics["version"], 1);
    assert!(
        diagnostics["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "constant-naming")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn client_request_id_cannot_collide_with_internal_diagnostic_job() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let barrier_directory = environment.path().join("analysis-barriers");
    fs::create_dir_all(&barrier_directory).expect("barrier directory");
    let diagnostic_entered = barrier_directory.join("diagnostics.entered");
    let diagnostic_release = barrier_directory.join("diagnostics.release");
    let formatting_entered = barrier_directory.join("formatting.entered");
    let formatting_release = barrier_directory.join("formatting.release");
    let diagnostic_value = format!(
        "{}|{}",
        diagnostic_entered.display(),
        diagnostic_release.display()
    );
    let formatting_value = format!(
        "{}|{}",
        formatting_entered.display(),
        formatting_release.display()
    );
    let executable = env!("CARGO_BIN_EXE_pascal-lsp-test-server");
    let child = Command::new(executable)
        .arg("--stdio")
        .env("HOME", environment.path().join("home"))
        .env("XDG_CONFIG_HOME", environment.path().join("config"))
        .env("PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER", &diagnostic_value)
        .env("PASCAL_LSP_TEST_FORMATTING_BARRIER", &formatting_value)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch pascal-lsp");
    let mut server = TestServer::from_child(child);
    server.initialize(&root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    wait_for_path(&diagnostic_entered);

    let colliding_id = RequestId::from("pascal-lsp-diagnostics-0".to_string());
    server.send_request(
        colliding_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    wait_for_path(&formatting_entered);
    fs::write(&diagnostic_release, "release").expect("release diagnostics barrier");
    thread::sleep(Duration::from_millis(100));

    let ping_id = RequestId::from("collision-ping".to_string());
    server.send_request(ping_id.clone(), "review/ping", Value::Null);
    let ping = server
        .response_with_timeout(&ping_id, Duration::from_secs(1))
        .expect("unrecognized request must remain responsive");
    assert_eq!(ping.error.expect("unknown method error").code, -32601);

    server.send_notification("$/cancelRequest", json!({"id": colliding_id}));
    fs::write(&formatting_release, "release").expect("release formatting barrier");
    let formatting = server
        .response_with_timeout(&colliding_id, Duration::from_secs(1))
        .expect("colliding client request must receive one response");
    assert_eq!(
        formatting.error.expect("cancelled formatting error").code,
        -32800
    );
    server.assert_no_response(&colliding_id);
}

#[cfg(feature = "test-support")]
#[test]
fn production_binary_built_with_test_support_does_not_activate_test_barrier_from_environment() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);
    let entered = environment.path().join("formatting.entered");
    let release = environment.path().join("formatting.release");
    let value = format!("{}|{}", entered.display(), release.display());
    let mut server = TestServer::launch_with_environment_path_and_variable(
        environment.path(),
        Some("PASCAL_LSP_TEST_FORMATTING_BARRIER"),
        Some(&value),
    );
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("feature-enabled-production-barrier-probe".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    let response = server
        .response_with_timeout(&request_id, Duration::from_secs(1))
        .expect("production binary must not stall on test barrier environment");
    assert!(response.error.is_none(), "formatting failed: {response:?}");
    assert!(
        !entered.exists(),
        "production binary built with test-support wrote a test barrier marker"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancellation_takes_precedence_over_stale_generation_for_formatting() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n";
    write_file(&main, source);

    let (mut server, barrier) = TestServer::launch_with_formatting_barrier(environment);
    server.initialize(&root, Value::Null);
    let request_id = RequestId::from("stale-formatting-cancellation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": uri(&main)},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    barrier.wait_until_entered();
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    server.send_notification("$/cancelRequest", json!({"id": request_id}));

    let response = server
        .response_with_timeout(&request_id, Duration::from_secs(1))
        .expect("cancelled formatting must respond");
    assert_eq!(response.error.expect("cancellation error").code, -32800);
    server.assert_no_response(&request_id);
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_invalidates_an_inflight_navigation_at_the_delivery_barrier() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let main_source = "unit Main; interface uses Provider; implementation procedure Run; begin ProviderRoutine; end; end.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit Provider; interface procedure ProviderRoutine; implementation procedure ProviderRoutine; begin end; end.\n",
    );

    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier_and_configuration(environment);
    server.initialize_with_configuration_capability(&root, Some(true), Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));

    let request_id = RequestId::from("runtime-stale-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ProviderRoutine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification("workspace/didChangeConfiguration", json!({"settings": {}}));
    let update = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("configuration refresh pull");
    server.send(Message::Response(Response::new_ok(
        update.id,
        json!([{"maxFiles": 1}]),
    )));
    barrier.release();

    let response = server.response(&request_id);
    assert_eq!(response.error.expect("stale navigation error").code, -32803);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn identical_runtime_configuration_does_not_stale_an_inflight_navigation() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let main_source = "unit Main; interface uses Provider; implementation procedure Run; begin ProviderRoutine; end; end.\n";
    write_file(&main, main_source);
    write_file(
        &provider,
        "unit Provider; interface procedure ProviderRoutine; implementation procedure ProviderRoutine; begin end; end.\n",
    );

    let (mut server, barrier) =
        TestServer::launch_with_navigation_barrier_and_configuration(environment);
    server.initialize_with_configuration_capability(&root, Some(true), Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(
        initial.id,
        json!([{"maxFiles": 1}]),
    )));

    let request_id = RequestId::from("runtime-identical-navigation".to_string());
    server.send_request(
        request_id.clone(),
        "textDocument/definition",
        navigation_params(&main, main_source, "ProviderRoutine", 0),
    );
    barrier.wait_until_entered();

    server.send_notification("workspace/didChangeConfiguration", json!({"settings": {}}));
    let update = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("configuration refresh pull");
    server.send(Message::Response(Response::new_ok(
        update.id,
        json!([{"maxFiles": 1}]),
    )));
    barrier.release();

    let response = server.response(&request_id);
    assert!(
        response.error.is_none(),
        "identical runtime settings must not stale a request: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_preparation_does_not_block_protocol_shutdown() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    fs::create_dir_all(&root).expect("workspace root");
    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server.initialize_with_configuration_capability(&root, Some(true), Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));

    server.send_notification("workspace/didChangeConfiguration", json!({"settings": {}}));
    let refresh = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("runtime configuration refresh");
    server.send(Message::Response(Response::new_ok(
        refresh.id,
        json!([{"exclude": ["generated/**"]}]),
    )));
    barrier.wait_until_entered();

    let ping_id = RequestId::from("configuration-preparation-ping".to_string());
    server.send_request(ping_id.clone(), "review/ping", Value::Null);
    let ping = server
        .response_with_timeout(&ping_id, Duration::from_secs(1))
        .expect("protocol request while configuration is prepared");
    assert_eq!(ping.error.expect("unknown method error").code, -32601);

    let shutdown_id = RequestId::from("configuration-preparation-shutdown".to_string());
    server.send_request(shutdown_id.clone(), "shutdown", Value::Null);
    let shutdown = server
        .response_with_timeout(&shutdown_id, Duration::from_secs(1))
        .expect("shutdown while configuration is prepared");
    assert!(shutdown.error.is_none(), "shutdown failed: {shutdown:?}");
    server.send_notification("exit", Value::Null);
    server.stdin.take();
    let status = server
        .child
        .wait()
        .expect("wait for configuration shutdown");
    assert!(status.success(), "server exited unsuccessfully: {status}");
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_compiler_context_change_stales_navigation_and_changes_the_branch() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = concat!(
        "unit Main;\ninterface\n",
        "type\n",
        "{$IF CompilerVersion >= 24}\n",
        "  TModern = class\n  end;\n",
        "{$ELSE}\n",
        "  TLegacy = class\n  end;\n",
        "{$ENDIF}\n",
        "implementation\n",
        "procedure Run;\nvar\n  Value: TModern;\nbegin\nend;\nend.\n"
    );
    write_file(&main, source);

    let (mut server, configuration_barrier, navigation_barrier) =
        TestServer::launch_with_navigation_and_configuration_preparation_barriers(environment);
    let initialize_id = RequestId::from("runtime-compiler-initialize".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(&root),
            "initializationOptions": {"compilerVersion": "24.0"},
            "capabilities": {
                "textDocument": {
                    "diagnostic": {
                        "dynamicRegistration": false,
                        "relatedDocumentSupport": true
                    }
                },
                "workspace": {
                    "diagnostics": {"refreshSupport": true},
                    "configuration": true,
                    "workspaceFolders": true,
                    "workspaceEdit": {"documentChanges": true}
                }
            }
        }),
    );
    let initialize = server.response(&initialize_id);
    assert!(
        initialize.error.is_none(),
        "initialize failed: {initialize:?}"
    );
    initialize.result.expect("initialize result");
    server.send_notification("initialized", Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(
        initial.id,
        json!([null]),
    )));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );

    // Initial null configuration does not apply a changed effective setting;
    // didOpen produces the one startup refresh. The ping may precede its ACKs
    // in client-to-server FIFO, so the ping itself is not their ordering point.
    // Those ACKs are sent before the initial definition request below; its
    // response follows FIFO processing and refresh retirement before the target
    // transition begins. The target refresh is then observed after apply while
    // stale navigation remains held.
    let startup_drain_id = RequestId::from("runtime-compiler-startup-refresh-drain".to_string());
    server.send_request(startup_drain_id.clone(), "review/ping", Value::Null);
    let mut startup_refresh_ids = Vec::new();
    loop {
        match server.next_message() {
            Message::Request(request) if request.method == "workspace/diagnostic/refresh" => {
                assert!(
                    !startup_refresh_ids.contains(&request.id),
                    "duplicate startup refresh ID: {}",
                    request.id
                );
                startup_refresh_ids.push(request.id.clone());
                server.send(Message::Response(Response::new_ok(request.id, Value::Null)));
            }
            Message::Response(response) if response.id == startup_drain_id => {
                assert_eq!(
                    response.error.expect("unknown review/ping method").code,
                    -32601
                );
                break;
            }
            message => panic!("unexpected message during startup refresh drain: {message:?}"),
        }
    }
    assert!(
        !startup_refresh_ids.is_empty(),
        "opening the document must request an initial pull-diagnostic refresh"
    );

    let initial_id = RequestId::from("runtime-compiler-initial-definition".to_string());
    server.send_request(
        initial_id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TModern", 1),
    );
    navigation_barrier.wait_until_entered();
    navigation_barrier.release();
    let initial_locations = result_locations(server.response(&initial_id));
    assert_eq!(
        initial_locations.len(),
        1,
        "initial compiler context must select the modern branch"
    );
    fs::remove_file(&navigation_barrier.release).expect("rearm navigation barrier");

    let stale_id = RequestId::from("runtime-compiler-stale-definition".to_string());
    server.send_request(
        stale_id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TModern", 1),
    );
    navigation_barrier.wait_for_entries(2);
    begin_configuration_preparation(
        &mut server,
        json!({"compilerVersion": "23.0"}),
        &configuration_barrier,
    );
    configuration_barrier.release();
    assert!(
        configuration_barrier.release.exists(),
        "configuration preparation must be released before observing its applied refresh"
    );
    assert!(
        !navigation_barrier.release.exists(),
        "navigation must still be held while waiting for the commit observer"
    );
    let commit_refresh = server
        .request_with_timeout("workspace/diagnostic/refresh", IO_TIMEOUT)
        .expect("applied runtime configuration must request a diagnostic refresh");
    assert!(
        !startup_refresh_ids.contains(&commit_refresh.id),
        "startup, didOpen, or coalesced pre-target refresh {} cannot witness target commit",
        commit_refresh.id
    );
    assert!(
        configuration_barrier.release.exists() && !navigation_barrier.release.exists(),
        "commit observer must be received after configuration release and before navigation release"
    );
    server.send(Message::Response(Response::new_ok(
        commit_refresh.id,
        Value::Null,
    )));
    navigation_barrier.release();
    let stale_response = server.response(&stale_id);
    assert_eq!(
        stale_response.error.as_ref().map(|error| error.code),
        Some(-32803),
        "context change must stale the in-flight query after commit: {stale_response:?}"
    );

    let recovery_id = RequestId::from("runtime-compiler-recovery-definition".to_string());
    server.send_request(
        recovery_id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TModern", 1),
    );
    assert!(
        result_locations(server.response(&recovery_id)).is_empty(),
        "the changed compiler context must not reuse the modern branch"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_compiler_context_change_may_return_old_navigation_before_commit() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = concat!(
        "unit Main;\ninterface\n",
        "type\n",
        "{$IF CompilerVersion >= 24}\n",
        "  TModern = class\n  end;\n",
        "{$ELSE}\n",
        "  TLegacy = class\n  end;\n",
        "{$ENDIF}\n",
        "implementation\n",
        "procedure Run;\nvar\n  Value: TModern;\nbegin\nend.\n"
    );
    write_file(&main, source);

    let (mut server, configuration_barrier, navigation_barrier) =
        TestServer::launch_with_navigation_and_configuration_preparation_barriers(environment);
    server.initialize_with_configuration_and_document_changes(
        &root,
        json!({"compilerVersion": "24.0"}),
    );
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(
        initial.id,
        json!([null]),
    )));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );

    let initial_id = RequestId::from("runtime-compiler-precommit-initial-definition".to_string());
    server.send_request(
        initial_id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TModern", 1),
    );
    navigation_barrier.wait_until_entered();
    navigation_barrier.release();
    let initial_locations = result_locations(server.response(&initial_id));
    assert_eq!(
        initial_locations.len(),
        1,
        "initial compiler context must select the modern branch"
    );
    fs::remove_file(&navigation_barrier.release).expect("rearm navigation barrier");

    let before_commit_id = RequestId::from("runtime-compiler-definition-before-commit".to_string());
    server.send_request(
        before_commit_id.clone(),
        "textDocument/definition",
        navigation_params(&main, source, "TModern", 1),
    );
    navigation_barrier.wait_for_entries(2);
    begin_configuration_preparation(
        &mut server,
        json!({"compilerVersion": "23.0"}),
        &configuration_barrier,
    );
    assert!(
        !configuration_barrier.release.exists(),
        "configuration must not be committed before releasing the positive-control query"
    );
    navigation_barrier.release();
    let before_commit = server.response(&before_commit_id);
    assert!(
        before_commit.error.is_none(),
        "old-context navigation may complete before configuration commit: {before_commit:?}"
    );
    assert_eq!(
        result_locations(before_commit),
        initial_locations,
        "pre-commit navigation must return the original TModern location"
    );

    configuration_barrier.release();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_rejects_a_deferred_rename_after_document_identity_changes() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let alpha_source = "unit Main; interface var Alpha: Integer; implementation procedure Run; begin Alpha := 1; end; end.\n";
    let beta_source = alpha_source.replace("Alpha", "BetaVariable");
    write_file(&main, alpha_source);

    let (mut server, configuration_barrier, navigation_barrier) =
        TestServer::launch_with_navigation_and_configuration_preparation_barriers(environment);
    server.initialize_with_configuration_and_document_changes(&root, Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": alpha_source
            }
        }),
    );

    begin_configuration_preparation(
        &mut server,
        json!({"maxFiles": 9999}),
        &configuration_barrier,
    );
    let prepare_id = RequestId::from("deferred-alpha-prepare".to_string());
    server.send_request(
        prepare_id.clone(),
        "textDocument/prepareRename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(alpha_source, "Alpha", 0)
        }),
    );
    let rename_id = RequestId::from("deferred-alpha-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(alpha_source, "Alpha", 0),
            "newName": "Renamed"
        }),
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": beta_source}]
        }),
    );

    let ping_id = RequestId::from("deferred-rename-ordering-ping".to_string());
    server.send_request(ping_id.clone(), "review/ping", Value::Null);
    let ping = server
        .response_with_timeout(&ping_id, Duration::from_secs(1))
        .expect("later document change must be processed while preparation is held");
    assert_eq!(ping.error.expect("unknown method error").code, -32601);

    configuration_barrier.release();
    navigation_barrier.wait_until_entered();
    navigation_barrier.release();
    let prepare = server.response(&prepare_id);
    let prepare_error = prepare.error.expect("stale prepareRename must fail");
    assert_eq!(prepare_error.code, -32803);
    let rename = server.response(&rename_id);
    let rename_error = rename.error.expect("stale rename must fail");
    assert_eq!(rename_error.code, -32803);
    assert!(
        !rename
            .result
            .is_some_and(|value| value.to_string().contains("Renamed"))
    );

    let recovery_id = RequestId::from("deferred-beta-recovery".to_string());
    server.send_request(
        recovery_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(&beta_source, "BetaVariable", 0),
            "newName": "Renamed"
        }),
    );
    let recovery = server.response(&recovery_id);
    assert!(
        recovery.error.is_none(),
        "current-document rename failed: {recovery:?}"
    );
    assert!(
        recovery
            .result
            .expect("recovery edit")
            .to_string()
            .contains("Renamed")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_rejects_a_deferred_rename_after_same_version_close_and_reopen() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source = "unit Main; interface var Alpha: Integer; implementation procedure Run; begin Alpha := 1; end; end.\n";
    write_file(&main, source);

    let (mut server, configuration_barrier, navigation_barrier) =
        TestServer::launch_with_navigation_and_configuration_preparation_barriers(environment);
    server.initialize_with_configuration_and_document_changes(&root, Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    begin_configuration_preparation(
        &mut server,
        json!({"maxFiles": 9999}),
        &configuration_barrier,
    );

    let rename_id = RequestId::from("same-version-reopen-rename".to_string());
    server.send_request(
        rename_id.clone(),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Alpha", 0),
            "newName": "Renamed"
        }),
    );
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source
            }
        }),
    );
    configuration_barrier.release();
    navigation_barrier.wait_until_entered();
    navigation_barrier.release();

    let response = server.response(&rename_id);
    assert_eq!(
        response
            .error
            .expect("same-version reopen must stale the request")
            .code,
        -32803
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_cancellation_allows_deferred_request_id_reuse() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source =
        "unit Main; interface procedure Run; implementation procedure Run; begin end; end.\n";
    write_file(&main, source);

    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server.initialize_with_configuration_and_document_changes(&root, Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    begin_configuration_preparation(&mut server, json!({"maxFiles": 9999}), &barrier);

    let reused_id = RequestId::from("configuration-deferred-reused-id".to_string());
    server.send_request(
        reused_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    server.send_notification("$/cancelRequest", json!({"id": reused_id.clone()}));
    let cancelled = server.response(&reused_id);
    assert_eq!(cancelled.error.expect("cancellation error").code, -32800);

    server.send_request(
        reused_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    barrier.release();
    let response = server.response(&reused_id);
    assert!(
        response.error.is_none(),
        "reused deferred request failed: {response:?}"
    );
    server.assert_no_response(&reused_id);
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_request_flood_cannot_displace_a_document_change() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let source_v1 = "unit Main; interface var Alpha: Integer; implementation procedure Run; begin Alpha := 1; end; end.\n";
    let source_v2 = source_v1.replace("Alpha", "Bravo");
    let source_v3 = source_v1.replace("Alpha", "Charlie");
    write_file(&main, source_v1);

    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server.initialize_with_configuration_and_document_changes(&root, Value::Null);
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": source_v1
            }
        }),
    );
    begin_configuration_preparation(&mut server, json!({"maxFiles": 9999}), &barrier);

    let request_ids = (0..64)
        .map(|index| RequestId::from(format!("flooded-document-symbol-{index}")))
        .collect::<Vec<_>>();
    for request_id in &request_ids {
        server.send_request(
            request_id.clone(),
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri(&main)}}),
        );
    }

    let excess_id = RequestId::from("flooded-document-symbol-excess".to_string());
    server.send_request(
        excess_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let before_edit_ping_id = RequestId::from("flooded-before-edit-ping".to_string());
    server.send_request(before_edit_ping_id.clone(), "review/ping", Value::Null);
    let excess = server
        .response_with_timeout(&excess_id, Duration::from_secs(1))
        .expect("excess request must be rejected while preparation is held");
    assert_eq!(
        excess.error.expect("excess request error").code,
        -32802,
        "request capacity must remain bounded"
    );
    let before_edit_ping = server
        .response_with_timeout(&before_edit_ping_id, Duration::from_secs(1))
        .expect("server must remain responsive before the edit");
    assert_eq!(
        before_edit_ping.error.expect("unknown method error").code,
        -32601
    );

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": source_v2}]
        }),
    );
    let after_edit_ping_id = RequestId::from("flooded-after-edit-ping".to_string());
    server.send_request(after_edit_ping_id.clone(), "review/ping", Value::Null);
    let after_edit_ping = server
        .response_with_timeout(&after_edit_ping_id, Duration::from_secs(1))
        .expect("a valid edit must not terminate the server");
    assert_eq!(
        after_edit_ping.error.expect("unknown method error").code,
        -32601
    );
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 3},
            "contentChanges": [{"text": source_v3}]
        }),
    );

    barrier.release();
    let mut cancelled = 0;
    for request_id in &request_ids {
        let response = server.response(request_id);
        if let Some(error) = response.error {
            assert!(
                error.code == -32802 || error.code == -32803,
                "only retryable or stale flood requests may be rejected: {error:?}"
            );
            if error.code == -32802 {
                cancelled += 1;
            }
        }
    }
    assert_eq!(
        cancelled, 2,
        "the two notifications must reclaim exactly two request slots"
    );

    let symbols_id = RequestId::from("flooded-document-symbol-v3".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "the final document overlay must remain authoritative after the flood: {symbols:?}"
    );
    assert!(
        symbols
            .result
            .expect("document symbols")
            .to_string()
            .contains("Charlie")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_processes_did_open_after_an_increased_file_limit() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let disk_source = "unit Main; interface implementation end.\n";
    let overlay_source = "unit Main; interface var OverlayOnly: Integer; implementation procedure Run; begin OverlayOnly := 1; end; end.\n";
    write_file(&main, disk_source);

    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server.initialize_with_configuration_and_document_changes(&root, json!({"maxFileBytes": 64}));
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    begin_configuration_preparation(&mut server, json!({"maxFileBytes": 10000}), &barrier);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": overlay_source
            }
        }),
    );
    let ping_id = RequestId::from("did-open-increase-ping".to_string());
    server.send_request(ping_id.clone(), "review/ping", Value::Null);
    let ping = server
        .response_with_timeout(&ping_id, Duration::from_secs(1))
        .expect("didOpen must not block protocol progress");
    assert_eq!(ping.error.expect("unknown method error").code, -32601);
    barrier.release();

    let symbols_id = RequestId::from("did-open-increase-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "increased limit lost didOpen: {symbols:?}"
    );
    let symbol_result = symbols.result.expect("document symbols");
    let names = symbol_result
        .as_array()
        .expect("symbol array")
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"OverlayOnly"));
    assert!(names.contains(&"Run"));
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_processes_did_open_after_a_decreased_file_limit() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let disk_source = "unit Main; interface implementation end.\n";
    let oversized_source = "unit Main; interface var OverlayOnly: Integer; implementation procedure Run; begin OverlayOnly := 1; end; end.\n";
    let recovered_source = "unit Main; interface var R: Integer; implementation end.\n";
    write_file(&main, disk_source);

    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server
        .initialize_with_configuration_and_document_changes(&root, json!({"maxFileBytes": 10000}));
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    begin_configuration_preparation(&mut server, json!({"maxFileBytes": 64}), &barrier);
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": oversized_source
            }
        }),
    );
    barrier.release();

    let symbols_id = RequestId::from("did-open-decrease-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert_eq!(
        symbols
            .error
            .expect("decreased limit must reject overlay")
            .code,
        -32803
    );

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": recovered_source}]
        }),
    );
    let recovery_id = RequestId::from("did-open-decrease-recovery".to_string());
    server.send_request(
        recovery_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let recovery = server.response(&recovery_id);
    assert!(
        recovery.error.is_none(),
        "full replacement must recover: {recovery:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_processes_did_change_after_an_increased_file_limit() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let short_source = "unit Main; interface implementation end.\n";
    let oversized_source = "unit Main; interface var OverlayOnly: Integer; implementation procedure Run; begin OverlayOnly := 1; end; end.\n";
    write_file(&main, short_source);

    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server.initialize_with_configuration_and_document_changes(&root, json!({"maxFileBytes": 64}));
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": short_source
            }
        }),
    );
    begin_configuration_preparation(&mut server, json!({"maxFileBytes": 10000}), &barrier);
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": oversized_source}]
        }),
    );
    barrier.release();

    let symbols_id = RequestId::from("did-change-increase-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert!(
        symbols.error.is_none(),
        "increased limit lost didChange: {symbols:?}"
    );
    assert!(
        symbols
            .result
            .expect("document symbols")
            .to_string()
            .contains("OverlayOnly")
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn runtime_configuration_processes_did_change_after_a_decreased_file_limit() {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let main = root.join("Main.pas");
    let oversized_source = "unit Main; interface var OverlayOnly: Integer; implementation procedure Run; begin OverlayOnly := 1; end; end.\n";
    let recovered_source = "unit Main; interface var R: Integer; implementation end.\n";
    write_file(&main, oversized_source);

    let (mut server, barrier) =
        TestServer::launch_with_configuration_preparation_barrier(environment);
    server
        .initialize_with_configuration_and_document_changes(&root, json!({"maxFileBytes": 10000}));
    let initial = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("initial configuration pull");
    server.send(Message::Response(Response::new_ok(initial.id, Value::Null)));
    server.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": oversized_source
            }
        }),
    );
    begin_configuration_preparation(&mut server, json!({"maxFileBytes": 64}), &barrier);
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 2},
            "contentChanges": [{"text": oversized_source}]
        }),
    );
    barrier.release();

    let symbols_id = RequestId::from("did-change-decrease-symbols".to_string());
    server.send_request(
        symbols_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let symbols = server.response(&symbols_id);
    assert_eq!(
        symbols
            .error
            .expect("decreased limit must reject change")
            .code,
        -32803
    );

    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(&main), "version": 3},
            "contentChanges": [{"text": recovered_source}]
        }),
    );
    let recovery_id = RequestId::from("did-change-decrease-recovery".to_string());
    server.send_request(
        recovery_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    let recovery = server.response(&recovery_id);
    assert!(
        recovery.error.is_none(),
        "full replacement must recover: {recovery:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
fn run_queued_progress_cancellation(cancel_with_work_done_token: bool) {
    let environment = tempfile::tempdir().expect("isolated server environment");
    let root = environment.path().join("workspace");
    let provider = root.join("Provider.pas");
    let main = root.join("Main.pas");
    let provider_source = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
    write_file(&provider, provider_source);
    write_file(&main, main_source);

    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize_with_progress(&root);

    let first_id = RequestId::from("queued-cancel-blocker-first".to_string());
    let mut first_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    first_params["workDoneToken"] = json!("queued-cancel-blocker-first-progress");
    server.send_request(first_id.clone(), "textDocument/definition", first_params);
    let second_id = RequestId::from("queued-cancel-blocker-second".to_string());
    let mut second_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    second_params["workDoneToken"] = json!("queued-cancel-blocker-second-progress");
    server.send_request(second_id.clone(), "textDocument/declaration", second_params);
    barrier.wait_for_entries(2);
    barrier.occupy_interactive_slot(&mut server, &main);

    let blocker_tokens = [
        "queued-cancel-blocker-first-progress",
        "queued-cancel-blocker-second-progress",
    ];
    let mut blocker_events = HashSet::new();
    for _ in 0..4 {
        let event = server.notification("$/progress");
        let token = event["token"].as_str().expect("blocker progress token");
        assert!(blocker_tokens.contains(&token));
        let kind = event["value"]["kind"].as_str().expect("progress kind");
        assert!(matches!(kind, "begin" | "report"));
        if kind == "report" {
            assert_eq!(event["value"]["message"], "Analysis started");
        }
        blocker_events.insert((token.to_string(), kind.to_string()));
    }
    assert_eq!(blocker_events.len(), 4);

    let queued_id = RequestId::from("queued-cancel-recipient".to_string());
    let mut queued_params = navigation_params(&main, main_source, "PublicRoutine", 0);
    queued_params["workDoneToken"] = json!("queued-cancel-progress");
    server.send_request(
        queued_id.clone(),
        "textDocument/implementation",
        queued_params,
    );
    let queued_begin = server.notification("$/progress");
    assert_eq!(queued_begin["token"], "queued-cancel-progress");
    assert_eq!(queued_begin["value"]["message"], "Queued for analysis");

    if cancel_with_work_done_token {
        server.send_notification(
            "window/workDoneProgress/cancel",
            json!({"token": "queued-cancel-progress"}),
        );
    } else {
        server.send_notification("$/cancelRequest", json!({"id": queued_id}));
    }
    let queued_response = server.response(&queued_id);
    assert_eq!(
        queued_response.error.expect("queued cancellation").code,
        -32800
    );
    let queued_end = server.notification("$/progress");
    assert_eq!(queued_end["token"], "queued-cancel-progress");
    assert_eq!(queued_end["value"]["kind"], "end");
    server.assert_no_progress();

    barrier.release();
    for id in [first_id, second_id] {
        let response = server.response(&id);
        assert!(
            response.error.is_none(),
            "blocker request failed: {response:?}"
        );
    }
    let mut blocker_ends = HashSet::new();
    for _ in 0..2 {
        let end = server.notification("$/progress");
        let token = end["token"].as_str().expect("blocker end token");
        assert!(blocker_tokens.contains(&token));
        assert_eq!(end["value"]["kind"], "end");
        blocker_ends.insert(token.to_string());
    }
    assert_eq!(blocker_ends.len(), 2);
    assert_eq!(
        fs::read(&barrier.entered)
            .expect("queued cancellation barrier entries")
            .len(),
        3,
        "cancelled queued work must not dispatch a fourth worker"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
fn request_workspace_notification_deadline_expiry(
    trigger: &Path,
    expired: &Path,
    watchdog: Duration,
) {
    fs::write(trigger, b"expire").expect("request deterministic worker deadline expiry");
    assert!(
        wait_for_file(expired, watchdog),
        "controlled expiry must enter the workspace deadline path and record evidence"
    );
    assert_eq!(
        fs::read(expired).expect("read deadline expiry evidence"),
        b"test-triggered",
        "deadline expiry must be caused by the test-control trigger"
    );
}

#[cfg(feature = "test-support")]
fn assert_saturated_control_bypasses_workspace_fifo(
    send_exit: bool,
    extra_ordinary_frames: usize,
    send_cancel: bool,
    deadline_ms: Option<u64>,
    verify_recovery_query: bool,
    final_delete_after_barrier: bool,
) {
    const CONTROL_WATCHDOG: Duration = Duration::from_secs(30);
    const EXPIRY_EVIDENCE_WATCHDOG: Duration = Duration::from_secs(30);
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let consumer = root.path().join("Consumer.pas");
    let old_provider = "unit Provider;\ninterface\ntype TBefore = Integer;\nimplementation\nend.\n";
    let new_provider = "unit Provider;\ninterface\ntype TAfter = Integer;\nimplementation\nend.\n";
    write_file(&provider, old_provider);
    let consumer_source =
        "unit Consumer;\ninterface\nuses Provider;\ntype TUse = TAfter;\nimplementation\nend.\n";
    write_file(&consumer, consumer_source);
    let barrier_dir = root.path().join("saturated-shutdown-barrier");
    fs::create_dir_all(&barrier_dir).expect("barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let navigation_barrier = TestBarrier {
        entered: barrier_dir.join("navigation-entered"),
        release: barrier_dir.join("navigation-release"),
    };
    let completed = barrier_dir.join("completed");
    let fifo_state = barrier_dir.join("fifo-state.json");
    let reader_pending = barrier_dir.join("reader-pending.json");
    let deadline_expiry_trigger = barrier_dir.join("expire-deadline");
    let deadline_expired = barrier_dir.join("deadline-expired");
    let server_stderr = barrier_dir.join("server-stderr.log");
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let navigation_barrier_value = format!(
        "{}|{}",
        navigation_barrier.entered.display(),
        navigation_barrier.release.display()
    );
    let fifo_probe_value = format!(
        "{}|{}|{}|{}",
        fifo_state.display(),
        reader_pending.display(),
        barrier.entered.display(),
        barrier.release.display()
    );
    let completed_value = completed.display().to_string();
    let deadline_value = deadline_ms.map_or_else(String::new, |deadline| deadline.to_string());
    let deadline_expiry_trigger_value = deadline_expiry_trigger.display().to_string();
    let deadline_expired_value = deadline_expired.display().to_string();
    let mut environment_variables = vec![
        (
            "PASCAL_LSP_TEST_FILE_DISCOVERY_BARRIER",
            barrier_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
            completed_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_MS",
            deadline_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_WORKSPACE_FIFO_PROBE",
            fifo_probe_value.as_str(),
        ),
    ];
    if send_cancel {
        environment_variables.push((
            "PASCAL_LSP_TEST_NAVIGATION_BARRIER",
            navigation_barrier_value.as_str(),
        ));
    }
    if deadline_ms.is_some() {
        environment_variables.push((
            "PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_TRIGGER",
            deadline_expiry_trigger_value.as_str(),
        ));
        environment_variables.push((
            "PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_EXPIRED",
            deadline_expired_value.as_str(),
        ));
    }
    let mut server = if deadline_ms.is_some() {
        TestServer::launch_test_server_with_environment_path_and_variables_and_stderr_file(
            root.path(),
            environment_variables,
            &server_stderr,
        )
    } else {
        TestServer::launch_test_server_with_environment_path_and_variables(
            root.path(),
            environment_variables,
        )
    };
    server.initialize_with_pull_diagnostics(root.path());
    let initial_id = RequestId::from("saturated-control-worker-load".to_string());
    server.send_request(
        initial_id.clone(),
        "textDocument/diagnostic",
        json!({"textDocument":{"uri":uri(&provider)},"previousResultId":null}),
    );
    assert!(server.response(&initial_id).error.is_none());
    let active_cancel_id = RequestId::from("saturated-active-navigation-cancel".to_string());
    if send_cancel {
        server.send_request(
            active_cancel_id.clone(),
            "textDocument/definition",
            navigation_params(&provider, old_provider, "TBefore", 0),
        );
        navigation_barrier.wait_until_entered();
    }
    if deadline_ms.is_some() {
        write_file(&provider, new_provider);
    }
    let changes = if final_delete_after_barrier {
        json!({"changes":[
            {"uri":uri(&provider),"type":2},
            {"uri":uri(&provider),"type":3}
        ]})
    } else {
        json!({"changes":[{"uri":uri(&provider),"type":2}]})
    };
    server.send_notification("workspace/didChangeWatchedFiles", changes);
    barrier.wait_until_entered();

    // Fill the 64-message FIFO and its retained overflow slot while the worker
    // is held. Keep the serialized byte accounting and the pending 66th-frame
    // handshake observable rather than inferring either from elapsed time.
    let mut queued_frames = Vec::with_capacity(65);
    for index in 0..65 {
        let overlay = root.path().join(format!("Queued{index:02}.pas"));
        let message = Message::Notification(Notification::new(
            "textDocument/didOpen".to_string(),
            json!({"textDocument":{"uri":uri(&overlay),"languageId":"pascal","version":1,"text":format!("unit Queued{index:02}; interface implementation end.")}}),
        ));
        server.send_tracked(message, &mut queued_frames);
    }
    assert!(
        wait_until_json_bool(&fifo_state, "full", true, CONTROL_WATCHDOG),
        "must observe the saturated deferred FIFO and overflow slot"
    );
    let full_state: Value = serde_json::from_slice(&fs::read(&fifo_state).expect("FIFO state"))
        .expect("valid FIFO state JSON");
    let queued_bytes = queued_frames[..64]
        .iter()
        .map(|message| {
            serde_json::to_vec(message)
                .expect("serialize FIFO frame")
                .len()
        })
        .sum::<usize>();
    let overflow_bytes = serde_json::to_vec(&queued_frames[64])
        .expect("serialize overflow frame")
        .len();
    assert_eq!(queued_frames.len(), 65);
    assert_eq!(full_state["queued_count"], 64);
    assert_eq!(full_state["queued_bytes"], queued_bytes);
    assert_eq!(full_state["overflow_occupied"], true);
    assert_eq!(full_state["overflow_bytes"], overflow_bytes);
    assert_eq!(full_state["worker_held"], true);
    assert_eq!(full_state["reader_pending"], false);
    assert!(queued_bytes + overflow_bytes <= 1024 * 1024);

    if extra_ordinary_frames > 0 {
        let final_message = Message::Notification(Notification::new(
            "textDocument/didOpen".to_string(),
            json!({"textDocument":{"uri":uri(&consumer),"languageId":"pascal","version":1,"text":consumer_source}}),
        ));
        let final_frame_bytes = serde_json::to_vec(&final_message)
            .expect("serialize final ordinary frame")
            .len();
        server.send(final_message);
        assert!(
            wait_until_json_bool(&fifo_state, "reader_pending", true, CONTROL_WATCHDOG),
            "the 66th ordinary frame must be pending at the reader rendezvous"
        );
        assert!(wait_for_file(&reader_pending, CONTROL_WATCHDOG));
        let pending_state: Value =
            serde_json::from_slice(&fs::read(&reader_pending).expect("reader-pending FIFO marker"))
                .expect("valid reader-pending marker JSON");
        assert_eq!(pending_state["reader_pending"], true);
        assert_eq!(pending_state["method"], "textDocument/didOpen");
        assert_eq!(pending_state["frame_bytes"], final_frame_bytes);
        assert_eq!(pending_state["worker_held"], true);
        assert!(queued_bytes + overflow_bytes + final_frame_bytes <= 1024 * 1024);
    }

    if send_cancel {
        server.send_notification("$/cancelRequest", json!({"id":active_cancel_id}));
        assert!(
            !navigation_barrier.release.exists(),
            "the real active request must still be held when cancellation is sent"
        );
        assert!(
            !completed.exists(),
            "the file worker must still be held when cancellation is sent"
        );
        assert!(
            deadline_ms.is_some(),
            "an explicit deadline hook is required to unblock the ordinary reader"
        );
        request_workspace_notification_deadline_expiry(
            &deadline_expiry_trigger,
            &deadline_expired,
            CONTROL_WATCHDOG,
        );
        let cancelled = server
            .response_with_timeout(&active_cancel_id, CONTROL_WATCHDOG)
            .expect("active navigation cancellation response");
        assert_eq!(
            cancelled.error.expect("active request cancellation").code,
            -32800,
            "the queued cancel must produce RequestCancelled after controlled expiry unblocks the reader"
        );
        assert!(
            !navigation_barrier.release.exists(),
            "cancellation must not need the active navigation barrier released"
        );
    }

    if deadline_ms.is_some() && verify_recovery_query {
        request_workspace_notification_deadline_expiry(
            &deadline_expiry_trigger,
            &deadline_expired,
            EXPIRY_EVIDENCE_WATCHDOG,
        );
        let query_id = RequestId::from("saturated-timeout-fresh-provider".to_string());
        server.send_request(
            query_id.clone(),
            "textDocument/definition",
            navigation_params(&consumer, consumer_source, "TAfter", 0),
        );
        let response = server.response_with_timeout(&query_id, CONTROL_WATCHDOG);
        if response.is_none() {
            let _ = server.child.kill();
            let _ = server.child.wait();
            panic!("deferred query was not replayed after controlled deadline expiry");
        }
        let response = response.expect("query response was checked");
        let locations = result_locations(response);
        let expected_provider_uri = uri(&provider).to_string();
        let location_uri = locations
            .first()
            .and_then(|location| location["uri"].as_str());
        if final_delete_after_barrier {
            assert!(
                locations.is_empty(),
                "cancellation during fallback must keep the unprocessed delete tombstone authoritative: {locations:?}"
            );
        } else {
            assert_eq!(
                location_uri,
                Some(expected_provider_uri.as_str()),
                "deferred query must run after conservative timeout fallback and see current disk identity"
            );
        }
        assert!(
            completed.exists(),
            "deadline fallback must complete the held worker"
        );
        assert!(
            !barrier.release.exists(),
            "cooperative deadline cancellation must not release the worker test barrier"
        );
        let refresh = server
            .request_with_timeout("workspace/diagnostic/refresh", CONTROL_WATCHDOG)
            .expect("timeout fallback must request a pull-diagnostic refresh");
        server.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
    }
    let shutdown = if send_exit {
        server.send_notification("exit", Value::Null);
        server.stdin.take();
        if deadline_ms.is_some() && !deadline_expired.exists() {
            request_workspace_notification_deadline_expiry(
                &deadline_expiry_trigger,
                &deadline_expired,
                CONTROL_WATCHDOG,
            );
        }
        None
    } else {
        let shutdown_id = RequestId::from("saturated-priority-shutdown".to_string());
        server.send_request(shutdown_id.clone(), "shutdown", Value::Null);
        if deadline_ms.is_some() && !deadline_expired.exists() {
            request_workspace_notification_deadline_expiry(
                &deadline_expiry_trigger,
                &deadline_expired,
                CONTROL_WATCHDOG,
            );
        }
        server.response_with_timeout(&shutdown_id, CONTROL_WATCHDOG)
    };
    // These are named per-operation hang guards. They make no claim about
    // response or completion latency under load.
    let completed_without_release = wait_for_file(&completed, CONTROL_WATCHDOG);
    if (!send_exit && shutdown.is_none()) || !completed_without_release {
        let _ = server.child.kill();
        let _ = server.child.wait();
    }
    assert!(
        completed_without_release,
        "worker must complete without the test releasing its barrier"
    );
    assert!(
        !barrier.release.exists(),
        "test must not release the worker barrier to make shutdown succeed"
    );
    if !send_exit {
        assert!(
            shutdown.is_some_and(|response| response.error.is_none()),
            "shutdown must bypass the saturated deferred FIFO and overflow slot"
        );
        server.send_notification("exit", Value::Null);
        server.stdin.take();
    }
    let status = server
        .child
        .wait()
        .expect("wait for priority control child");
    if send_exit {
        // LSP requires a nonzero exit when the client exits without first
        // sending shutdown; the watchdog above is only a hang guard.
        assert_eq!(status.code(), Some(1));
    } else {
        assert!(status.success(), "server exited unsuccessfully: {status}");
    }
    if deadline_ms.is_some() {
        let stderr = fs::read_to_string(&server_stderr).expect("read test server stderr");
        assert!(
            stderr.contains(
                "pascal-lsp: workspace notification reconciliation test-triggered expiry; requesting cooperative cancellation"
            ),
            "controlled expiry must log its synthetic cause, got:\n{stderr}"
        );
        assert!(
            !stderr.contains(
                "pascal-lsp: workspace notification reconciliation deadline elapsed; requesting cooperative cancellation"
            ),
            "controlled expiry must not be logged as an elapsed deadline, got:\n{stderr}"
        );
    }
}

#[cfg(feature = "test-support")]
fn assert_rename_overlay_is_invalidated_on_reconciliation_fallback(deadline_ms: Option<u64>) {
    const FALLBACK_WATCHDOG: Duration = Duration::from_secs(30);
    let root = tempfile::tempdir().expect("temporary workspace");
    let provider = root.path().join("Provider.pas");
    let provider_source =
        "unit Provider;\ninterface\ntype TThing = class end;\nimplementation\nend.\n";
    write_file(&provider, provider_source);

    let mut consumers = Vec::new();
    let mut consumer_sources = Vec::new();
    for index in 0..64 {
        let consumer = root.path().join(format!("Consumer{index:02}.pas"));
        let source = format!(
            "unit Consumer{index:02};\ninterface\nuses Provider;\ntype TAlias{index:02} = Provider.TThing;\nimplementation\nend.\n"
        );
        write_file(&consumer, &source);
        consumers.push(consumer);
        consumer_sources.push(source);
    }

    let barrier_dir = root.path().join("rename-budget-barrier");
    fs::create_dir_all(&barrier_dir).expect("rename budget barrier directory");
    let barrier = TestBarrier {
        entered: barrier_dir.join("entered"),
        release: barrier_dir.join("release"),
    };
    let barrier_value = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let metrics = barrier_dir.join("rename-budget.json");
    let metrics_value = metrics.display().to_string();
    let completed = barrier_dir.join("worker-completed");
    let completed_value = completed.display().to_string();
    let fifo_state = barrier_dir.join("fifo-state.json");
    let reader_pending = barrier_dir.join("reader-pending.json");
    let fifo_probe_value = format!(
        "{}|{}|{}|{}",
        fifo_state.display(),
        reader_pending.display(),
        barrier.entered.display(),
        barrier.release.display()
    );
    let deadline_expiry_trigger = barrier_dir.join("expire-deadline");
    let deadline_expiry_trigger_value = deadline_expiry_trigger.display().to_string();
    let deadline_expired = barrier_dir.join("deadline-expired");
    let deadline_expired_value = deadline_expired.display().to_string();
    let deadline_value = deadline_ms.map_or_else(String::new, |deadline| deadline.to_string());
    let mut environment_variables = vec![
        (
            "PASCAL_LSP_TEST_RECONCILIATION_WORK_RESULT",
            metrics_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_FILE_WORKER_COMPLETED",
            completed_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_DIAGNOSTIC_WORK_BARRIER",
            barrier_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_MS",
            deadline_value.as_str(),
        ),
        (
            "PASCAL_LSP_TEST_WORKSPACE_FIFO_PROBE",
            fifo_probe_value.as_str(),
        ),
    ];
    if deadline_ms.is_some() {
        environment_variables.push((
            "PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_TRIGGER",
            deadline_expiry_trigger_value.as_str(),
        ));
        environment_variables.push((
            "PASCAL_LSP_TEST_WORKSPACE_NOTIFICATION_DEADLINE_EXPIRED",
            deadline_expired_value.as_str(),
        ));
    }
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        root.path(),
        environment_variables,
    );
    let initialize_id = RequestId::from("budget-rename-init".to_string());
    server.send_request(
        initialize_id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(root.path()),
            "capabilities": {
                "textDocument": {
                    "diagnostic": {
                        "dynamicRegistration": false,
                        "relatedDocumentSupport": true
                    }
                },
                "workspace": {
                    "diagnostics": {"refreshSupport": true},
                    "workspaceEdit": {"documentChanges": true},
                    "fileOperations": {"willRename": true}
                }
            }
        }),
    );
    assert!(server.response(&initialize_id).error.is_none());
    server.send_notification("initialized", json!({}));
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&provider),"languageId":"pascal","version":5,"text":provider_source}}),
    );

    let mut previous_result_id = None;
    for (index, (consumer, source)) in consumers.iter().zip(&consumer_sources).enumerate() {
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri(consumer),"languageId":"pascal","version":1,"text":source}}),
        );
        let pull_id = RequestId::from(format!("budget-rename-pull-before-{index}"));
        server.send_request(
            pull_id.clone(),
            "textDocument/diagnostic",
            json!({"textDocument":{"uri":uri(consumer)},"previousResultId":null}),
        );
        let pull = server.response(&pull_id);
        assert!(pull.error.is_none(), "initial pull failed: {pull:?}");
        let result = pull.result.expect("initial document diagnostics");
        if index == 0 {
            previous_result_id = result["resultId"].as_str().map(ToOwned::to_owned);
            assert!(
                previous_result_id.is_some(),
                "initial pull result ID is required"
            );
        }
    }

    let renamed = root.path().join("Renamed.pas");
    let will_id = RequestId::from("budget-rename-will".to_string());
    server.send_request(
        will_id.clone(),
        "workspace/willRenameFiles",
        json!({"files":[{"oldUri":uri(&provider),"newUri":uri(&renamed)}]}),
    );
    let will = server.response(&will_id);
    assert!(will.error.is_none(), "willRename failed: {will:?}");
    let edit = will.result.expect("staged provider rename edit");
    let provider_updated = apply_workspace_edit_to_source(provider_source, &edit, &uri(&provider));
    let updated_consumers = consumers
        .iter()
        .zip(&consumer_sources)
        .map(|(consumer, source)| apply_workspace_edit_to_source(source, &edit, &uri(consumer)))
        .collect::<Vec<_>>();
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri(&provider),"version":6},"contentChanges":[{"text":provider_updated}]}),
    );
    for (consumer, source) in consumers.iter().zip(&updated_consumers) {
        server.send_notification(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri(consumer),"version":2},"contentChanges":[{"text":source}]}),
        );
    }
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument":{"uri":uri(&provider)}}),
    );
    fs::rename(&provider, &renamed).expect("client-owned first-pair move");
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri(&renamed),"languageId":"pascal","version":1,"text":provider_updated}}),
    );
    // This is stale destination disk content. The exact opened overlay must
    // initially win, but must not remain authoritative after batch fallback.
    write_file(&renamed, &provider_source.replace("TThing", "TStale"));

    let mut files = vec![json!({"oldUri":uri(&provider),"newUri":uri(&renamed)})];
    for index in 0..40 {
        files.push(json!({
            "oldUri": uri(&root.path().join(format!("Unused{index:02}.pas"))),
            "newUri": uri(&root.path().join(format!("Moved{index:02}.pas")))
        }));
    }
    server.send_notification("workspace/didRenameFiles", json!({"files":files}));
    barrier.wait_until_entered();
    let post_event_source = updated_consumers[0].replace(
        "implementation",
        "type TFifoMarker = Integer;\ntype TMarkerUse = TFifoMarker;\nimplementation",
    );
    server.send_notification(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri(&consumers[0]),"version":3},"contentChanges":[{"text":post_event_source}]}),
    );
    let definition_id = RequestId::from("budget-rename-no-stale-destination".to_string());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &post_event_source, "TThing", 0),
    );
    let fifo_definition_id = RequestId::from("budget-rename-fifo-overlay-control".to_string());
    server.send_request(
        fifo_definition_id.clone(),
        "textDocument/definition",
        navigation_params(&consumers[0], &post_event_source, "TFifoMarker", 1),
    );
    let pull_id = RequestId::from("budget-rename-pull-after-fallback".to_string());
    server.send_request(
        pull_id.clone(),
        "textDocument/diagnostic",
        json!({
            "textDocument":{"uri":uri(&consumers[0])},
            "previousResultId":previous_result_id
        }),
    );
    let fifo_queue_deadline = Instant::now() + FALLBACK_WATCHDOG;
    let held_fifo_state = loop {
        let snapshot = fs::read(&fifo_state)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if let Some(snapshot) = snapshot
            && snapshot["queued_count"] == 4
            && snapshot["worker_held"] == true
        {
            break snapshot;
        }
        assert!(
            Instant::now() < fifo_queue_deadline,
            "the four post-event messages must be observably queued behind the held worker"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(held_fifo_state["overflow_occupied"], false);
    assert_eq!(held_fifo_state["reader_pending"], false);
    if deadline_ms.is_none() {
        barrier.release();
    } else {
        request_workspace_notification_deadline_expiry(
            &deadline_expiry_trigger,
            &deadline_expired,
            FALLBACK_WATCHDOG,
        );
        assert!(
            wait_for_file(&completed, FALLBACK_WATCHDOG),
            "cooperative deadline cancellation must complete the worker"
        );
        assert!(
            !barrier.release.exists(),
            "deadline fallback must not require release of the held work barrier"
        );
    }
    assert!(
        wait_for_file(&metrics, FALLBACK_WATCHDOG),
        "rename worker must report reconciliation accounting"
    );
    let metrics: Value = serde_json::from_slice(&fs::read(&metrics).expect("read budget metrics"))
        .expect("parse budget metrics");
    assert_eq!(
        metrics["budget_exceeded"].as_bool().unwrap_or(false),
        deadline_ms.is_none(),
        "fallback must use the work-budget or deadline cause as configured: {metrics}"
    );
    if deadline_ms.is_none() {
        let rename_path_visits = metrics["file_event_path_visits"]
            .as_u64()
            .unwrap_or_default();
        assert!(
            (2..=82).contains(&rename_path_visits) && rename_path_visits.is_multiple_of(2),
            "processed rename pairs must charge their old/new file-event visits, without charging unattempted pairs: {metrics}"
        );
        assert!(
            metrics["diagnostic_record_checks"]
                .as_u64()
                .unwrap_or_default()
                >= 2_048,
            "diagnostic comparison counter must establish the actual overrun: {metrics}"
        );
    }
    // Budget exhaustion and deadline cancellation both discard derived state
    // and reject the transferred overlay; neither fences the workspace.
    let definition = result_locations(server.response(&definition_id));
    assert!(
        definition.is_empty(),
        "fallback must reject the already-transferred B overlay rather than resolve stale B disk: {definition:?}"
    );
    let fifo_definition = result_locations(server.response(&fifo_definition_id));
    assert_eq!(
        fifo_definition.len(),
        1,
        "deferred didChange must replay before query"
    );
    assert_eq!(fifo_definition[0]["uri"], uri(&consumers[0]).to_string());
    let pull = server.response(&pull_id);
    assert!(pull.error.is_none(), "post-fallback pull failed: {pull:?}");
    let result = pull.result.expect("fresh post-fallback pull result");
    assert_eq!(
        result["kind"], "full",
        "old pull result ID must be invalidated: {result}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
fn begin_configuration_preparation(
    server: &mut TestServer,
    settings: Value,
    barrier: &TestBarrier,
) {
    server.send_notification(
        "workspace/didChangeConfiguration",
        json!({"settings": {"pascalLsp": settings.clone()}}),
    );
    let refresh = server
        .request_with_timeout("workspace/configuration", IO_TIMEOUT)
        .expect("runtime configuration refresh");
    server.send(Message::Response(Response::new_ok(
        refresh.id,
        json!([settings]),
    )));
    barrier.wait_until_entered();
}

// Moved from protocol/cache_baseline.rs: feature-gated metrics endpoint tests
// run in this opt-in target with the other test-support cases.
#[cfg(feature = "test-support")]
fn metrics_counters(server: &mut TestServer, id: &str) -> Value {
    let id = RequestId::from(id.to_string());
    server.send_request(id.clone(), "pascal/testAnalysisMetrics", json!({}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let result = response.result.expect("metrics result");
    assert_eq!(result["schema"], 1);
    assert_eq!(result["countScope"], "process_window_endpoint");
    assert!(result["orderlyShutdown"].is_null());
    result["counters"].clone()
}

#[cfg(feature = "test-support")]
#[test]
fn metrics_test_snapshot_is_monotonic_across_worker_requests() {
    let (temp, main, provider, _main_source, provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root").to_path_buf();
    let mut server = TestServer::launch_with_analysis_metrics(tempfile::tempdir().unwrap());
    server.initialize(&root, Value::Null);

    let before = metrics_counters(&mut server, "metrics-before");
    let references_id = RequestId::from("metrics-references".to_string());
    server.send_request(
        references_id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&provider)},
            "position": position_of(&provider_source, "PublicRoutine", 0),
            "context": {"includeDeclaration": false}
        }),
    );
    let references = server.response(&references_id);
    assert!(references.error.is_none(), "{references:?}");
    let after = metrics_counters(&mut server, "metrics-after");

    // Independent process-wide atomics: this is a window, not an exclusive
    // per-request count, and may include background work.
    let before = before.as_object().expect("before counters");
    for (name, value) in after.as_object().expect("after counters") {
        assert!(
            value.as_u64().unwrap() >= before[name].as_u64().unwrap(),
            "{name} regressed"
        );
    }
    assert!(
        after["coreParserInvocations"].as_u64().unwrap()
            > before["coreParserInvocations"].as_u64().unwrap(),
        "references must have parsed at least one document: {before:?} -> {after:?}"
    );
    server.shutdown();
    drop(temp);
}

#[cfg(feature = "test-support")]
#[test]
fn metrics_test_endpoint_is_rejected_after_shutdown() {
    let (temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root").to_path_buf();
    let mut server = TestServer::launch_with_analysis_metrics(tempfile::tempdir().unwrap());
    server.initialize(&root, Value::Null);

    let shutdown_id = RequestId::from("metrics-shutdown".to_string());
    server.send_request(shutdown_id.clone(), "shutdown", Value::Null);
    let shutdown = server.response(&shutdown_id);
    assert!(shutdown.error.is_none(), "{shutdown:?}");

    let id = RequestId::from("metrics-after-shutdown".to_string());
    server.send_request(id.clone(), "pascal/testAnalysisMetrics", json!({}));
    let response = server.response(&id);
    assert_eq!(
        response.error.expect("post-shutdown rejection").code,
        -32600,
        "{:?}",
        response.result
    );

    server.send_notification("exit", Value::Null);
    server.stdin.take();
    let status = server.child.wait().expect("wait for LSP server");
    assert!(
        status.success(),
        "LSP server exited unsuccessfully: {status}"
    );
    drop(temp);
}
