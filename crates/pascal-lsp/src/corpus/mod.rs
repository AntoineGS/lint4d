use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Severity parsed from dcc or emitted by pascal-lsp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
    Hint,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DccMessage {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub severity: Severity,
    pub code: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspFinding {
    pub path: PathBuf,
    pub line: u32,
    pub severity: Severity,
    pub code: String,
    pub source: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeCounts {
    pub dcc: usize,
    pub lint4d: usize,
    pub matched: usize,
    pub invented: usize,
    pub missed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrictionCategory {
    pub category: String,
    pub count: usize,
    pub example: LspFinding,
}

pub type NonDccKey = (String, String, Severity);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub per_code: BTreeMap<String, CodeCounts>,
    pub invented: Vec<LspFinding>,
    pub non_dcc: BTreeMap<NonDccKey, usize>,
    pub non_dcc_samples: BTreeMap<NonDccKey, Vec<LspFinding>>,
    pub error_severity_on_clean_files: Vec<LspFinding>,
    pub include_located: Vec<DccMessage>,
    pub unanalysed_files: Vec<PathBuf>,
    pub index_timed_out: bool,
    pub friction: Vec<FrictionCategory>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub projects: Vec<ManifestProject>,
    pub initialization_options: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestProject {
    pub name: String,
    pub dproj: PathBuf,
    pub config: Option<String>,
    pub platform: Option<String>,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Hint => "Hint",
            Self::Fatal => "Fatal",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "Error" => Some(Self::Error),
            "Warning" => Some(Self::Warning),
            "Hint" => Some(Self::Hint),
            "Fatal" => Some(Self::Fatal),
            _ => None,
        }
    }
}

pub fn parse_dcc_output(
    raw: &str,
    wine_to_linux: &dyn Fn(&str) -> Option<PathBuf>,
) -> (Vec<DccMessage>, Vec<String>) {
    let mut messages = Vec::new();
    let mut skipped = Vec::new();

    for line in raw.lines() {
        let Some((path, line_number, severity, code, text)) = parse_dcc_line(line) else {
            skipped.push(line.to_owned());
            continue;
        };
        let Some(path) = wine_to_linux(path) else {
            skipped.push(line.to_owned());
            continue;
        };
        messages.push(DccMessage {
            path,
            line: Some(line_number),
            severity,
            code: code.to_owned(),
            text: text.to_owned(),
        });
    }

    (messages, skipped)
}

fn parse_dcc_line(line: &str) -> Option<(&str, u32, Severity, &str, &str)> {
    let mut offset = 0;
    while let Some(relative_open) = line[offset..].find('(') {
        let open = offset + relative_open;
        offset = open + 1;

        let path = &line[..open];
        let suffix = &line[open + 1..];
        let Some((line_number, message)) = suffix.split_once(") ") else {
            continue;
        };
        let Ok(line_number) = line_number.parse::<u32>() else {
            continue;
        };

        let Some((severity_text, message)) = message.split_once(": ") else {
            continue;
        };
        let Some(severity) = Severity::parse(severity_text) else {
            continue;
        };
        let Some((code, text)) = message.split_once(' ') else {
            continue;
        };
        let code_bytes = code.as_bytes();
        if code_bytes.len() != 5
            || !matches!(code_bytes[0], b'E' | b'W' | b'H' | b'F')
            || !code_bytes[1..].iter().all(u8::is_ascii_digit)
        {
            continue;
        }

        return Some((path, line_number, severity, code, text));
    }

    None
}

pub fn wine_path_to_linux(path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].eq_ignore_ascii_case(&b'Z')
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }

    let suffix = path[3..].replace('\\', "/");
    Some(PathBuf::from(format!("/{suffix}")))
}

pub fn linux_path_to_wine(path: &Path) -> String {
    let path = path.to_string_lossy().replace('/', "\\");
    format!("Z:{path}")
}

pub fn format_lsp_finding(finding: &LspFinding) -> String {
    format!(
        "{}({}) {} {} {} {}",
        finding.path.display(),
        finding.line,
        finding.severity.as_str(),
        finding.code,
        finding.source,
        finding.text
    )
}

pub fn parse_lsp_findings(text: &str) -> Vec<LspFinding> {
    text.lines().filter_map(parse_lsp_line).collect()
}

fn parse_lsp_line(line: &str) -> Option<LspFinding> {
    line.match_indices('(').find_map(|(open, _)| {
        let path = &line[..open];
        let (line_text, rest) = line[open + 1..].split_once(") ")?;
        if line_text.is_empty() || !line_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let line_number = line_text.parse().ok()?;
        let (severity_text, rest) = take_token(rest)?;
        let severity = Severity::parse(severity_text)?;
        let (code, rest) = take_token(rest)?;
        let (source, text) = take_token_with_optional_text(rest)?;
        if code.is_empty() || source.is_empty() {
            return None;
        }

        Some(LspFinding {
            path: PathBuf::from(path),
            line: line_number,
            severity,
            code: code.to_owned(),
            source: source.to_owned(),
            text: text.to_owned(),
        })
    })
}

fn take_token(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace)?;
    let token = &text[..end];
    let rest = text[end..].trim_start();
    Some((token, rest))
}

fn take_token_with_optional_text(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if let Some(end) = text.find(char::is_whitespace) {
        Some((&text[..end], text[end..].trim_start()))
    } else if text.is_empty() {
        None
    } else {
        Some((text, ""))
    }
}

fn valid_message_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 5
        && matches!(bytes[0], b'E' | b'W' | b'H' | b'F')
        && bytes[1..].iter().all(u8::is_ascii_digit)
}

/// Compare compiler and language-server findings for source files owned by a
/// project. Include-file findings are retained separately because their
/// locations are not necessarily open Pascal documents in the editor.
pub fn compare(
    dcc: &[DccMessage],
    lsp: &[LspFinding],
    project_root: &Path,
    dcc_failed_files: &HashSet<PathBuf>,
) -> Report {
    let project_root = normalize_path(project_root);
    let failed_files = dcc_failed_files
        .iter()
        .map(|path| normalize_path(path))
        .collect::<HashSet<_>>();
    let mut report = Report::default();
    let mut dcc_errors = HashSet::new();
    let mut dcc_occurrences = HashMap::<(PathBuf, u32, String), usize>::new();

    for message in dcc {
        let path = normalize_path(&message.path);
        if is_include_file(&path) {
            report.include_located.push(message.clone());
            continue;
        }
        if failed_files.contains(&path) {
            continue;
        }
        if !path.starts_with(&project_root) || !is_pascal_source(&path) {
            continue;
        }
        if matches!(message.severity, Severity::Error | Severity::Fatal) {
            dcc_errors.insert(path.clone());
        }
        let Some(line) = message.line else {
            continue;
        };
        let counts = report.per_code.entry(message.code.clone()).or_default();
        counts.dcc += 1;
        *dcc_occurrences
            .entry((path, line, message.code.clone()))
            .or_default() += 1;
    }

    for finding in lsp {
        let path = normalize_path(&finding.path);
        if failed_files.contains(&path)
            || !path.starts_with(&project_root)
            || !is_pascal_source(&path)
        {
            continue;
        }

        if valid_message_code(&finding.code) {
            let counts = report.per_code.entry(finding.code.clone()).or_default();
            counts.lint4d += 1;
            let key = (path.clone(), finding.line, finding.code.clone());
            match dcc_occurrences.get_mut(&key) {
                Some(remaining) if *remaining > 0 => {
                    *remaining -= 1;
                    counts.matched += 1;
                }
                _ => {
                    counts.invented += 1;
                    report.invented.push(finding.clone());
                }
            }
        } else {
            let key = (
                finding.source.clone(),
                finding.code.clone(),
                finding.severity,
            );
            *report.non_dcc.entry(key.clone()).or_default() += 1;
            let samples = report.non_dcc_samples.entry(key).or_default();
            if samples.len() < 3 {
                samples.push(finding.clone());
            }
        }

        if finding.severity == Severity::Error && !dcc_errors.contains(&path) {
            report.error_severity_on_clean_files.push(finding.clone());
        }
    }

    for counts in report.per_code.values_mut() {
        counts.missed = counts.dcc.saturating_sub(counts.matched);
    }
    report.invented.sort_by(compare_findings);
    report
        .error_severity_on_clean_files
        .sort_by(compare_findings);
    report.include_located.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.code.cmp(&right.code))
    });
    report.friction = friction_categories(&report);
    report
}

fn compare_findings(left: &LspFinding, right: &LspFinding) -> std::cmp::Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.code.cmp(&right.code))
        .then_with(|| left.source.cmp(&right.source))
}

fn normalize_path(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    });
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn is_include_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "inc" | "inl"))
}

fn is_pascal_source(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "pas" | "dpr" | "dpk"
            )
        })
}

fn friction_categories(report: &Report) -> Vec<FrictionCategory> {
    let mut categories = BTreeMap::<String, (usize, LspFinding)>::new();
    for finding in &report.invented {
        let category = format!("dcc-coded {}", finding.code);
        categories
            .entry(category)
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, finding.clone()));
    }
    for ((source, code, severity), samples) in &report.non_dcc_samples {
        let count = report
            .non_dcc
            .get(&(source.clone(), code.clone(), *severity))
            .copied()
            .unwrap_or(0);
        if let Some(example) = samples.first() {
            let category = format!("non-dcc {source}/{code} ({})", severity.as_str());
            categories
                .entry(category)
                .and_modify(|(category_count, _)| *category_count += count)
                .or_insert((count, example.clone()));
        }
    }

    let mut friction = categories
        .into_iter()
        .map(|(category, (count, example))| FrictionCategory {
            category,
            count,
            example,
        })
        .collect::<Vec<_>>();
    friction.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.category.cmp(&right.category))
    });
    friction
}

/// Render a report without exposing diagnostic message text or absolute
/// project paths; each example contains only a basename, line, and code.
pub fn render_markdown(report: &Report) -> String {
    let mut markdown = String::from("# Corpus comparison\n\n");
    markdown.push_str("## Per-code parity\n\n");
    if report.index_timed_out {
        markdown.push_str("Parity counts unavailable: index timeout.\n");
        markdown.push_str("\n## Excluded\n\n- index timeout\n");
    } else {
        markdown.push_str(
            "| Code | DCC | lint4d | Matched | Invented | Missed | Precision | Recall |\n",
        );
        markdown.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        if report.per_code.is_empty() {
            markdown.push_str("| _none_ | 0 | 0 | 0 | 0 | 0 | — | — |\n");
        } else {
            for (code, counts) in &report.per_code {
                markdown.push_str(&format!(
                    "| {code} | {} | {} | {} | {} | {} | {} | {} |\n",
                    counts.dcc,
                    counts.lint4d,
                    counts.matched,
                    counts.invented,
                    counts.missed,
                    format_percentage(counts.matched, counts.lint4d),
                    format_percentage(counts.matched, counts.dcc)
                ));
            }
        }
    }

    markdown.push_str("\n## Friction\n\n");
    if report.index_timed_out {
        markdown.push_str("Unavailable: index timeout.\n");
    } else {
        markdown.push_str("| Category | Count | Example |\n");
        markdown.push_str("| --- | ---: | --- |\n");
        if report.friction.is_empty() {
            markdown.push_str("| _none_ | 0 | — |\n");
        } else {
            for category in &report.friction {
                markdown.push_str(&format!(
                    "| {} | {} | {} |\n",
                    category.category,
                    category.count,
                    finding_example(&category.example)
                ));
            }
        }
    }

    markdown.push_str("\n## Non-DCC diagnostics\n\n");
    if report.index_timed_out {
        markdown.push_str("Unavailable: index timeout.\n");
    } else {
        markdown.push_str("| Source | Code | Severity | Count | Sample |\n");
        markdown.push_str("| --- | --- | --- | ---: | --- |\n");
        if report.non_dcc.is_empty() {
            markdown.push_str("| _none_ | — | — | 0 | — |\n");
        } else {
            for ((source, code, severity), count) in &report.non_dcc {
                let sample = report
                    .non_dcc_samples
                    .get(&(source.clone(), code.clone(), *severity))
                    .and_then(|findings| findings.first())
                    .map(finding_example)
                    .unwrap_or_else(|| "—".to_string());
                markdown.push_str(&format!(
                    "| {source} | {code} | {} | {count} | {sample} |\n",
                    severity.as_str()
                ));
            }
        }
    }

    markdown.push_str("\n## Error severity on DCC-clean files\n\n");
    if report.index_timed_out {
        markdown.push_str("Unavailable: index timeout.\n");
    } else if report.error_severity_on_clean_files.is_empty() {
        markdown.push_str("None.\n");
    } else {
        for finding in &report.error_severity_on_clean_files {
            markdown.push_str(&format!("- {}\n", finding_example(finding)));
        }
    }

    markdown.push_str("\n## Include-located DCC messages\n\n");
    if report.include_located.is_empty() {
        markdown.push_str("None.\n");
    } else {
        for message in &report.include_located {
            markdown.push_str(&format!(
                "- {}:{} {}\n",
                file_basename(&message.path),
                message
                    .line
                    .map_or_else(|| "?".to_string(), |line| line.to_string()),
                message.code
            ));
        }
    }

    markdown.push_str("\n## Index settle\n\n");
    markdown.push_str(if report.index_timed_out {
        "Timed out before the index became idle.\n"
    } else {
        "Index reached idle.\n"
    });

    markdown.push_str("\n## Unanalysed files\n\n");
    if report.unanalysed_files.is_empty() {
        markdown.push_str("None reported.\n");
    } else {
        for path in &report.unanalysed_files {
            markdown.push_str(&format!("- {}\n", file_basename(path)));
        }
    }
    markdown
}

fn format_percentage(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        return "—".to_string();
    }
    let percentage = numerator as f64 / denominator as f64 * 100.0;
    format!("{percentage:.1}%")
}

fn finding_example(finding: &LspFinding) -> String {
    format!(
        "{}:{} {}",
        file_basename(&finding.path),
        finding.line,
        finding.code
    )
}

fn file_basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string())
}

#[derive(Default)]
struct PartialProject {
    name: Option<String>,
    dproj: Option<PathBuf>,
    config: Option<String>,
    platform: Option<String>,
}

#[derive(Clone, Copy)]
enum ManifestSection {
    Root,
    Project(usize),
    Lsp,
}

pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let mut projects = Vec::<PartialProject>::new();
    let mut initialization_options = None;
    let mut saw_initialization_options = false;
    let mut saw_lsp_table = false;
    let mut section = ManifestSection::Root;

    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_manifest_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        match line {
            "[[project]]" => {
                projects.push(PartialProject::default());
                section = ManifestSection::Project(projects.len() - 1);
                continue;
            }
            "[lsp]" => {
                if saw_lsp_table {
                    return Err(format!("line {line_number}: duplicate [lsp] table"));
                }
                saw_lsp_table = true;
                section = ManifestSection::Lsp;
                continue;
            }
            _ if line.starts_with('[') => {
                return Err(format!("line {line_number}: unsupported table {line:?}"));
            }
            _ => {}
        }

        let (key, raw_value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_number}: expected key = string"))?;
        let key = key.trim();
        let value = parse_manifest_string(raw_value.trim())
            .map_err(|error| format!("line {line_number}: {error}"))?;

        match section {
            ManifestSection::Root => {
                return Err(format!(
                    "line {line_number}: key {key:?} must be inside a table"
                ));
            }
            ManifestSection::Project(project_index) => {
                let project = &mut projects[project_index];
                let slot = match key {
                    "name" => &mut project.name,
                    "dproj" => {
                        if project.dproj.replace(PathBuf::from(value)).is_some() {
                            return Err(format!(
                                "line {line_number}: duplicate project key {key:?}"
                            ));
                        }
                        continue;
                    }
                    "config" => &mut project.config,
                    "platform" => &mut project.platform,
                    _ => {
                        return Err(format!(
                            "line {line_number}: unsupported project key {key:?}"
                        ));
                    }
                };
                if slot.replace(value).is_some() {
                    return Err(format!("line {line_number}: duplicate project key {key:?}"));
                }
            }
            ManifestSection::Lsp => {
                if key != "initialization_options_json" {
                    return Err(format!("line {line_number}: unsupported lsp key {key:?}"));
                }
                if saw_initialization_options {
                    return Err(format!("line {line_number}: duplicate lsp key {key:?}"));
                }
                saw_initialization_options = true;
                initialization_options = Some(serde_json::from_str(&value).map_err(|error| {
                    format!("line {line_number}: invalid initialization_options_json: {error}")
                })?);
            }
        }
    }

    if projects.is_empty() {
        return Err("manifest must contain at least one [[project]] table".to_string());
    }

    let projects = projects
        .into_iter()
        .enumerate()
        .map(|(index, project)| {
            let name = project
                .name
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| format!("project {} is missing name", index + 1))?;
            let dproj = project
                .dproj
                .filter(|path| !path.as_os_str().is_empty())
                .ok_or_else(|| format!("project {name:?} is missing dproj"))?;
            Ok(ManifestProject {
                name,
                dproj,
                config: project.config,
                platform: project.platform,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    Ok(Manifest {
        projects,
        initialization_options,
    })
}

fn parse_manifest_string(value: &str) -> Result<String, String> {
    if value.starts_with('"') {
        return serde_json::from_str(value)
            .map_err(|error| format!("expected a quoted string: {error}"));
    }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Ok(value[1..value.len() - 1].to_owned());
    }
    Err("expected a single- or double-quoted string".to_string())
}

fn strip_manifest_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('"'), '\\') => escaped = true,
            (Some(current), ch) if current == ch => quote = None,
            (None, '"' | '\'') => quote = Some(character),
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{
        CodeCounts, DccMessage, LspFinding, Manifest, ManifestProject, Report, Severity, compare,
        format_lsp_finding, linux_path_to_wine, parse_dcc_output, parse_lsp_findings,
        parse_manifest, render_markdown, wine_path_to_linux,
    };
    use std::collections::BTreeMap;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    #[test]
    fn parses_dcc_warnings_hints_errors_and_fatals() {
        let raw = concat!(
            r"Z:\home\u\p\Unit1.pas(12) Warning: W1036 Variable 'X' might not have been initialized",
            "\n",
            r"Z:\home\u\p\Unit1.pas(3) Hint: H2164 Variable 'Y' is declared but never used",
            "\n",
            r"Z:\home\u\p\Unit1.pas(7) Error: E2003 Undeclared identifier: 'Q'",
            "\n",
            r"Unit1.pas(9) Fatal: F2613 Unit 'Foo' not found.",
        );
        let expected_path = PathBuf::from("/home/u/p/Unit1.pas");
        let (messages, skipped) = parse_dcc_output(raw, &|path| {
            if path.starts_with("Z:") || path.starts_with("z:") {
                wine_path_to_linux(path)
            } else {
                Some(PathBuf::from("/home/u/p").join(path))
            }
        });

        assert!(skipped.is_empty());
        assert_eq!(
            messages,
            vec![
                DccMessage {
                    path: expected_path.clone(),
                    line: Some(12),
                    severity: Severity::Warning,
                    code: "W1036".into(),
                    text: "Variable 'X' might not have been initialized".into(),
                },
                DccMessage {
                    path: expected_path.clone(),
                    line: Some(3),
                    severity: Severity::Hint,
                    code: "H2164".into(),
                    text: "Variable 'Y' is declared but never used".into(),
                },
                DccMessage {
                    path: expected_path.clone(),
                    line: Some(7),
                    severity: Severity::Error,
                    code: "E2003".into(),
                    text: "Undeclared identifier: 'Q'".into(),
                },
                DccMessage {
                    path: PathBuf::from("/home/u/p/Unit1.pas"),
                    line: Some(9),
                    severity: Severity::Fatal,
                    code: "F2613".into(),
                    text: "Unit 'Foo' not found.".into(),
                },
            ]
        );
    }

    #[test]
    fn returns_unlocated_and_banner_lines_as_skipped() {
        let raw = concat!(
            "Compiler Speed Pack 7.00",
            "\n",
            "Fatal: no source file specified",
            "\n",
            r"Z:\home\u\p\Unit1.pas(12) Warning: W1036 Warning text",
        );
        let (messages, skipped) = parse_dcc_output(raw, &wine_path_to_linux);

        assert_eq!(messages.len(), 1);
        assert_eq!(
            skipped,
            vec![
                "Compiler Speed Pack 7.00".to_string(),
                "Fatal: no source file specified".to_string()
            ]
        );
    }

    #[test]
    fn parses_parentheses_in_dcc_paths_and_message_text() {
        let raw = r"Z:\home\u\p\Unit(1).pas(12) Warning: W1036 Variable 'X' might not have been initialized (see docs)";
        let (messages, skipped) = parse_dcc_output(raw, &wine_path_to_linux);

        assert!(skipped.is_empty());
        assert_eq!(
            messages,
            vec![DccMessage {
                path: PathBuf::from("/home/u/p/Unit(1).pas"),
                line: Some(12),
                severity: Severity::Warning,
                code: "W1036".into(),
                text: "Variable 'X' might not have been initialized (see docs)".into(),
            }]
        );
    }

    #[test]
    fn wine_paths_round_trip_with_spaces_and_mixed_case_drive_letter() {
        let linux = PathBuf::from("/home/Case Sensitive/Project/Unit1.pas");
        let wine = linux_path_to_wine(&linux);

        assert_eq!(wine, r"Z:\home\Case Sensitive\Project\Unit1.pas");
        assert_eq!(
            wine_path_to_linux(r"z:\home\Case Sensitive\Project\Unit1.pas"),
            Some(linux)
        );
        assert_eq!(wine_path_to_linux(r"C:\Delphi\Unit1.pas"), None);
    }

    #[test]
    fn formats_and_parses_lsp_findings() {
        let finding = LspFinding {
            path: PathBuf::from("/home/Case Sensitive/Project/Unit1.pas"),
            line: 12,
            severity: Severity::Warning,
            code: "W1036".into(),
            source: "pascal-lsp".into(),
            text: "Variable 'X' might not have been initialized".into(),
        };
        let formatted = format_lsp_finding(&finding);

        assert_eq!(
            formatted,
            "/home/Case Sensitive/Project/Unit1.pas(12) Warning W1036 pascal-lsp Variable 'X' might not have been initialized"
        );
        assert_eq!(parse_lsp_findings(&formatted), vec![finding]);
    }

    #[test]
    fn parses_lsp_finding_with_parentheses_in_path_and_text() {
        let finding = LspFinding {
            path: PathBuf::from("/home/Case (old)/Project/Unit(1).pas"),
            line: 12,
            severity: Severity::Warning,
            code: "W1036".into(),
            source: "pascal-lsp".into(),
            text: "Variable 'X' might not be initialized (see docs)".into(),
        };
        let formatted = format_lsp_finding(&finding);

        assert_eq!(parse_lsp_findings(&formatted), vec![finding]);
    }

    #[test]
    fn parses_manifest_projects_and_lsp_initialization_options() {
        let text = r#"
[[project]]
name = "legacy"
dproj = "/src/Legacy/Legacy.dproj"
config = "Release"
platform = "Win32"

[[project]]
name = "modern"
dproj = "/src/Modern/Modern.dproj"

[lsp]
initialization_options_json = '{"sourcePaths":["C:/DelphiSources/rtl"]}'
"#;
        let parsed = parse_manifest(text).expect("manifest should parse");

        assert_eq!(
            parsed,
            Manifest {
                projects: vec![
                    ManifestProject {
                        name: "legacy".into(),
                        dproj: PathBuf::from("/src/Legacy/Legacy.dproj"),
                        config: Some("Release".into()),
                        platform: Some("Win32".into()),
                    },
                    ManifestProject {
                        name: "modern".into(),
                        dproj: PathBuf::from("/src/Modern/Modern.dproj"),
                        config: None,
                        platform: None,
                    },
                ],
                initialization_options: Some(serde_json::json!({
                    "sourcePaths": ["C:/DelphiSources/rtl"]
                })),
            }
        );
    }

    #[test]
    fn rejects_manifest_project_missing_dproj() {
        let error = parse_manifest("[[project]]\nname = \"missing-path\"\n")
            .expect_err("dproj is required");

        assert!(error.contains("dproj"), "unexpected error: {error}");
    }

    #[test]
    fn ignores_only_messages_that_cannot_be_mapped_to_a_location() {
        let (messages, skipped) = parse_dcc_output(
            r"C:\Delphi\Unit1.pas(12) Error: E2003 Undeclared identifier",
            &wine_path_to_linux,
        );

        assert!(messages.is_empty());
        assert_eq!(
            skipped,
            vec![r"C:\Delphi\Unit1.pas(12) Error: E2003 Undeclared identifier".to_string()]
        );
    }

    #[test]
    fn lsp_parser_rejects_non_findings_without_panicking() {
        assert!(parse_lsp_findings("not a finding\n").is_empty());
        assert!(parse_lsp_findings("/tmp/Unit.pas(x) Warning W1036 src text").is_empty());
        assert_eq!(
            wine_path_to_linux(r"Z:\home\x\A.pas").as_deref(),
            Some(Path::new("/home/x/A.pas"))
        );
    }

    #[test]
    fn lsp_parser_keeps_non_dcc_diagnostic_codes_and_sources() {
        let parsed = parse_lsp_findings(
            "/corpus/Unit.pas(4) Error parse-error tree-sitter unexpected token",
        );

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].code, "parse-error");
        assert_eq!(parsed[0].source, "tree-sitter");
        assert_eq!(parsed[0].text, "unexpected token");
    }

    #[test]
    fn compares_dcc_findings_and_separates_non_dcc_include_and_failed_files() {
        let root = Path::new("/corpus/project");
        let dcc = vec![
            DccMessage {
                path: root.join("Unit.pas"),
                line: Some(3),
                severity: Severity::Warning,
                code: "W1036".into(),
                text: "not initialized".into(),
            },
            DccMessage {
                path: root.join("Unit.pas"),
                line: Some(7),
                severity: Severity::Hint,
                code: "H2164".into(),
                text: "unused".into(),
            },
            DccMessage {
                path: root.join("Types.inc"),
                line: Some(5),
                severity: Severity::Error,
                code: "E2003".into(),
                text: "include error".into(),
            },
            DccMessage {
                path: root.join("Broken.pas"),
                line: Some(9),
                severity: Severity::Error,
                code: "E2003".into(),
                text: "compile error".into(),
            },
        ];
        let lsp = vec![
            LspFinding {
                path: root.join("Unit.pas"),
                line: 3,
                severity: Severity::Warning,
                code: "W1036".into(),
                source: "pascal-lsp".into(),
                text: "not initialized".into(),
            },
            LspFinding {
                path: root.join("Unit.pas"),
                line: 11,
                severity: Severity::Warning,
                code: "W1036".into(),
                source: "pascal-lsp".into(),
                text: "invented warning".into(),
            },
            LspFinding {
                path: root.join("Clean.pas"),
                line: 2,
                severity: Severity::Error,
                code: "parse-error".into(),
                source: "tree-sitter".into(),
                text: "unexpected token".into(),
            },
            LspFinding {
                path: root.join("Broken.pas"),
                line: 9,
                severity: Severity::Error,
                code: "E2003".into(),
                source: "pascal-lsp".into(),
                text: "compile error".into(),
            },
        ];

        let report = compare(
            &dcc,
            &lsp,
            root,
            &HashSet::from([root.join("Broken.pas"), root.join("Types.inc")]),
        );

        assert_eq!(
            report.per_code["W1036"],
            CodeCounts {
                dcc: 1,
                lint4d: 2,
                matched: 1,
                invented: 1,
                missed: 0,
            }
        );
        assert_eq!(report.invented.len(), 1);
        assert_eq!(report.invented[0].line, 11);
        assert_eq!(
            report.per_code["H2164"],
            CodeCounts {
                dcc: 1,
                lint4d: 0,
                matched: 0,
                invented: 0,
                missed: 1,
            }
        );
        assert_eq!(report.non_dcc.len(), 1);
        assert_eq!(report.error_severity_on_clean_files, vec![lsp[2].clone()]);
        assert_eq!(report.include_located, vec![dcc[2].clone()]);
        assert!(
            !report
                .invented
                .iter()
                .any(|finding| finding.path.ends_with("Broken.pas"))
        );
        assert!(!report.per_code.contains_key("E2003"));
    }

    #[test]
    fn renders_comparison_table_and_friction_examples() {
        let root = Path::new("/corpus/project");
        let dcc = vec![DccMessage {
            path: root.join("Unit.pas"),
            line: Some(3),
            severity: Severity::Warning,
            code: "W1036".into(),
            text: "warning".into(),
        }];
        let lsp = vec![LspFinding {
            path: root.join("Unit.pas"),
            line: 8,
            severity: Severity::Warning,
            code: "W1036".into(),
            source: "pascal-lsp".into(),
            text: "invented warning".into(),
        }];

        let report = compare(&dcc, &lsp, root, &HashSet::new());
        let markdown = render_markdown(&report);

        assert!(markdown.contains(
            "| Code | DCC | lint4d | Matched | Invented | Missed | Precision | Recall |"
        ));
        assert!(markdown.contains("## Friction"));
        assert!(markdown.contains("Unit.pas:8"));
        assert!(markdown.contains("W1036"));
    }

    #[test]
    fn renders_precision_and_recall_and_handles_zero_denominators() {
        let report = Report {
            per_code: BTreeMap::from([
                (
                    "W1001".into(),
                    CodeCounts {
                        dcc: 4,
                        lint4d: 5,
                        matched: 3,
                        invented: 2,
                        missed: 1,
                    },
                ),
                (
                    "W1002".into(),
                    CodeCounts {
                        dcc: 2,
                        lint4d: 0,
                        matched: 0,
                        invented: 0,
                        missed: 2,
                    },
                ),
                (
                    "W1003".into(),
                    CodeCounts {
                        dcc: 0,
                        lint4d: 2,
                        matched: 0,
                        invented: 2,
                        missed: 0,
                    },
                ),
            ]),
            ..Report::default()
        };

        let markdown = render_markdown(&report);

        assert!(markdown.contains(
            "| Code | DCC | lint4d | Matched | Invented | Missed | Precision | Recall |"
        ));
        assert!(markdown.contains("| W1001 | 4 | 5 | 3 | 2 | 1 | 60.0% | 75.0% |"));
        assert!(markdown.contains("| W1002 | 2 | 0 | 0 | 0 | 2 | — | 0.0% |"));
        assert!(markdown.contains("| W1003 | 0 | 2 | 0 | 2 | 0 | 0.0% | — |"));
    }

    #[test]
    fn index_timeout_marks_parity_unavailable_and_excludes_partial_counts() {
        let report = Report {
            per_code: BTreeMap::from([(
                "W9999".into(),
                CodeCounts {
                    dcc: 1,
                    lint4d: 1,
                    matched: 1,
                    invented: 0,
                    missed: 0,
                },
            )]),
            index_timed_out: true,
            ..Report::default()
        };

        let markdown = render_markdown(&report);

        assert!(markdown.contains("Parity counts unavailable: index timeout."));
        assert!(markdown.contains("## Excluded\n\n- index timeout"));
        assert!(!markdown.contains("| Code | DCC |"));
        assert!(!markdown.contains("W9999"));
    }
}
