use lsp_types::Url;
use pascal_lsp::corpus::{LspFinding, Severity, format_lsp_finding};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const PUSH_QUIET_PERIOD: Duration = Duration::from_millis(300);
const PUSH_QUIET_LIMIT: Duration = Duration::from_secs(2);
const MAX_LSP_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const NEOVIM_SOURCE_PATHS: &[&str] = &[
    "C:/DelphiSources/rtl/sys",
    "C:/DelphiSources/rtl/common",
    "C:/DelphiSources/rtl/win",
    "C:/DelphiSources/vcl",
    "C:/DelphiSources/db",
    "C:/DelphiSources/db/ado",
    "C:/DelphiSources/db/dsnap",
    "C:/DelphiSources/db/dbx",
    "C:/DelphiSources/db/vclctrls",
    "C:/DelphiSources/IBX",
    "C:/DelphiSources/internet",
    "C:/DelphiSources/soap",
    "C:/DelphiSources/xml",
];

const USAGE: &str = "Usage:\n  lsp_corpus_diagnostics <pascal-lsp-binary> <command.json> <out-file> [--init-options <json-file>] [--timeout-secs N]\n";

struct Arguments {
    binary: PathBuf,
    command_json: PathBuf,
    output: PathBuf,
    init_options: Option<PathBuf>,
    timeout: Duration,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("lsp_corpus_diagnostics: {error}\n\n{USAGE}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let arguments = parse_arguments(&env::args().skip(1).collect::<Vec<_>>())?;
    if !arguments.binary.is_file() {
        return Err(format!(
            "pascal-lsp binary does not exist: {}",
            arguments.binary.display()
        ));
    }
    let command: Value = serde_json::from_slice(
        &fs::read(&arguments.command_json)
            .map_err(|error| format!("could not read command JSON: {error}"))?,
    )
    .map_err(|error| format!("invalid command JSON: {error}"))?;
    let files = string_array(&command, "files")?;
    let project_file = command
        .get("project_file")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| {
            command
                .get("main_source")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .ok_or_else(|| "command JSON has no project_file or main_source".to_string())?;
    let root = command
        .get("workspace_root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root(&project_file));
    let initialization_options = match &arguments.init_options {
        Some(path) => serde_json::from_slice(&fs::read(path).map_err(|error| {
            format!(
                "could not read initialization options {}: {error}",
                path.display()
            )
        })?)
        .map_err(|error| format!("invalid initialization options JSON: {error}"))?,
        None => default_initialization_options(),
    };

    let mut documents = Vec::with_capacity(files.len());
    for file in files {
        let path = PathBuf::from(file);
        let uri = Url::from_file_path(&path)
            .map_err(|()| {
                format!(
                    "could not convert source path to file URI: {}",
                    path.display()
                )
            })?
            .to_string();
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read source {}: {error}", path.display()))?;
        documents.push((path, uri, String::from_utf8_lossy(&bytes).into_owned()));
    }

    let mut client = Client::spawn(&arguments.binary)?;
    let initialize_result = client.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": file_uri(&root)?,
            "initializationOptions": initialization_options,
            "capabilities": {
                "textDocument": {
                    "diagnostic": {
                        "dynamicRegistration": false,
                        "relatedDocumentSupport": true
                    }
                },
                "workspace": {
                    "diagnostics": { "refreshSupport": true },
                    "workspaceFolders": true
                }
            }
        }),
        arguments.timeout,
    )?;
    let pull_supported = initialize_result
        .pointer("/capabilities/diagnosticProvider")
        .is_some_and(|provider| !provider.is_null() && provider != &Value::Bool(false));
    client.notify("initialized", json!({}))?;

    for (_, uri, text) in &documents {
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "pascal",
                    "version": 1,
                    "text": text
                }
            }),
        )?;
    }

    let (index_status, index_timed_out) =
        client.wait_for_index(arguments.timeout, !documents.is_empty())?;
    let mut unanalysed_files = HashSet::new();
    if pull_supported {
        for (path, uri, _) in &documents {
            let result = client.request(
                "textDocument/diagnostic",
                json!({
                    "textDocument": { "uri": uri },
                    "previousResultId": null
                }),
                arguments.timeout,
            );
            match result {
                Ok(result) => {
                    if !client.record_pull_diagnostics(uri, &result) {
                        unanalysed_files.insert(path.clone());
                    }
                }
                Err(error) => {
                    eprintln!(
                        "diagnostic pull failed for {}: {error}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    unanalysed_files.insert(path.clone());
                }
            }
        }
    } else {
        let deadline = Instant::now() + arguments.timeout;
        let expected = documents
            .iter()
            .map(|(_, uri, _)| uri.clone())
            .collect::<HashSet<_>>();
        while !expected.is_subset(&client.pushed_uris) && Instant::now() < deadline {
            if !client.receive_one(deadline.saturating_duration_since(Instant::now()))? {
                break;
            }
        }
        for (path, uri, _) in &documents {
            if !client.pushed_uris.contains(uri) {
                unanalysed_files.insert(path.clone());
            }
        }
    }
    client.drain_pushes();

    let mut output = Vec::new();
    for (path, uri, _) in &documents {
        let Some(diagnostics) = client.latest_diagnostics.get(uri) else {
            unanalysed_files.insert(path.clone());
            continue;
        };
        for diagnostic in diagnostics {
            if let Some(finding) = diagnostic_finding(path, diagnostic) {
                output.push(format_lsp_finding(&finding));
            }
        }
    }
    output.sort();

    let mut output_file = fs::File::create(&arguments.output)
        .map_err(|error| format!("could not create output file: {error}"))?;
    for line in &output {
        writeln!(output_file, "{line}")
            .map_err(|error| format!("could not write output file: {error}"))?;
    }

    let meta_path = arguments.output.with_extension("meta.json");
    let unanalysed_count = unanalysed_files.len();
    let meta = json!({
        "index_timed_out": index_timed_out,
        "index_status": index_status,
        "pull_diagnostics": pull_supported,
        "files_opened": documents.len(),
        "diagnostics": output.len(),
        "unanalysed_files": sorted_paths(unanalysed_files)
    });
    fs::write(
        &meta_path,
        serde_json::to_vec_pretty(&meta)
            .map_err(|error| format!("could not encode metadata: {error}"))?,
    )
    .map_err(|error| format!("could not write metadata {}: {error}", meta_path.display()))?;

    client.shutdown(arguments.timeout)?;
    if index_timed_out {
        eprintln!(
            "pascal/indexStatus did not become idle within {} seconds",
            arguments.timeout.as_secs()
        );
    }
    eprintln!(
        "opened {} file(s), recorded {} diagnostic(s), {} unanalysed file(s)",
        documents.len(),
        output.len(),
        unanalysed_count
    );
    Ok(())
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    if args == ["--help"] || args == ["-h"] {
        print!("{USAGE}");
        std::process::exit(0);
    }
    if args.len() < 3 {
        return Err("expected a binary, command JSON, and output file".to_string());
    }
    let binary = PathBuf::from(&args[0]);
    let command_json = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    let mut init_options = None;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let mut index = 3;
    while index < args.len() {
        let option = args[index].as_str();
        index += 1;
        match option {
            "--init-options" => {
                let value = args
                    .get(index)
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| "--init-options requires a JSON file".to_string())?;
                if init_options.replace(PathBuf::from(value)).is_some() {
                    return Err("--init-options may be specified only once".to_string());
                }
                index += 1;
            }
            "--timeout-secs" => {
                let value = args
                    .get(index)
                    .ok_or_else(|| "--timeout-secs requires a number".to_string())?;
                let seconds = value
                    .parse::<u64>()
                    .map_err(|error| format!("invalid timeout {value:?}: {error}"))?;
                if seconds == 0 {
                    return Err("--timeout-secs must be greater than zero".to_string());
                }
                timeout = Duration::from_secs(seconds);
                index += 1;
            }
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(Arguments {
        binary,
        command_json,
        output,
        init_options,
        timeout,
    })
}

fn string_array(value: &Value, field: &str) -> Result<Vec<String>, String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("command JSON has no {field} array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("command JSON {field} must contain only strings"))
        })
        .collect()
}

fn default_initialization_options() -> Value {
    json!({
        "sourcePaths": NEOVIM_SOURCE_PATHS
    })
}

fn file_uri(path: &Path) -> Result<String, String> {
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| format!("could not convert path to file URI: {}", path.display()))
}

fn workspace_root(project_file: &Path) -> PathBuf {
    let starting_directory = project_file.parent().unwrap_or(project_file);
    starting_directory
        .ancestors()
        .find(|directory| {
            directory.join(".lint4d.toml").is_file() || directory.join(".git").exists()
        })
        .unwrap_or(starting_directory)
        .to_path_buf()
}

fn diagnostic_finding(path: &Path, diagnostic: &Value) -> Option<LspFinding> {
    let line = diagnostic
        .pointer("/range/start/line")
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())?
        .saturating_add(1);
    let severity = match diagnostic.get("severity").and_then(Value::as_u64) {
        Some(1) => Severity::Error,
        Some(2) => Severity::Warning,
        Some(3 | 4) => Severity::Hint,
        _ => Severity::Warning,
    };
    let code = diagnostic
        .get("code")
        .map(|code| match code {
            Value::String(code) => code.clone(),
            other => other.to_string(),
        })
        .filter(|code| !code.is_empty())
        .unwrap_or_else(|| "-".to_string());
    let source = diagnostic
        .get("source")
        .and_then(Value::as_str)
        .filter(|source| !source.is_empty())
        .unwrap_or("-")
        .to_owned();
    let text = diagnostic
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    Some(LspFinding {
        path: path.to_path_buf(),
        line,
        severity,
        code,
        source,
        text,
    })
}

fn sorted_paths(paths: HashSet<PathBuf>) -> Vec<String> {
    let mut paths = paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

struct Client {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    next_id: u64,
    latest_diagnostics: HashMap<String, Vec<Value>>,
    pushed_uris: HashSet<String>,
}

impl Client {
    fn spawn(binary: &Path) -> Result<Self, String> {
        let mut child = Command::new(binary)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "pascal-lsp stdin was not piped".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "pascal-lsp stdout was not piped".to_string())?;
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || read_messages(stdout, sender));
        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            messages,
            next_id: 1,
            latest_diagnostics: HashMap::new(),
            pushed_uris: HashSet::new(),
        })
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + timeout;
        loop {
            let message = self.receive_message(deadline)?;
            if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
                if let Some(error) = message.get("error") {
                    return Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string()));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.process_message(message)?;
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| self.stdin.write_all(&body))
            .and_then(|()| self.stdin.flush())
            .map_err(|error| format!("could not write LSP message: {error}"))
    }

    fn receive_message(&mut self, deadline: Instant) -> Result<Value, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for pascal-lsp".to_string());
        }
        match self.messages.recv_timeout(remaining) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => Err("timed out waiting for pascal-lsp".to_string()),
            Err(RecvTimeoutError::Disconnected) => {
                Err("pascal-lsp closed its output stream".to_string())
            }
        }
    }

    fn process_message(&mut self, message: Value) -> Result<(), String> {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = message.get("params")
                    && let Some(uri) = params.get("uri").and_then(Value::as_str)
                {
                    let diagnostics = params
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    self.latest_diagnostics.insert(uri.to_owned(), diagnostics);
                    self.pushed_uris.insert(uri.to_owned());
                }
            } else if let Some(id) = message.get("id") {
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null
                }))?;
            }
        }
        Ok(())
    }

    fn receive_one(&mut self, timeout: Duration) -> Result<bool, String> {
        match self.messages.recv_timeout(timeout) {
            Ok(Ok(message)) => {
                self.process_message(message)?;
                Ok(true)
            }
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => {
                Err("pascal-lsp closed its output stream".to_string())
            }
        }
    }

    fn wait_for_index(
        &mut self,
        timeout: Duration,
        require_context: bool,
    ) -> Result<(Value, bool), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.request("pascal/indexStatus", json!({}), timeout)?;
            if index_status_idle(&status, require_context) {
                return Ok((status, false));
            }
            if Instant::now() >= deadline {
                return Ok((status, true));
            }
            let pause = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500));
            thread::sleep(pause);
        }
    }

    fn record_pull_diagnostics(&mut self, uri: &str, result: &Value) -> bool {
        if result
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "unchanged")
        {
            return self.latest_diagnostics.contains_key(uri);
        }
        let Some(items) = result.get("items").and_then(Value::as_array) else {
            return false;
        };
        self.latest_diagnostics
            .insert(uri.to_owned(), items.clone());
        true
    }

    fn drain_pushes(&mut self) {
        let deadline = Instant::now() + PUSH_QUIET_LIMIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.messages.recv_timeout(remaining.min(PUSH_QUIET_PERIOD)) {
                Ok(Ok(message)) => {
                    let _ = self.process_message(message);
                }
                Ok(Err(_)) | Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => {
                    break;
                }
            }
        }
    }

    fn shutdown(&mut self, timeout: Duration) -> Result<(), String> {
        let _ = self.request("shutdown", Value::Null, timeout);
        self.notify("exit", Value::Null)?;
        self.stdin.flush().map_err(|error| error.to_string())?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| format!("could not wait for pascal-lsp: {error}"))?
            {
                if !status.success() {
                    return Err(format!("pascal-lsp exited with status {status}"));
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for pascal-lsp to exit".to_string());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn index_status_idle(status: &Value, require_context: bool) -> bool {
    let Some(contexts) = status.get("contexts").and_then(Value::as_array) else {
        return false;
    };
    (!require_context || !contexts.is_empty())
        && contexts.iter().all(|context| {
            context.get("complete").and_then(Value::as_bool) == Some(true)
                && context.get("pending").and_then(Value::as_u64).unwrap_or(0) == 0
        })
}

fn read_messages(stdout: impl Read, sender: Sender<Result<Value, String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_message(&mut reader) {
            Ok(Some(message)) => {
                if sender.send(Ok(message)).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                let _ = sender.send(Err(error.to_string()));
                break;
            }
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return if saw_header {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF inside LSP headers",
                ))
            } else {
                Ok(None)
            };
        }
        saw_header = true;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            content_length =
                Some(value.trim().parse::<usize>().map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?);
        }
    }
    let length = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    if length > MAX_LSP_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP message exceeds the corpus driver's size limit",
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::{diagnostic_finding, index_status_idle};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn index_status_waits_for_context_completion() {
        assert!(!index_status_idle(
            &json!({"contexts": [{"complete": false, "pending": 0}]}),
            true
        ));
        assert!(!index_status_idle(&json!({"contexts": []}), true));
        assert!(index_status_idle(
            &json!({"contexts": [{"complete": true, "pending": 0}]}),
            true
        ));
    }

    #[test]
    fn converts_lsp_diagnostics_to_comparison_findings() {
        let finding = diagnostic_finding(
            Path::new("/corpus/Unit.pas"),
            &json!({
                "range": {"start": {"line": 4}},
                "severity": 1,
                "code": "parse-error",
                "source": "tree-sitter",
                "message": "unexpected token"
            }),
        )
        .expect("diagnostic has a start line");
        assert_eq!(finding.line, 5);
        assert_eq!(finding.code, "parse-error");
        assert_eq!(finding.source, "tree-sitter");
        assert_eq!(finding.text, "unexpected token");
    }
}
