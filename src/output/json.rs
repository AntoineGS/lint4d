use crate::engine::Diagnostic;
use serde::Serialize;

#[derive(Serialize)]
struct JsonOutput<'a> {
    version: u32,
    files: Vec<JsonFileOutput<'a>>,
}

#[derive(Serialize)]
struct JsonFileOutput<'a> {
    file: &'a str,
    diagnostics: &'a [Diagnostic],
}

pub fn format_json_output(file_diagnostics: &[(String, Vec<Diagnostic>)]) -> String {
    let files: Vec<JsonFileOutput> = file_diagnostics
        .iter()
        .map(|(path, diags)| JsonFileOutput {
            file: path,
            diagnostics: diags,
        })
        .collect();

    let output = JsonOutput { version: 1, files };
    serde_json::to_string_pretty(&output).unwrap()
}
