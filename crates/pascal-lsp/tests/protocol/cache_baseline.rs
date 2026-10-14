use super::*;
use std::io;
use std::process::{Command, Stdio};

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
