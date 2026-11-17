use pascal_lsp::corpus::{
    DccMessage, Severity, compare, parse_dcc_output, parse_lsp_findings, render_markdown,
    wine_path_to_linux,
};
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage:\n  corpus_compare <command.json> <dcc.raw> <lsp.out> <report.md> [--meta <lsp.meta.json>]\n";

struct Arguments {
    command_json: PathBuf,
    dcc_raw: PathBuf,
    lsp_output: PathBuf,
    report: PathBuf,
    meta: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("corpus_compare: {error}\n\n{USAGE}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let arguments = parse_arguments(&env::args().skip(1).collect::<Vec<_>>())?;
    let command = read_json(&arguments.command_json)?;
    let project_root = command
        .get("project_root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| {
            command
                .get("main_source")
                .and_then(Value::as_str)
                .and_then(|source| Path::new(source).parent().map(Path::to_path_buf))
        })
        .ok_or_else(|| "command JSON has no project_root or main_source".to_string())?;
    let dcc_base = command
        .get("main_source")
        .and_then(Value::as_str)
        .and_then(|source| Path::new(source).parent())
        .unwrap_or(&project_root);

    let raw_dcc = fs::read_to_string(&arguments.dcc_raw)
        .map_err(|error| format!("could not read DCC output: {error}"))?;
    let (dcc, skipped_dcc_lines) =
        parse_dcc_output(&raw_dcc, &|path| resolve_dcc_path(path, dcc_base));
    let lsp_text = fs::read_to_string(&arguments.lsp_output)
        .map_err(|error| format!("could not read LSP findings: {error}"))?;
    let lsp = parse_lsp_findings(&lsp_text);
    let dcc_failed_files = failed_files(&dcc);
    let mut report = compare(&dcc, &lsp, &project_root, &dcc_failed_files);
    if let Some(meta_path) = &arguments.meta
        && meta_path.is_file()
    {
        let meta = read_json(meta_path)?;
        report.index_timed_out = meta
            .get("index_timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        report.unanalysed_files = meta
            .get("unanalysed_files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect();
        report.unanalysed_files.sort();
        report.unanalysed_files.dedup();
    }
    if report.index_timed_out {
        report.per_code.clear();
        report.invented.clear();
        report.non_dcc.clear();
        report.non_dcc_samples.clear();
        report.error_severity_on_clean_files.clear();
        report.friction.clear();
    }

    let markdown = render_markdown(&report);
    if let Some(parent) = arguments
        .report
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create report directory: {error}"))?;
    }
    fs::write(&arguments.report, markdown)
        .map_err(|error| format!("could not write comparison report: {error}"))?;
    let dcc_count = report
        .per_code
        .values()
        .map(|counts| counts.dcc)
        .sum::<usize>();
    let lint_count = report
        .per_code
        .values()
        .map(|counts| counts.lint4d)
        .sum::<usize>();
    if report.index_timed_out {
        eprintln!("parity counts unavailable: project excluded because index timed out");
    } else {
        eprintln!(
            "compared {} DCC finding(s) with {} dcc-coded lint4d finding(s); {} invented, {} missed, {} non-DCC",
            dcc_count,
            lint_count,
            report.invented.len(),
            report
                .per_code
                .values()
                .map(|counts| counts.missed)
                .sum::<usize>(),
            report.non_dcc.values().sum::<usize>()
        );
    }
    if !skipped_dcc_lines.is_empty() {
        eprintln!(
            "ignored {} unlocated or unmapped DCC output line(s)",
            skipped_dcc_lines.len()
        );
    }
    Ok(())
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    if args == ["--help"] || args == ["-h"] {
        print!("{USAGE}");
        std::process::exit(0);
    }
    if args.len() < 4 {
        return Err("expected command JSON, DCC output, LSP output, and report paths".to_string());
    }
    let mut parsed = Arguments {
        command_json: PathBuf::from(&args[0]),
        dcc_raw: PathBuf::from(&args[1]),
        lsp_output: PathBuf::from(&args[2]),
        report: PathBuf::from(&args[3]),
        meta: None,
    };
    let mut index = 4;
    while index < args.len() {
        match args[index].as_str() {
            "--meta" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "--meta requires a JSON file".to_string())?;
                if parsed.meta.replace(PathBuf::from(path)).is_some() {
                    return Err("--meta may be specified only once".to_string());
                }
                index += 2;
            }
            option => return Err(format!("unknown option {option:?}")),
        }
    }
    Ok(parsed)
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))
}

fn failed_files(dcc: &[DccMessage]) -> HashSet<PathBuf> {
    dcc.iter()
        .filter(|message| matches!(message.severity, Severity::Error | Severity::Fatal))
        .map(|message| message.path.clone())
        .collect()
}

fn resolve_dcc_path(path: &str, dcc_base: &Path) -> Option<PathBuf> {
    if let Some(path) = wine_path_to_linux(path) {
        return Some(path);
    }

    let bytes = path.as_bytes();
    if (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with("\\\\")
        || path.starts_with("//")
    {
        return None;
    }

    let path = PathBuf::from(path.replace('\\', "/"));
    if path.is_absolute() {
        Some(path)
    } else {
        Some(dcc_base.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_dcc_path;
    use pascal_lsp::corpus::{LspFinding, Severity, compare, parse_dcc_output};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    #[test]
    fn compares_nested_relative_windows_dcc_paths_to_absolute_lsp_paths() {
        let root = Path::new("/corpus/project");
        let (dcc, skipped) = parse_dcc_output(
            r"src\Nested\Unit.pas(8) Warning: W1036 variable may be uninitialized",
            &|path| resolve_dcc_path(path, root),
        );
        let lsp = [LspFinding {
            path: root.join("src/Nested/Unit.pas"),
            line: 8,
            severity: Severity::Warning,
            code: "W1036".into(),
            source: "pascal-lsp".into(),
            text: "variable may be uninitialized".into(),
        }];

        let report = compare(&dcc, &lsp, root, &HashSet::new());

        assert!(skipped.is_empty());
        assert_eq!(report.per_code["W1036"].matched, 1);
        assert_eq!(report.per_code["W1036"].invented, 0);
        assert_eq!(report.per_code["W1036"].missed, 0);
    }

    #[test]
    fn rejects_unmapped_windows_drive_paths() {
        assert_eq!(
            resolve_dcc_path(r"C:\work\Unit.pas", Path::new("/corpus/project")),
            None::<PathBuf>
        );
    }
}
