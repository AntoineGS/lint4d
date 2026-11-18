use lsp_types::Url;
use pascal_lsp::corpus::{
    LspFinding, Severity, ensure_output_outside_roots, format_lsp_finding, protected_output_roots,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const PUSH_QUIET_PERIOD: Duration = Duration::from_millis(300);
const PUSH_QUIET_LIMIT: Duration = Duration::from_secs(2);
const INDEX_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);
const REQUIRED_ZERO_PENDING_POLLS: usize = 3;
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

const USAGE: &str = "Usage:\n  lsp_corpus_diagnostics <pascal-lsp-binary> <command.json> <out-file> [--init-options <json-file>] [--timeout-secs N] [--fingerprint-only]\n";

struct Arguments {
    binary: PathBuf,
    command_json: PathBuf,
    output: PathBuf,
    init_options: Option<PathBuf>,
    timeout: Duration,
    fingerprint_only: bool,
}

#[derive(Clone)]
struct SourceDocument {
    path: PathBuf,
    uri: String,
    text: Option<String>,
    bytes: Vec<u8>,
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
    let command_bytes = fs::read(&arguments.command_json)
        .map_err(|error| format!("could not read command JSON: {error}"))?;
    let command: Value = serde_json::from_slice(&command_bytes)
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
    let initialization_options = load_initialization_options(arguments.init_options.as_deref())?;
    let mut documents = Vec::with_capacity(files.len());
    let mut undecodable_files = HashSet::new();
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
        let text = match decode_pascal_source(&bytes) {
            Ok(text) => Some(text),
            Err(error) => {
                eprintln!(
                    "source {} is undecodable and will not be analysed: {error}",
                    path.display()
                );
                undecodable_files.insert(path.clone());
                None
            }
        };
        documents.push(SourceDocument {
            path,
            uri,
            text,
            bytes,
        });
    }

    let (run_fingerprint, server_binary, server_mtime_ns) = compute_run_fingerprint(
        &arguments.binary,
        &command_bytes,
        &documents,
        &initialization_options,
    )?;
    if arguments.fingerprint_only {
        println!("{run_fingerprint}");
        return Ok(());
    }

    let canonical_project_file = fs::canonicalize(&project_file).map_err(|error| {
        format!(
            "could not resolve project file {}: {error}",
            project_file.display()
        )
    })?;
    let project_directory = canonical_project_file
        .parent()
        .ok_or_else(|| "project file has no parent directory".to_string())?
        .to_path_buf();
    let project_uri = canonical_project_file
        .extension()
        .filter(|extension| extension.eq_ignore_ascii_case("dproj"))
        .map(|_| file_uri(&canonical_project_file))
        .transpose()?;
    let protected_roots = protected_output_roots(&project_directory, Some(&root))?;
    let output_path = ensure_output_outside_roots(&arguments.output, &protected_roots)?;
    let requested_meta_path = arguments.output.with_extension("meta.json");
    let meta_path = ensure_output_outside_roots(&requested_meta_path, &protected_roots)?;

    let analysable_count = documents
        .iter()
        .filter(|document| document.text.is_some())
        .count();
    if analysable_count == 0 {
        let undecodable_count = undecodable_files.len();
        fs::write(&output_path, b"")
            .map_err(|error| format!("could not create output file: {error}"))?;
        let meta = json!({
            "index_timed_out": false,
            "index_status": null,
            "index_settle": null,
            "project_selection": null,
            "pull_diagnostics": false,
            "files_opened": 0,
            "diagnostics": 0,
            "run_fingerprint": run_fingerprint,
            "server_binary": server_binary,
            "server_mtime_ns": server_mtime_ns,
            "undecodable_files": sorted_paths(undecodable_files.clone()),
            "unanalysed_files": sorted_paths(undecodable_files)
        });
        fs::write(
            &meta_path,
            serde_json::to_vec_pretty(&meta)
                .map_err(|error| format!("could not encode metadata: {error}"))?,
        )
        .map_err(|error| format!("could not write metadata {}: {error}", meta_path.display()))?;
        eprintln!(
            "opened 0 file(s), recorded 0 diagnostic(s), {} unanalysed file(s)",
            undecodable_count
        );
        return Ok(());
    }

    let mut client = Client::spawn(&arguments.binary)?;
    let (pull_supported, project_selection) = initialize_and_open_documents(
        &mut client,
        &file_uri(&root)?,
        &initialization_options,
        project_uri.as_deref(),
        &documents,
        arguments.timeout,
    )?;

    let (index_status, index_timed_out, zero_pending_polls) =
        client.wait_for_index(arguments.timeout, analysable_count > 0)?;
    let mut unanalysed_files = undecodable_files;
    if index_timed_out {
        unanalysed_files.extend(documents.iter().map(|document| document.path.clone()));
    } else {
        if pull_supported {
            for document in documents.iter().filter(|document| document.text.is_some()) {
                let result = client.request(
                    "textDocument/diagnostic",
                    json!({
                        "textDocument": { "uri": document.uri },
                        "previousResultId": null
                    }),
                    arguments.timeout,
                );
                match result {
                    Ok(result) => {
                        if !client.record_pull_diagnostics(&document.uri, &result) {
                            unanalysed_files.insert(document.path.clone());
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "diagnostic pull failed for {}: {error}",
                            document
                                .path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        );
                        unanalysed_files.insert(document.path.clone());
                    }
                }
            }
        } else {
            let deadline = Instant::now() + arguments.timeout;
            let expected = documents
                .iter()
                .filter(|document| document.text.is_some())
                .map(|document| document.uri.clone())
                .collect::<HashSet<_>>();
            while !expected.is_subset(&client.pushed_uris) && Instant::now() < deadline {
                if !client.receive_one(deadline.saturating_duration_since(Instant::now()))? {
                    break;
                }
            }
            for document in documents.iter().filter(|document| document.text.is_some()) {
                if !client.pushed_uris.contains(&document.uri) {
                    unanalysed_files.insert(document.path.clone());
                }
            }
        }
        client.drain_pushes();
    }

    let mut output = Vec::new();
    for document in documents.iter().filter(|document| document.text.is_some()) {
        if index_timed_out {
            continue;
        }
        let Some(diagnostics) = client.latest_diagnostics.get(&document.uri) else {
            unanalysed_files.insert(document.path.clone());
            continue;
        };
        for diagnostic in diagnostics {
            if let Some(finding) = diagnostic_finding(&document.path, diagnostic) {
                output.push(format_lsp_finding(&finding));
            }
        }
    }
    output.sort();

    let mut output_file = fs::File::create(&output_path)
        .map_err(|error| format!("could not create output file: {error}"))?;
    for line in &output {
        writeln!(output_file, "{line}")
            .map_err(|error| format!("could not write output file: {error}"))?;
    }

    let unanalysed_count = unanalysed_files.len();
    let meta = json!({
        "index_timed_out": index_timed_out,
        "index_status": index_status,
        "index_settle": index_settle_metadata(&index_status, zero_pending_polls),
        "project_selection": project_selection,
        "pull_diagnostics": pull_supported,
        "files_opened": analysable_count,
        "diagnostics": output.len(),
        "run_fingerprint": run_fingerprint,
        "server_binary": server_binary,
        "server_mtime_ns": server_mtime_ns,
        "undecodable_files": sorted_paths(undecodable_paths(&documents)),
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
        return Err("index timeout; parity diagnostics are unavailable".to_string());
    }
    eprintln!(
        "opened {} file(s), recorded {} diagnostic(s), {} unanalysed file(s)",
        analysable_count,
        output.len(),
        unanalysed_count
    );
    Ok(())
}

fn initialize_capabilities() -> Value {
    json!({
        "textDocument": {
            "diagnostic": {
                "dynamicRegistration": false,
                "relatedDocumentSupport": true
            },
            "publishDiagnostics": {}
        },
        "workspace": {
            "diagnostics": { "refreshSupport": true },
            "workspaceFolders": true
        }
    })
}

fn decode_pascal_source(bytes: &[u8]) -> Result<String, String> {
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16_source(bytes, true);
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16_source(bytes, false);
    }
    Ok(pascal_core::decode_bytes(bytes).into_owned())
}

fn decode_utf16_source(bytes: &[u8], little_endian: bool) -> Result<String, String> {
    if !bytes.len().is_multiple_of(2) {
        return Err("UTF-16 byte sequence has an odd length".to_string());
    }
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes(*chunk)
            } else {
                u16::from_be_bytes(*chunk)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| "UTF-16 contains an unpaired surrogate".to_string())
}

trait CorpusProtocol {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String>;

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String>;
}

impl CorpusProtocol for Client {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        Client::request(self, method, params, timeout)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        Client::notify(self, method, params)
    }
}

fn initialize_and_open_documents(
    client: &mut impl CorpusProtocol,
    root_uri: &str,
    initialization_options: &Value,
    project_uri: Option<&str>,
    documents: &[SourceDocument],
    timeout: Duration,
) -> Result<(bool, Value), String> {
    let initialize_result = client.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": root_uri,
            "initializationOptions": initialization_options,
            "capabilities": initialize_capabilities()
        }),
        timeout,
    )?;
    let pull_supported = initialize_result
        .pointer("/capabilities/diagnosticProvider")
        .is_some_and(|provider| !provider.is_null() && provider != &Value::Bool(false));

    client.notify("initialized", json!({}))?;

    let scope = documents
        .iter()
        .find(|document| document.text.is_some())
        .ok_or_else(|| "corpus command has no decodable source documents".to_string())?;
    let selection_result = client.request(
        "pascal/selectProject",
        json!({
            "textDocument": { "uri": scope.uri },
            "projectUri": project_uri
        }),
        timeout,
    )?;
    let project_selection = project_selection_metadata(selection_result, &scope.uri, project_uri)?;

    for document in documents.iter().filter(|document| document.text.is_some()) {
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": document.uri,
                    "languageId": "pascal",
                    "version": 1,
                    "text": document
                        .text
                        .as_deref()
                        .expect("only decodable source documents are opened")
                }
            }),
        )?;
    }
    Ok((pull_supported, project_selection))
}

fn project_selection_metadata(
    response: Value,
    scope_uri: &str,
    requested_project_uri: Option<&str>,
) -> Result<Value, String> {
    let mode = response
        .get("selectionMode")
        .and_then(Value::as_str)
        .ok_or_else(|| "pascal/selectProject returned no selectionMode".to_string())?;
    let selected_project_uri = response.get("selectedProjectUri").unwrap_or(&Value::Null);
    if mode == "invalid" {
        return Err("pascal/selectProject returned an invalid project selection".to_string());
    }
    if let Some(requested_uri) = requested_project_uri {
        let selected_uri = selected_project_uri.as_str().ok_or_else(|| {
            format!("pascal/selectProject did not select the requested project {requested_uri}")
        })?;
        let requested = Url::parse(requested_uri)
            .map_err(|error| format!("invalid requested project URI: {error}"))?;
        let selected = Url::parse(selected_uri)
            .map_err(|error| format!("invalid selected project URI: {error}"))?;
        if selected != requested {
            return Err(format!(
                "pascal/selectProject did not select the requested project {requested_uri}; selected {selected_uri}"
            ));
        }
    }

    Ok(json!({
        "scopeUri": scope_uri,
        "requestedProjectUri": requested_project_uri,
        "selectedProjectUri": selected_project_uri,
        "selectionMode": mode,
        "response": response
    }))
}

fn load_initialization_options(path: Option<&Path>) -> Result<Value, String> {
    match path {
        Some(path) => serde_json::from_slice(&fs::read(path).map_err(|error| {
            format!(
                "could not read initialization options {}: {error}",
                path.display()
            )
        })?)
        .map_err(|error| format!("invalid initialization options JSON: {error}")),
        None => Ok(default_initialization_options()),
    }
}

fn compute_run_fingerprint(
    binary: &Path,
    command_json: &[u8],
    documents: &[SourceDocument],
    initialization_options: &Value,
) -> Result<(String, String, u128), String> {
    let server_binary = fs::canonicalize(binary)
        .map_err(|error| format!("could not resolve pascal-lsp binary: {error}"))?;
    let binary_metadata = fs::metadata(&server_binary)
        .map_err(|error| format!("could not inspect pascal-lsp binary: {error}"))?;
    let modified = binary_metadata
        .modified()
        .map_err(|error| format!("could not read pascal-lsp binary mtime: {error}"))?;
    let server_mtime_ns = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut hasher = blake3::Hasher::new();
    update_fingerprint(&mut hasher, "version", b"lsp-corpus-run-v1");
    update_fingerprint(&mut hasher, "command.json", command_json);
    let options = serde_json::to_vec(initialization_options)
        .map_err(|error| format!("could not encode initialization options: {error}"))?;
    update_fingerprint(&mut hasher, "initialization-options", &options);
    update_fingerprint(
        &mut hasher,
        "server-binary-path",
        server_binary.to_string_lossy().as_bytes(),
    );
    update_fingerprint(
        &mut hasher,
        "server-binary-mtime-ns",
        server_mtime_ns.to_string().as_bytes(),
    );

    let mut sorted_documents = documents
        .iter()
        .map(|document| {
            fs::canonicalize(&document.path)
                .map(|path| (path, document.bytes.as_slice()))
                .map_err(|error| {
                    format!(
                        "could not resolve project file {} for fingerprint: {error}",
                        document.path.display()
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    sorted_documents.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, bytes) in sorted_documents {
        update_fingerprint(
            &mut hasher,
            "project-file-path",
            path.to_string_lossy().as_bytes(),
        );
        update_fingerprint(&mut hasher, "project-file-content", bytes);
    }

    Ok((
        hasher.finalize().to_hex().to_string(),
        server_binary.to_string_lossy().into_owned(),
        server_mtime_ns,
    ))
}

fn update_fingerprint(hasher: &mut blake3::Hasher, label: &str, bytes: &[u8]) {
    hasher.update(&(label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
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
    let mut fingerprint_only = false;
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
            "--fingerprint-only" => {
                if fingerprint_only {
                    return Err("--fingerprint-only may be specified only once".to_string());
                }
                fingerprint_only = true;
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
        fingerprint_only,
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

fn undecodable_paths(documents: &[SourceDocument]) -> HashSet<PathBuf> {
    documents
        .iter()
        .filter(|document| document.text.is_none())
        .map(|document| document.path.clone())
        .collect()
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
    ) -> Result<(Value, bool, usize), String> {
        let deadline = Instant::now() + timeout;
        let mut tracker = IndexSettleTracker::default();
        let mut last_status = Value::Null;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok((last_status, true, tracker.zero_pending_polls()));
            }
            let status = match self.request("pascal/indexStatus", json!({}), remaining) {
                Ok(status) => status,
                Err(error) if error == "timed out waiting for pascal-lsp" => {
                    return Ok((last_status, true, tracker.zero_pending_polls()));
                }
                Err(error) => return Err(error),
            };
            let observed_at = Instant::now();
            let settled = tracker.observe(&status, observed_at, require_context);
            last_status = status;
            if settled && observed_at < deadline {
                return Ok((last_status, false, tracker.zero_pending_polls()));
            }
            let pause = deadline
                .saturating_duration_since(Instant::now())
                .min(INDEX_STATUS_POLL_INTERVAL);
            if pause.is_zero() {
                return Ok((last_status, true, tracker.zero_pending_polls()));
            }
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

#[derive(Debug, Default)]
struct IndexSettleTracker {
    zero_pending_polls: usize,
    last_zero_pending_poll: Option<Instant>,
}

impl IndexSettleTracker {
    fn observe(&mut self, status: &Value, observed_at: Instant, require_context: bool) -> bool {
        if !index_status_pending_zero(status, require_context) {
            self.zero_pending_polls = 0;
            self.last_zero_pending_poll = None;
            return false;
        }

        self.zero_pending_polls = match self.last_zero_pending_poll {
            Some(last_poll)
                if observed_at.saturating_duration_since(last_poll)
                    >= INDEX_STATUS_POLL_INTERVAL =>
            {
                self.zero_pending_polls.saturating_add(1)
            }
            _ => 1,
        };
        self.last_zero_pending_poll = Some(observed_at);
        self.zero_pending_polls >= REQUIRED_ZERO_PENDING_POLLS
    }

    fn zero_pending_polls(&self) -> usize {
        self.zero_pending_polls
    }
}

fn index_status_pending_zero(status: &Value, require_context: bool) -> bool {
    let Some(contexts) = status.get("contexts").and_then(Value::as_array) else {
        return false;
    };
    (!require_context || !contexts.is_empty())
        && contexts
            .iter()
            .all(|context| context.get("pending").and_then(Value::as_u64) == Some(0))
}

fn index_settle_metadata(status: &Value, zero_pending_polls: usize) -> Value {
    let contexts = status.get("contexts").and_then(Value::as_array);
    let context_count = contexts.map(Vec::len);
    let pending = contexts.and_then(|contexts| {
        contexts.iter().try_fold(0_u64, |total, context| {
            total.checked_add(context.get("pending")?.as_u64()?)
        })
    });
    let all_contexts_bool = |key: &str| {
        contexts.and_then(|contexts| {
            (!contexts.is_empty()).then(|| {
                contexts.iter().try_fold(true, |all, context| {
                    context.get(key)?.as_bool().map(|value| all && value)
                })
            })?
        })
    };

    json!({
        "context_count": context_count,
        "pending": pending,
        "complete": all_contexts_bool("complete"),
        "membershipComplete": all_contexts_bool("membershipComplete"),
        "consecutive_zero_pending_polls": zero_pending_polls,
        "required_zero_pending_polls": REQUIRED_ZERO_PENDING_POLLS
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
    use super::{
        CorpusProtocol, IndexSettleTracker, SourceDocument, compute_run_fingerprint,
        diagnostic_finding, index_settle_metadata, index_status_pending_zero,
        initialize_and_open_documents, initialize_capabilities, load_initialization_options,
    };
    use pascal_lsp::corpus::{ensure_output_outside_roots, protected_output_roots};
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    struct RecordingProtocol {
        events: Vec<String>,
        request_params: HashMap<String, serde_json::Value>,
        selection_response: serde_json::Value,
    }

    impl CorpusProtocol for RecordingProtocol {
        fn request(
            &mut self,
            method: &str,
            params: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            self.events.push(format!("request:{method}"));
            self.request_params.insert(method.to_owned(), params);
            Ok(match method {
                "initialize" => json!({
                    "capabilities": { "diagnosticProvider": {} }
                }),
                "pascal/selectProject" => self.selection_response.clone(),
                _ => serde_json::Value::Null,
            })
        }

        fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), String> {
            self.events.push(format!("notify:{method}"));
            self.request_params.insert(method.to_owned(), params);
            Ok(())
        }
    }

    fn source_document(path: &Path) -> SourceDocument {
        SourceDocument {
            path: path.to_path_buf(),
            uri: super::file_uri(path).expect("document URI"),
            text: Some("unit Unit;".to_string()),
            bytes: b"unit Unit;".to_vec(),
        }
    }

    #[test]
    fn corpus_open_documents_decode_legacy_eight_bit_source_without_replacements() {
        let source = b"unit Legacy; // caf\xe9\n";
        assert_eq!(
            super::decode_pascal_source(source).expect("legacy source decodes"),
            "unit Legacy; // café\n"
        );
    }

    #[test]
    fn corpus_decoder_does_not_silently_drop_an_odd_utf16_byte() {
        assert!(
            super::decode_pascal_source(&[0xff, 0xfe, b'A', 0, b'B']).is_err(),
            "odd UTF-16 input must be rejected instead of dropping its trailing byte"
        );
    }

    #[test]
    fn corpus_decoder_rejects_unpaired_utf16_surrogates() {
        for source in [
            &[0xff, 0xfe, 0x00, 0xd8][..],
            &[0xfe, 0xff, 0xd8, 0x00][..],
            &[0xff, 0xfe, 0x00, 0xdc][..],
        ] {
            assert!(
                super::decode_pascal_source(source).is_err(),
                "malformed UTF-16 must be rejected rather than analysed"
            );
        }
    }

    #[test]
    fn undecodable_utf16_is_recorded_and_not_opened_for_analysis() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let invalid_path = temp.path().join("Invalid.pas");
        let invalid_bytes = vec![0xff, 0xfe, 0x00, 0xd8];
        let invalid_document = SourceDocument {
            path: invalid_path.clone(),
            uri: super::file_uri(&invalid_path).expect("invalid document URI"),
            text: super::decode_pascal_source(&invalid_bytes).ok(),
            bytes: invalid_bytes,
        };
        let valid_path = temp.path().join("Valid.pas");
        let valid_document = source_document(&valid_path);
        let documents = [invalid_document, valid_document.clone()];

        assert_eq!(
            super::undecodable_paths(&documents),
            std::collections::HashSet::from([invalid_path])
        );

        let mut protocol = RecordingProtocol {
            events: Vec::new(),
            request_params: HashMap::new(),
            selection_response: json!({
                "scopeUri": "file:///tmp",
                "selectedProjectUri": null,
                "selectionMode": "directory",
                "warnings": []
            }),
        };
        super::initialize_and_open_documents(
            &mut protocol,
            "file:///tmp",
            &json!({}),
            None,
            &documents,
            Duration::from_secs(5),
        )
        .expect("open only decodable source");

        assert_eq!(
            protocol.events,
            [
                "request:initialize",
                "notify:initialized",
                "request:pascal/selectProject",
                "notify:textDocument/didOpen"
            ]
        );
        assert_eq!(
            protocol.request_params["textDocument/didOpen"]["textDocument"]["uri"],
            valid_document.uri
        );
        assert_eq!(
            protocol.request_params["textDocument/didOpen"]["textDocument"]["text"],
            "unit Unit;"
        );
    }

    #[test]
    fn corpus_startup_selects_manifest_project_after_initialize_before_opening_documents() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let document_path = temp.path().join("Unit.pas");
        let document = source_document(&document_path);
        let project_uri = super::file_uri(&temp.path().join("App.dproj")).expect("project URI");
        let mut protocol = RecordingProtocol {
            events: Vec::new(),
            request_params: HashMap::new(),
            selection_response: json!({
                "scopeUri": "file:///tmp",
                "selectedProjectUri": project_uri,
                "selectionMode": "directory",
                "warnings": []
            }),
        };

        let (pull_supported, selection) = initialize_and_open_documents(
            &mut protocol,
            "file:///tmp",
            &json!({}),
            Some(&project_uri),
            std::slice::from_ref(&document),
            Duration::from_secs(5),
        )
        .expect("initialize, select project, and open source");

        assert!(pull_supported);
        assert_eq!(
            protocol.events,
            [
                "request:initialize",
                "notify:initialized",
                "request:pascal/selectProject",
                "notify:textDocument/didOpen"
            ]
        );
        assert_eq!(
            protocol.request_params["pascal/selectProject"],
            json!({
                "textDocument": { "uri": document.uri },
                "projectUri": project_uri
            })
        );
        assert_eq!(selection["requestedProjectUri"], project_uri);
        assert_eq!(selection["selectedProjectUri"], project_uri);
        assert_eq!(selection["selectionMode"], "directory");
    }

    #[test]
    fn corpus_startup_fails_closed_when_manifest_project_selection_is_rejected() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let document = source_document(&temp.path().join("Unit.pas"));
        let project_uri = super::file_uri(&temp.path().join("App.dproj")).expect("project URI");
        let mut protocol = RecordingProtocol {
            events: Vec::new(),
            request_params: HashMap::new(),
            selection_response: json!({
                "scopeUri": "file:///tmp",
                "selectedProjectUri": "file:///tmp/Other.dproj",
                "selectionMode": "ambiguous",
                "warnings": ["requested project is not selected"]
            }),
        };

        let error = initialize_and_open_documents(
            &mut protocol,
            "file:///tmp",
            &json!({}),
            Some(&project_uri),
            std::slice::from_ref(&document),
            Duration::from_secs(5),
        )
        .expect_err("rejected project selection must fail the corpus run");

        assert!(
            error.contains("did not select the requested project"),
            "{error}"
        );
        assert!(
            !protocol
                .events
                .iter()
                .any(|event| event == "notify:textDocument/didOpen"),
            "documents must not be opened after project selection is rejected"
        );
    }

    #[test]
    fn zero_pending_is_settled_even_when_context_membership_is_incomplete() {
        assert!(index_status_pending_zero(
            &json!({
                "contexts": [{
                    "complete": false,
                    "membershipComplete": false,
                    "pending": 0
                }]
            }),
            true
        ));
        assert!(!index_status_pending_zero(
            &json!({"contexts": [{"complete": true, "pending": 1}]}),
            true
        ));
        assert!(!index_status_pending_zero(&json!({"contexts": []}), true));
        assert!(index_status_pending_zero(
            &json!({"contexts": [{"complete": true, "pending": 0}]}),
            true
        ));
    }

    #[test]
    fn index_settle_requires_three_spaced_zero_pending_samples_and_resets_on_work() {
        let zero_pending_incomplete = json!({
            "contexts": [{
                "complete": false,
                "membershipComplete": false,
                "pending": 0
            }]
        });
        let pending_work = json!({
            "contexts": [{
                "complete": false,
                "membershipComplete": false,
                "pending": 1
            }]
        });
        let start = Instant::now();
        let mut tracker = IndexSettleTracker::default();

        assert!(!tracker.observe(&zero_pending_incomplete, start, true));
        assert!(!tracker.observe(
            &zero_pending_incomplete,
            start + Duration::from_secs(2),
            true
        ));
        assert!(tracker.observe(
            &zero_pending_incomplete,
            start + Duration::from_secs(4),
            true
        ));

        let mut tracker = IndexSettleTracker::default();
        assert!(!tracker.observe(&zero_pending_incomplete, start, true));
        assert!(!tracker.observe(
            &zero_pending_incomplete,
            start + Duration::from_secs(2),
            true
        ));
        assert!(!tracker.observe(&pending_work, start + Duration::from_secs(4), true));
        assert!(!tracker.observe(
            &zero_pending_incomplete,
            start + Duration::from_secs(6),
            true
        ));
        assert!(!tracker.observe(
            &zero_pending_incomplete,
            start + Duration::from_secs(8),
            true
        ));
        assert!(tracker.observe(
            &zero_pending_incomplete,
            start + Duration::from_secs(10),
            true
        ));

        let mut tracker = IndexSettleTracker::default();
        for poll in 0..4 {
            assert!(!tracker.observe(&pending_work, start + Duration::from_secs(poll * 2), true));
        }
    }

    #[test]
    fn index_settle_metadata_preserves_incomplete_server_status_fields() {
        let status = json!({
            "contexts": [{
                "pending": 0,
                "complete": false,
                "membershipComplete": false
            }]
        });

        assert_eq!(
            index_settle_metadata(&status, 3),
            json!({
                "context_count": 1,
                "pending": 0,
                "complete": false,
                "membershipComplete": false,
                "consecutive_zero_pending_polls": 3,
                "required_zero_pending_polls": 3
            })
        );
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

    #[test]
    fn advertises_publish_diagnostics_capabilities() {
        let capabilities = initialize_capabilities();

        assert!(
            capabilities
                .pointer("/textDocument/publishDiagnostics")
                .is_some()
        );
    }

    #[test]
    fn initialization_options_use_neovim_defaults_only_when_not_supplied() {
        let default = load_initialization_options(None).expect("default options");
        assert_eq!(default["sourcePaths"], json!(super::NEOVIM_SOURCE_PATHS));

        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("init-options.json");
        fs::write(&path, r#"{"sourcePaths":["C:/custom/rtl"]}"#).expect("write options");
        let manifest_options = load_initialization_options(Some(&path)).expect("manifest options");
        assert_eq!(manifest_options, json!({"sourcePaths": ["C:/custom/rtl"]}));
    }

    #[test]
    fn run_fingerprint_covers_source_contents_options_and_server_identity() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = temp.path().join("pascal-lsp");
        let source = temp.path().join("Unit.pas");
        fs::write(&binary, b"server binary").expect("write binary");
        fs::write(&source, b"unit Unit;").expect("write source");
        let document = SourceDocument {
            path: source.clone(),
            uri: "file:///Unit.pas".to_string(),
            text: Some("unit Unit;".to_string()),
            bytes: b"unit Unit;".to_vec(),
        };
        let options = json!({"sourcePaths": ["C:/rtl"]});
        let command = b"{\"files\":[\"Unit.pas\"]}";
        let (original, server_path, mtime_ns) =
            compute_run_fingerprint(&binary, command, std::slice::from_ref(&document), &options)
                .expect("fingerprint");
        assert_eq!(
            server_path,
            fs::canonicalize(&binary).unwrap().display().to_string()
        );
        assert_ne!(mtime_ns, 0);

        let (changed_command, _, _) = compute_run_fingerprint(
            &binary,
            b"{\"files\":[\"Unit.pas\"],\"changed\":true}",
            std::slice::from_ref(&document),
            &options,
        )
        .expect("command fingerprint");
        assert_ne!(original, changed_command);

        let mut changed_document = document.clone();
        changed_document.bytes = b"unit Unit; // changed".to_vec();
        let (changed_source, _, _) = compute_run_fingerprint(
            &binary,
            command,
            std::slice::from_ref(&changed_document),
            &options,
        )
        .expect("source fingerprint");
        assert_ne!(original, changed_source);

        let changed_options = json!({"sourcePaths": ["C:/different"]});
        let (changed_options, _, _) = compute_run_fingerprint(
            &binary,
            command,
            &[SourceDocument {
                path: source,
                uri: "file:///Unit.pas".to_string(),
                text: Some("unit Unit;".to_string()),
                bytes: b"unit Unit;".to_vec(),
            }],
            &changed_options,
        )
        .expect("options fingerprint");
        assert_ne!(original, changed_options);

        let second_binary = temp.path().join("other-pascal-lsp");
        fs::write(&second_binary, b"server binary").expect("write second binary");
        let (changed_server_path, _, _) = compute_run_fingerprint(
            &second_binary,
            command,
            std::slice::from_ref(&document),
            &options,
        )
        .expect("server-path fingerprint");
        assert_ne!(original, changed_server_path);

        let server = fs::OpenOptions::new()
            .write(true)
            .open(&binary)
            .expect("open server binary");
        let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        server
            .set_times(fs::FileTimes::new().set_modified(modified))
            .expect("set server mtime");
        let (changed_server_mtime, _, _) =
            compute_run_fingerprint(&binary, command, &[document], &options)
                .expect("server-mtime fingerprint");
        assert_ne!(original, changed_server_mtime);
    }

    #[test]
    fn diagnostics_output_guard_rejects_direct_and_symlinked_project_paths() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let project_root = temp.path().join("project");
        fs::create_dir_all(&project_root).expect("project root");
        let roots = protected_output_roots(&project_root, None).expect("protected roots");

        let direct = project_root.join("generated/lsp.txt");
        assert!(
            ensure_output_outside_roots(&direct, &roots)
                .unwrap_err()
                .contains("inside a corpus project root")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let alias = temp.path().join("project-alias");
            symlink(&project_root, &alias).expect("project root symlink");
            let indirect = alias.join("generated/lsp.txt");
            assert!(
                ensure_output_outside_roots(&indirect, &roots)
                    .unwrap_err()
                    .contains("inside a corpus project root")
            );
        }
    }
}
