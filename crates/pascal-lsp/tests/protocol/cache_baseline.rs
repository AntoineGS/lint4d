use super::*;

#[test]
fn metrics_private_method_is_not_a_normal_server_feature() {
    let temp = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("ordinary-metrics".to_string());
    server.send_request(id.clone(), "pascal/testAnalysisMetrics", json!({}));
    let response = server.response(&id);
    assert_eq!(response.error.unwrap().code, -32601);
    server.shutdown();
}

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

fn metrics_launch_ordinary(environment: &Path, enabled: bool) -> TestServer {
    let child = Command::new(env!("CARGO_BIN_EXE_pascal-lsp"))
        .arg("--stdio")
        .env("HOME", environment.join("home"))
        .env("XDG_CONFIG_HOME", environment.join("config"))
        .env_remove("PASCAL_LSP_ANALYSIS_METRICS")
        .envs(enabled.then_some(("PASCAL_LSP_ANALYSIS_METRICS", "1")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch pascal-lsp");
    TestServer::from_child(child)
}

fn metrics_finish_and_collect_stderr(mut server: TestServer) -> String {
    server.shutdown();
    // Everything the server wrote to stdout must have been framed JSON-RPC.
    loop {
        match server.messages.recv_timeout(IO_TIMEOUT) {
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => break,
            other => panic!("stdout was not clean framed JSON-RPC: {other:?}"),
        }
    }
    let mut stderr = String::new();
    io::Read::read_to_string(&mut server.child.stderr.take().unwrap(), &mut stderr).unwrap();
    stderr
}

#[test]
fn metrics_shutdown_reports_one_bounded_line_on_stderr_only_when_enabled() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let root = main.parent().expect("workspace root");
    let environment = tempfile::tempdir().unwrap();

    let mut server = metrics_launch_ordinary(environment.path(), true);
    server.initialize(root, Value::Null);
    let stderr = metrics_finish_and_collect_stderr(server);
    let lines = stderr
        .lines()
        .filter(|line| line.starts_with("pascal-lsp analysis-metrics "))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "stderr: {stderr}");
    assert!(lines[0].len() < 4096);
    let report: Value =
        serde_json::from_str(&lines[0]["pascal-lsp analysis-metrics ".len()..]).unwrap();
    assert_eq!(report["schema"], 1);
    assert_eq!(report["countScope"], "process_lifetime");
    assert_eq!(report["orderlyShutdown"], true);

    let mut server = metrics_launch_ordinary(environment.path(), false);
    server.initialize(root, Value::Null);
    let stderr = metrics_finish_and_collect_stderr(server);
    assert!(!stderr.contains("analysis-metrics"), "stderr: {stderr}");
}
