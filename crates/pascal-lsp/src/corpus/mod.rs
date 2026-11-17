use std::path::{Path, PathBuf};

/// Severity parsed from dcc or emitted by pascal-lsp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let open = line.rfind('(')?;
    let path = &line[..open];
    let (line_text, rest) = line[open + 1..].split_once(") ")?;
    let line_number = line_text.parse().ok()?;
    let (severity_text, rest) = take_token(rest)?;
    let severity = Severity::parse(severity_text)?;
    let (code, rest) = take_token(rest)?;
    let (source, text) = take_token(rest)?;
    if !valid_message_code(code) || source.is_empty() {
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
}

fn take_token(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace)?;
    let token = &text[..end];
    let rest = text[end..].trim_start();
    Some((token, rest))
}

fn valid_message_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 5
        && matches!(bytes[0], b'E' | b'W' | b'H' | b'F')
        && bytes[1..].iter().all(u8::is_ascii_digit)
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
        DccMessage, LspFinding, Manifest, ManifestProject, Severity, format_lsp_finding,
        linux_path_to_wine, parse_dcc_output, parse_lsp_findings, parse_manifest,
        wine_path_to_linux,
    };
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
}
