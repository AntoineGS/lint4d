//! dcc-recorded fixtures (`crates/pascal-lsp/tests/dcc_oracle`): for the
//! codes in `ENABLED_CODES`, the semantic pass must report exactly what dcc
//! recorded (precision always; recall unless a case or code has a documented
//! waiver, or a required RTL source is unavailable).
use super::dcc_surface::{SurfaceKey, SurfacePlatform, table_for, verified_system_context};
use super::dcc_switches::{SwitchTable, switch_table};
use super::*;
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{
    CompilerVersion, ConditionalFact, ProjectContext, ProjectOptions, PropertyValue,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// dcc codes whose findings are compared. Grows as each rule lands.
const ENABLED_CODES: &[&str] = &[
    "E1000", "E1001", "E1002", "E1003", "E1004", "E1005", "E1006", "E1007", "E1009", "E1010",
    "E2003", "E2004", "E2037", "E2065", "E2137", "E2170", "E2211", "E2252", "E2254", "E2291",
    "E2301", "H2077", "H2135", "H2164", "H2219", "W1000", "W1001", "W1002", "W1003", "W1004",
    "W1005", "W1006", "W1007", "W1009", "W1010", "W1035", "W1036", "W1037",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    file: String,
    line: usize,
    code: String,
    message: String,
    severity: &'static str,
}

struct Case {
    name: String,
    dir: PathBuf,
    unit: String,
    compilers: Vec<String>,
    config: Option<String>,
    platform: Option<String>,
    requires_rtl: bool,
    recall: bool,
    recall_exempt: Vec<String>,
    silent_codes: Vec<String>,
}

impl Case {
    fn recall_enforced_for(&self, code: &str, rtl_available: bool) -> bool {
        self.recall
            && (!self.requires_rtl || rtl_available)
            && !self
                .recall_exempt
                .iter()
                .any(|exempt| exempt.eq_ignore_ascii_case(code))
    }
}

struct CaseInitialState {
    conditional_context: ConditionalContext,
    project: Option<Arc<ProjectContext>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CodeSummary {
    expected: usize,
    matched: usize,
    invented: usize,
    missed: usize,
    waived: usize,
}

fn cases_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dcc_oracle")
}

fn oracle_context(version: &str) -> ConditionalContext {
    let compiler_version = CompilerVersion::parse(version)
        .unwrap_or_else(|| panic!("unsupported Delphi compiler version {version:?}"));
    let key = SurfaceKey {
        major: compiler_version.major,
        minor: compiler_version.minor,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for(&key)
        .unwrap_or_else(|| panic!("no dcc surface table for compiler {version}-Win32"));
    verified_system_context(&key, &table)
}

fn rtl_root(version: &str) -> Option<PathBuf> {
    let version_var = format!("PASCAL_RTL_{}", version.replace('.', "_"));
    let root = std::env::var_os(&version_var).or_else(|| {
        (version == "37.0")
            .then(|| std::env::var_os("PASCAL_RTL"))
            .flatten()
    })?;
    let root = PathBuf::from(root);
    Some(root.strip_prefix("~").map_or(root.clone(), |suffix| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(suffix)
    }))
}

fn pascal_rtl_system(version: &str) -> Option<(Url, String)> {
    let root = rtl_root(version)?;
    let path = root.join("rtl/sys/System.pas");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("PASCAL_RTL System source {path:?}: {error}"));
    let uri = Url::from_file_path(&path).expect("PASCAL_RTL System source URI");
    Some((uri, source))
}

fn load_case(dir: &Path) -> Case {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let toml = std::fs::read_to_string(dir.join("case.toml"))
        .unwrap_or_else(|error| panic!("{name}: case.toml: {error}"));
    load_case_from_toml(&name, dir, &toml)
}

fn load_case_from_toml(name: &str, dir: &Path, toml: &str) -> Case {
    let value = |key: &str| {
        toml.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
        })
    };
    let recall = value("recall").as_deref() != Some("false");
    let recall_exempt = value("recall_exempt")
        .map(|raw| parse_string_array(&raw, name, "recall_exempt"))
        .unwrap_or_default();
    let reason = value("reason");
    assert!(
        recall || reason.is_some(),
        "{name}: recall = false requires a reason"
    );
    assert!(
        recall_exempt.is_empty() || reason.is_some(),
        "{name}: recall_exempt requires a reason"
    );
    let compilers = value("compilers")
        .map(|raw| parse_string_array(&raw, name, "compilers"))
        .unwrap_or_else(|| vec!["37.0".to_owned()]);
    assert!(!compilers.is_empty(), "{name}: compilers must not be empty");
    let silent_codes = value("silent")
        .map(|raw| parse_string_array(&raw, name, "silent"))
        .unwrap_or_default();
    Case {
        unit: value("unit").unwrap_or_else(|| panic!("{name}: case.toml has no unit")),
        compilers,
        config: value("config"),
        platform: value("platform"),
        requires_rtl: value("requires_rtl").as_deref() == Some("true"),
        recall,
        recall_exempt,
        silent_codes,
        name: name.to_owned(),
        dir: dir.to_path_buf(),
    }
}

fn parse_string_array(raw: &str, case_name: &str, key: &str) -> Vec<String> {
    raw.strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{case_name}: {key} must be an array of strings"))
        .split(',')
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

fn expected_path(case_dir: &Path, version: &str) -> PathBuf {
    let name = match version {
        "37.0" => "expected.dcc".to_owned(),
        "21.0" | "24.0" => format!("expected.{version}.dcc"),
        _ => panic!("unsupported Delphi compiler version {version:?}"),
    };
    case_dir.join(name)
}

/// `File.pas(23) E2037 message` lines; `#` lines are headers.
fn parse_expected(case: &Case, version: &str) -> Vec<Finding> {
    let path = expected_path(&case.dir, version);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{}: no {}; run scripts/dcc-oracle-record.sh --compiler {} {}",
            case.name,
            path.display(),
            version,
            case.name,
        )
    });
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (location, rest) = line.split_once(") ").expect("File(line) prefix");
            let (file, line_no) = location.split_once('(').expect("File(line)");
            let (code, message) = rest.split_once(' ').expect("code and message");
            Finding {
                file: file.to_owned(),
                line: line_no.parse().expect("line number"),
                code: code.to_owned(),
                message: message.to_owned(),
                severity: severity_for_code(code),
            }
        })
        .collect()
}

fn severity_for_code(code: &str) -> &'static str {
    match code.as_bytes().first() {
        Some(b'E') => "Error",
        Some(b'W') => "Warning",
        Some(b'H') => "Hint",
        _ => "Unknown",
    }
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())].matches('\n').count() + 1
}

fn project_context_for_case(
    case: &Case,
    version: &str,
    config: Option<String>,
    platform: Option<String>,
) -> Option<Arc<ProjectContext>> {
    let mut projects = std::fs::read_dir(&case.dir)
        .unwrap_or_else(|error| panic!("{}: case directory: {error}", case.name))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"))
        })
        .collect::<Vec<_>>();
    projects.sort();
    assert!(
        projects.len() <= 1,
        "{}: expected at most one .dproj, found {:?}",
        case.name,
        projects
    );
    let project_file = projects.into_iter().next()?;
    assert!(
        config
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        "{}: .dproj case.toml must set config",
        case.name
    );
    assert!(
        platform
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        "{}: .dproj case.toml must set platform",
        case.name
    );
    let source = case.dir.join(&case.unit);
    let options = ProjectOptions {
        project_file: Some(project_file),
        build_config: config,
        platform,
        conditional_context: oracle_context(version),
        ..ProjectOptions::default()
    };
    Some(
        ProjectContext::discover_with_overrides(
            &source,
            std::slice::from_ref(&case.dir),
            &options,
            &OverrideSession::new(None),
        )
        .unwrap_or_else(|error| panic!("{}: .dproj discovery: {error}", case.name))
        .context,
    )
}

fn project_switch_flags(
    warning_properties: &BTreeMap<String, PropertyValue>,
    table: &SwitchTable,
) -> Result<Vec<String>, String> {
    let mut flags = Vec::new();
    for (property, prefix) in [("DCC_Hints", "-H"), ("DCC_Warnings", "-W")] {
        if let Some(value) = warning_properties.get(property) {
            let PropertyValue::Known(value) = value else {
                return Err(format!("{property} is unevaluable"));
            };
            let sign = match value.trim().to_ascii_lowercase().as_str() {
                "true" => "+",
                "false" => "-",
                _ => return Err(format!("{property} must be true or false, got {value:?}")),
            };
            flags.push(format!("{prefix}{sign}"));
        }
    }

    for (property, value) in warning_properties {
        let Some(id) = table.warning_id_for_property(property) else {
            continue;
        };
        let PropertyValue::Known(value) = value else {
            return Err(format!("{property} is unevaluable"));
        };
        let sign = match value.trim().to_ascii_lowercase().as_str() {
            "true" => "+",
            "false" => "-",
            "error" => "^",
            _ => {
                return Err(format!(
                    "{property} must be true, false, or error, got {value:?}"
                ));
            }
        };
        flags.push(format!("-W{sign}{id}"));
    }

    flags.sort();
    Ok(flags)
}

fn compare_switch_manifest(recorded: &str, evaluated: &[String]) -> Result<(), String> {
    let recorded = recorded.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut evaluated = evaluated.to_vec();
    evaluated.sort();
    if recorded == evaluated {
        Ok(())
    } else {
        Err(format!(
            "dproj-switches.txt mismatch: recorded {recorded:?}, pascal-project evaluated {evaluated:?}"
        ))
    }
}

fn verify_project_switch_manifest(case: &Case, version: &str) {
    let Some(project) =
        project_context_for_case(case, version, case.config.clone(), case.platform.clone())
    else {
        return;
    };
    let compiler = CompilerVersion::parse(version)
        .unwrap_or_else(|| panic!("unsupported Delphi compiler version {version:?}"));
    let table = switch_table(compiler.major, compiler.minor)
        .unwrap_or_else(|| panic!("no dcc switch table for compiler {version}"));
    let evaluated = project_switch_flags(project.warning_properties(), &table)
        .unwrap_or_else(|error| panic!("{}: .dproj switch mapping failed: {error}", case.name));
    let manifest_path = case.dir.join("dproj-switches.txt");
    let recorded = std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
        panic!(
            "{}: missing or unreadable {}: {error}",
            case.name,
            manifest_path.display()
        )
    });
    compare_switch_manifest(&recorded, &evaluated)
        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
}

fn initial_state_for_case(case: &Case, version: &str) -> CaseInitialState {
    let conditional_context = oracle_context(version);
    let project =
        project_context_for_case(case, version, case.config.clone(), case.platform.clone());
    CaseInitialState {
        conditional_context,
        project,
    }
}

fn actual_findings(case: &Case, version: &str) -> Vec<Finding> {
    let mut index = NavigationIndex::new();
    // Only warning switches come from the project. The compiler-specific
    // conditional context remains closed exactly as oracle_context established.
    let case_state = initial_state_for_case(case, version);
    let context = case_state.conditional_context;
    let project = case_state.project;
    let cancel = AtomicBool::new(false);
    let mut unit_uri = None;
    let mut sources = HashMap::new();
    let system = case
        .requires_rtl
        .then(|| pascal_rtl_system(version))
        .flatten();
    if let Some((uri, source)) = &system {
        index
            .update_with_context_with_cancel(uri.clone(), source.clone(), &context, &cancel)
            .expect("PASCAL_RTL System parses");
    }
    let mut entries = std::fs::read_dir(&case.dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pas"))
        })
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let source = std::fs::read_to_string(&path).unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        let warning_properties = project
            .as_ref()
            .filter(|project| project.owns_unit_source(&path))
            .map(|project| Arc::new(project.warning_properties().clone()));
        index
            .update_with_session(
                uri.clone(),
                source.clone(),
                &context,
                None,
                &cancel,
                None,
                warning_properties,
            )
            .expect("fixture parses");
        if path.file_name().unwrap().to_string_lossy() == case.unit {
            unit_uri = Some(uri.clone());
        }
        if let Some((system_uri, _)) = &system {
            index.bind_imports(
                &uri,
                std::iter::once(("System".to_owned(), system_uri.clone())),
            );
        }
        sources.insert(uri, source);
    }
    let uri = unit_uri.unwrap_or_else(|| panic!("{}: unit {} not found", case.name, case.unit));
    let source = &sources[&uri];
    index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete")
        .into_iter()
        .map(|diagnostic| Finding {
            file: case.unit.clone(),
            line: line_of(source, diagnostic.span.start),
            code: diagnostic.kind.code().to_owned(),
            message: diagnostic.message,
            severity: match diagnostic.kind.severity() {
                DccSeverity::Error => "Error",
                DccSeverity::Warning => "Warning",
                DccSeverity::Hint => "Hint",
            },
        })
        .collect()
}

fn summarize_code(
    code: &str,
    expected: &[Finding],
    actual: &[Finding],
    recall_enforced: bool,
    silent_codes: &[String],
) -> CodeSummary {
    let expected = expected
        .iter()
        .filter(|finding| finding.code == code)
        .cloned()
        .collect::<Vec<_>>();
    let actual = actual
        .iter()
        .filter(|finding| finding.code == code)
        .cloned()
        .collect::<Vec<_>>();
    let silent = silent_codes.iter().any(|silent| silent == code);
    let (invented, missed) = finding_differences(&expected, &actual, silent_codes);

    CodeSummary {
        expected: expected.len(),
        matched: if silent {
            0
        } else {
            expected.len().saturating_sub(missed.len())
        },
        invented: invented.len(),
        missed: if recall_enforced && !silent {
            missed.len()
        } else {
            0
        },
        waived: if recall_enforced && !silent {
            0
        } else {
            expected.len()
        },
    }
}

fn format_summary(code: &str, summary: &CodeSummary) -> String {
    format!(
        "{code} expected={} matched={} invented={} missed={} waived={}",
        summary.expected, summary.matched, summary.invented, summary.missed, summary.waived
    )
}

fn finding_differences(
    expected: &[Finding],
    actual: &[Finding],
    silent_codes: &[String],
) -> (Vec<Finding>, Vec<Finding>) {
    let mut expected = expected
        .iter()
        .filter(|finding| !silent_codes.contains(&finding.code))
        .cloned()
        .collect::<Vec<_>>();
    let mut actual = actual.to_vec();
    expected.sort();
    actual.sort();
    let (mut expected_index, mut actual_index) = (0, 0);
    let (mut invented, mut missed) = (Vec::new(), Vec::new());
    while expected_index < expected.len() && actual_index < actual.len() {
        match actual[actual_index].cmp(&expected[expected_index]) {
            std::cmp::Ordering::Less => {
                invented.push(actual[actual_index].clone());
                actual_index += 1;
            }
            std::cmp::Ordering::Greater => {
                missed.push(expected[expected_index].clone());
                expected_index += 1;
            }
            std::cmp::Ordering::Equal => {
                actual_index += 1;
                expected_index += 1;
            }
        }
    }
    invented.extend_from_slice(&actual[actual_index..]);
    missed.extend_from_slice(&expected[expected_index..]);
    (invented, missed)
}

#[test]
fn finding_differences_compare_duplicate_occurrences() {
    let finding = Finding {
        file: "Probe.pas".to_owned(),
        line: 12,
        code: "E2065".to_owned(),
        message: "Unsatisfied forward or external declaration: 'TFoo.Missing'".to_owned(),
        severity: "Error",
    };
    let one = vec![finding.clone()];
    let two = vec![finding.clone(), finding.clone()];

    let (invented, missed) = finding_differences(&one, &two, &[]);
    assert_eq!(invented, vec![finding.clone()]);
    assert!(missed.is_empty());

    let (invented, missed) = finding_differences(&two, &one, &[]);
    assert!(invented.is_empty());
    assert_eq!(missed, vec![finding]);
}

#[test]
fn silent_case_codes_override_matching_dcc_and_linter_findings() {
    let case = Case {
        name: "synthetic".to_owned(),
        dir: PathBuf::new(),
        unit: "Synthetic.pas".to_owned(),
        compilers: vec!["37.0".to_owned()],
        config: None,
        platform: None,
        requires_rtl: false,
        recall: true,
        recall_exempt: Vec::new(),
        silent_codes: vec!["E2065".to_owned()],
    };
    let finding = Finding {
        file: case.unit.clone(),
        line: 12,
        code: "E2065".to_owned(),
        message: "Unsatisfied forward or external declaration: 'TFoo.Hidden'".to_owned(),
        severity: "Error",
    };

    let (invented, missed) = finding_differences(
        std::slice::from_ref(&finding),
        std::slice::from_ref(&finding),
        &case.silent_codes,
    );

    assert_eq!(invented, vec![finding.clone()]);
    assert!(missed.is_empty());

    let (invented, missed) =
        finding_differences(std::slice::from_ref(&finding), &[], &case.silent_codes);
    assert!(invented.is_empty());
    assert!(missed.is_empty());
}

#[test]
fn semantic_diagnostics_stay_silent_for_e2065_in_an_unknown_conditional() {
    let path = cases_root()
        .join("e2065_conditional_body")
        .join("ConditionalBody.pas");
    let source = std::fs::read_to_string(&path).unwrap();
    let uri = Url::from_file_path(path).unwrap();
    let mut index = NavigationIndex::new();
    index.update(uri.clone(), source).expect("fixture parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind.code() == "E2065")
    );
}

fn switch_filter_source(prefix: &str) -> String {
    format!(
        "unit SwitchFilterProbe;\ninterface\n{prefix}\
         type TObject = class end;\n\
         TBase = class(TObject) protected Value: Integer; end;\n\
         TChild = class(TBase) private FChild: Integer; public property Value: Integer read FChild; end;\n\
         implementation\nend.\n"
    )
}

#[test]
fn switch_filter_uses_agreed_defaults_without_a_known_switch_table() {
    let source = switch_filter_source("");
    for (uri, context) in [
        (
            Url::parse("file:///tmp/switch-filter-no-version.pas").unwrap(),
            ConditionalContext::default(),
        ),
        (
            Url::parse("file:///tmp/switch-filter-unsupported-version.pas").unwrap(),
            ConditionalContext::default().with_compiler_version(CompilerVersion::new(22, 0)),
        ),
    ] {
        let mut index = NavigationIndex::new();
        index
            .update_with_context_with_cancel(
                uri.clone(),
                source.clone(),
                &context,
                &AtomicBool::new(false),
            )
            .expect("probe parses");

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete");

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind.code() == "W1009"),
            "the unanimous default applies for {uri}"
        );
    }
}

#[test]
fn warning_switches_require_position_rules_for_every_enabled_code() {
    let source = "\
unit WarningSwitchPositionRules;
interface
{$WARNINGS OFF}
{$HINTS OFF}
implementation
end.
";
    let uri = Url::parse("file:///tmp/warning-switch-position-rules.pas").unwrap();
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &oracle_context("37.0"),
            &AtomicBool::new(false),
        )
        .expect("probe parses");

    let document = index.documents.get(&uri).expect("probe document indexed");
    let switches = super::warning_switches::WarningSwitches::new(document);
    for code in ENABLED_CODES
        .iter()
        .copied()
        .filter(|code| matches!(code.as_bytes().first(), Some(b'W' | b'H')))
    {
        assert_eq!(
            switches.effective_switch(document, code, source.len()),
            Some(super::dcc_switches::SwitchState::Off),
            "{code} has no recorded switch-position rule"
        );
    }
}

#[test]
fn unrecorded_warn_all_makes_id_switches_unknown_without_affecting_hints() {
    let source = "\
unit UnknownWarnAll;
interface
{$WARN ALL OFF}
type
  TObject = class
  end;
  TBase = class(TObject)
  protected
    Value: Integer;
  end;
  TChild = class(TBase)
  private
    FChild: Integer;
  public
    property Value: Integer read FChild;
  end;
  TUnused = class(TObject)
  private
    FUnused: Integer;
  end;
implementation
end.
";
    let uri = Url::parse("file:///tmp/unknown-warn-all.pas").unwrap();
    let context = ConditionalContext::default().with_compiler_version(CompilerVersion::new(22, 0));
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("probe parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind.code() != "W1009"),
        "an unrecorded ALL directive leaves warning-ID state unknown"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind.code() == "H2219"),
        "the unrecorded umbrella must not affect the hint row without a WARN ID"
    );
}

#[test]
fn unknown_conditional_warning_switch_makes_later_warning_unknown() {
    let source = switch_filter_source("{$IFDEF SWITCH_MAYBE_DEFINED}\n{$WARNINGS OFF}\n{$ENDIF}\n");
    let uri = Url::parse("file:///tmp/switch-filter-unknown-conditional.pas").unwrap();
    let mut index = NavigationIndex::new();
    let context = ConditionalContext::default().with_compiler_version(CompilerVersion::new(37, 0));
    let warning_offset = source.find("type TObject").expect("warning probe position");
    index
        .update_with_context_with_cancel(uri.clone(), source, &context, &AtomicBool::new(false))
        .expect("probe parses");

    let document = index.documents.get(&uri).expect("probe document indexed");
    let switches = super::warning_switches::WarningSwitches::new(document);
    assert_eq!(
        switches.effective_switch(document, "W1009", warning_offset),
        None,
        "an unknown conditional switch must leave the warning state unknown"
    );

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind.code() != "W1009")
    );
}

#[test]
fn expanded_include_warning_switch_filters_only_following_finding() {
    let case = cases_root().join("switch_in_include");
    let source = std::fs::read_to_string(case.join("SwitchInInclude.pas")).unwrap();
    let include = std::fs::read_to_string(case.join("Switches.inc")).unwrap();
    let include_directive = "{$I Switches.inc}";
    let include_start = source.find(include_directive).expect("include directive");
    let include_end = include_start + include_directive.len();
    let masked_directive = source[include_start..include_end]
        .chars()
        .map(|character| {
            if matches!(character, '\r' | '\n') {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let expanded_source = format!(
        "{}{}{}{}",
        &source[..include_start],
        masked_directive,
        include,
        &source[include_end..]
    );
    let context = oracle_context("37.0");
    let cancel = AtomicBool::new(false);
    let mut expansion = crate::include_expansion::ExpandedSource::default();
    expansion.conditionals = Some(pascal_core::conditional::analyze_with_context_and_cancel(
        &expanded_source,
        &context,
        &cancel,
    ));

    let uri = Url::parse("file:///tmp/switch-filter-expanded-include.pas").unwrap();
    let mut index = NavigationIndex::new();
    index
        .update_with_session(
            uri.clone(),
            expanded_source.clone(),
            &context,
            None,
            &cancel,
            Some(&expansion),
            None,
        )
        .expect("expanded probe parses");

    let document = index.documents.get(&uri).expect("probe document indexed");
    let switches = super::warning_switches::WarningSwitches::new(document);
    let after_include = expanded_source
        .rfind("property Value")
        .expect("post-include warning position");
    assert_eq!(
        switches.effective_switch(document, "W1009", after_include),
        Some(super::dcc_switches::SwitchState::Off)
    );

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &cancel)
        .expect("semantic diagnostics complete");
    let w1009 = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind.code() == "W1009")
        .collect::<Vec<_>>();
    assert_eq!(w1009.len(), 1, "{diagnostics:?}");
    assert!(
        w1009[0].span.start < include_start,
        "the pre-include finding remains enabled: {w1009:?}"
    );
}

#[test]
fn warning_switches_do_not_filter_error_diagnostics() {
    let source = concat!(
        "unit SwitchFilterError;\ninterface\n{$WARNINGS OFF}\n",
        "const DuplicateName = 1;\nvar DuplicateName: Integer;\n",
        "implementation\nend.\n",
    );
    let uri = Url::parse("file:///tmp/switch-filter-error.pas").unwrap();
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &oracle_context("37.0"),
            &AtomicBool::new(false),
        )
        .expect("probe parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind.code() == "E2004")
    );
}

#[test]
fn identical_parameter_methods_match_dcc_across_overload_directives() {
    for name in [
        "e2252_identical_params_unmarked",
        "e2252_identical_params_first_overload",
        "e2252_identical_params_second_overload",
    ] {
        let case = load_case(&cases_root().join(name));
        let expected = parse_expected(&case, "37.0")
            .into_iter()
            .filter(|finding| finding.code == "E2252")
            .collect::<Vec<_>>();
        let actual = actual_findings(&case, "37.0")
            .into_iter()
            .filter(|finding| finding.code == "E2252")
            .collect::<Vec<_>>();
        let (invented, missed) = finding_differences(&expected, &actual, &case.silent_codes);

        assert_eq!(expected.len(), 1, "{name}: expected one dcc E2252");
        assert!(invented.is_empty(), "{name}: invented E2252: {invented:?}");
        assert!(missed.is_empty(), "{name}: missed dcc E2252: {missed:?}");
    }
}

#[test]
fn oracle_context_closes_the_define_set() {
    let context = oracle_context("37.0");

    assert_eq!(
        context.defines.get("MSWINDOWS"),
        Some(&ConditionalFact::True)
    );
    assert_eq!(context.absent_define, ConditionalFact::False);
    assert!(!context.defines.contains_key("NOT_PREDEFINED"));

    let source = "unit OracleContext;\ninterface\n\
        {$IFDEF NOT_PREDEFINED}\n\
        type TInactive = Integer;\n\
        {$ENDIF}\n\
        {$IFDEF MSWINDOWS}\n\
        type TActive = Integer;\n\
        {$ENDIF}\n\
        implementation\nend.\n";
    let uri = Url::parse("file:///OracleContext.pas").unwrap();
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("context fixture parses");
    let parsed = index.parsed_document(&uri).expect("document indexed");
    let inactive_offset = source.find("TInactive").unwrap();
    let active_offset = source.find("TActive").unwrap();
    assert!(
        parsed
            .conditionals
            .inactive_spans
            .iter()
            .any(|span| span.start <= inactive_offset && inactive_offset < span.end)
    );
    assert!(!parsed.conditionals.is_unknown_at(active_offset));
    assert!(
        !parsed
            .conditionals
            .inactive_spans
            .iter()
            .any(|span| span.start <= active_offset && active_offset < span.end)
    );
}

#[test]
fn dproj_oracle_context_uses_selected_warning_properties_and_oracle_defines() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let dir = directory.path();
    std::fs::write(
        dir.join("Configured.pas"),
        "unit Configured;\ninterface\nimplementation\nend.\n",
    )
    .expect("Pascal fixture");
    std::fs::write(
        dir.join("Configured.dproj"),
        r#"<Project>
  <PropertyGroup><MainSource>Configured.pas</MainSource></PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug'">
    <DCC_SYMBOL_DEPRECATED>false</DCC_SYMBOL_DEPRECATED>
  </PropertyGroup>
</Project>"#,
    )
    .expect("project fixture");
    let case = Case {
        name: "synthetic_dproj".to_owned(),
        dir: dir.to_path_buf(),
        unit: "Configured.pas".to_owned(),
        compilers: vec!["37.0".to_owned()],
        config: Some("Debug".to_owned()),
        platform: Some("Win32".to_owned()),
        requires_rtl: false,
        recall: true,
        recall_exempt: Vec::new(),
        silent_codes: Vec::new(),
    };

    let project =
        project_context_for_case(&case, "37.0", case.config.clone(), case.platform.clone())
            .expect("case has one project file");
    let initial_state = initial_state_for_case(&case, "37.0");

    assert_eq!(project.config(), Some("Debug"));
    assert_eq!(
        initial_state
            .project
            .as_ref()
            .expect("case has one project file")
            .warning_properties()
            .get("DCC_SYMBOL_DEPRECATED"),
        Some(&PropertyValue::Known("false".to_owned()))
    );
    assert_eq!(
        initial_state.conditional_context.define("MSWINDOWS"),
        ConditionalFact::True
    );
    assert_eq!(
        initial_state.conditional_context.absent_define,
        ConditionalFact::False,
        "the dcc oracle's closed define set must survive project discovery"
    );
}

#[test]
fn configured_dproj_oracle_cases_select_warning_properties() {
    for (name, property, value) in [
        ("dproj_warning_switches", "DCC_SYMBOL_DEPRECATED", "false"),
        ("dproj_warnings_off", "DCC_Warnings", "false"),
    ] {
        let case = load_case(&cases_root().join(name));
        let project =
            project_context_for_case(&case, "37.0", case.config.clone(), case.platform.clone())
                .expect("fixture has one .dproj");

        assert_eq!(project.config(), Some("Debug"), "{name}");
        assert_eq!(
            project.warning_properties().get(property),
            Some(&PropertyValue::Known(value.to_owned())),
            "{name}"
        );
        if name == "dproj_warning_switches" {
            assert_eq!(
                project.warning_properties().get("DCC_Hints"),
                Some(&PropertyValue::Known("false".to_owned())),
                "{name}"
            );
        }
    }
}

#[test]
fn dproj_oracle_cases_require_explicit_config_and_platform() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let dir = directory.path();
    std::fs::write(
        dir.join("Configured.pas"),
        "unit Configured;\ninterface\nimplementation\nend.\n",
    )
    .expect("Pascal fixture");
    std::fs::write(
        dir.join("Configured.dproj"),
        "<Project><PropertyGroup><MainSource>Configured.pas</MainSource></PropertyGroup></Project>",
    )
    .expect("project fixture");

    for (config, platform, missing) in [
        (None, Some("Win32"), "config"),
        (Some(""), Some("Win32"), "config"),
        (Some("Debug"), None, "platform"),
        (Some("Debug"), Some(" \t"), "platform"),
    ] {
        let case = Case {
            name: "synthetic_dproj".to_owned(),
            dir: dir.to_path_buf(),
            unit: "Configured.pas".to_owned(),
            compilers: vec!["37.0".to_owned()],
            config: config.map(str::to_owned),
            platform: platform.map(str::to_owned),
            requires_rtl: false,
            recall: true,
            recall_exempt: Vec::new(),
            silent_codes: Vec::new(),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            project_context_for_case(&case, "37.0", case.config.clone(), case.platform.clone())
        }));
        let panic = match result {
            Err(panic) => panic,
            Ok(_) => panic!("missing {missing} selection should be rejected"),
        };
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains(&format!("case.toml must set {missing}")),
            "expected a clear missing-{missing} error, got {message:?}"
        );
    }
}

#[test]
fn dproj_switch_manifest_comparison_rejects_mismatched_flags() {
    let recorded = "-H-\n-W-\n";
    let evaluated = vec!["-H-".to_owned(), "-W-SYMBOL_DEPRECATED".to_owned()];

    assert!(compare_switch_manifest(recorded, &evaluated).is_err());
}

#[test]
fn dproj_switch_mapping_supports_error_promotion() {
    let properties = BTreeMap::from([(
        "DCC_SYMBOL_DEPRECATED".to_owned(),
        PropertyValue::Known("error".to_owned()),
    )]);
    let table = switch_table(37, 0).expect("37.0 switch table");

    assert_eq!(
        project_switch_flags(&properties, &table).unwrap(),
        ["-W^SYMBOL_DEPRECATED"]
    );
}

#[test]
fn dproj_recordings_document_the_compiler_switches_used() {
    for version in ["21.0", "24.0", "37.0"] {
        let warning_case = load_case(&cases_root().join("dproj_warning_switches"));
        let warning_record = std::fs::read_to_string(expected_path(&warning_case.dir, version))
            .expect("recorded .dproj warning case");
        assert!(
            warning_record
                .lines()
                .nth(1)
                .is_some_and(|header| header.contains("-H- -W-SYMBOL_DEPRECATED")),
            "{version}: missing configured hint/code switches in recording header"
        );

        let global_case = load_case(&cases_root().join("dproj_warnings_off"));
        let global_record = std::fs::read_to_string(expected_path(&global_case.dir, version))
            .expect("recorded .dproj global warning case");
        assert!(
            global_record
                .lines()
                .nth(1)
                .is_some_and(|header| header.contains("-W-")),
            "{version}: missing configured global warning switch in recording header"
        );
    }
}

#[test]
fn case_compilers_default_to_37() {
    let case = load_case(&cases_root().join("clean_unit"));

    assert_eq!(case.compilers, ["37.0"]);
}

#[test]
fn case_compilers_parse_multiple_versions() {
    let case = load_case_from_toml(
        "synthetic",
        Path::new("synthetic"),
        "unit = \"Synthetic.pas\"\ncompilers = [\"21.0\", \"24.0\"]\n",
    );

    assert_eq!(case.compilers, ["21.0", "24.0"]);
}

#[test]
#[should_panic(expected = "recall_exempt requires a reason")]
fn case_recall_exempt_requires_a_reason() {
    let _ = load_case_from_toml(
        "synthetic",
        Path::new("synthetic"),
        "unit = \"Synthetic.pas\"\nrecall_exempt = [\"W1000\"]\n",
    );
}

#[test]
fn case_build_selection_is_optional_and_parsed() {
    let case = load_case_from_toml(
        "synthetic",
        Path::new("synthetic"),
        "unit = \"Synthetic.pas\"\nconfig = \"Debug\"\nplatform = \"Win32\"\n",
    );

    assert_eq!(case.config.as_deref(), Some("Debug"));
    assert_eq!(case.platform.as_deref(), Some("Win32"));
}

#[test]
fn expected_file_names_follow_the_compiler() {
    assert_eq!(
        expected_path(Path::new("case"), "37.0")
            .file_name()
            .unwrap(),
        "expected.dcc"
    );
    assert_eq!(
        expected_path(Path::new("case"), "21.0")
            .file_name()
            .unwrap(),
        "expected.21.0.dcc"
    );
    assert_eq!(
        expected_path(Path::new("case"), "24.0")
            .file_name()
            .unwrap(),
        "expected.24.0.dcc"
    );
}

#[test]
fn summary_formats_synthetic_findings_and_waivers_per_code() {
    let expected = vec![
        Finding {
            file: "Probe.pas".to_owned(),
            line: 2,
            code: "E2004".to_owned(),
            message: "Duplicate identifier: 'Foo'".to_owned(),
            severity: "Error",
        },
        Finding {
            file: "Probe.pas".to_owned(),
            line: 3,
            code: "E2004".to_owned(),
            message: "Duplicate identifier: 'Bar'".to_owned(),
            severity: "Error",
        },
        Finding {
            file: "Probe.pas".to_owned(),
            line: 4,
            code: "E2065".to_owned(),
            message: "Unsatisfied forward or external declaration: 'TFoo.Missing'".to_owned(),
            severity: "Error",
        },
    ];
    let actual = vec![
        expected[0].clone(),
        Finding {
            file: "Probe.pas".to_owned(),
            line: 5,
            code: "E2004".to_owned(),
            message: "Duplicate identifier: 'Baz'".to_owned(),
            severity: "Error",
        },
    ];

    let e2004 = summarize_code("E2004", &expected, &actual, true, &[]);
    let e2065 = summarize_code("E2065", &expected, &actual, true, &["E2065".to_owned()]);

    assert_eq!(
        format_summary("E2004", &e2004),
        "E2004 expected=2 matched=1 invented=1 missed=1 waived=0"
    );
    assert_eq!(
        format_summary("E2065", &e2065),
        "E2065 expected=1 matched=0 invented=0 missed=0 waived=1"
    );
}

#[test]
fn dcc_oracle_cases_match_recorded_dcc_output() {
    let mut dirs = std::fs::read_dir(cases_root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    assert!(!dirs.is_empty(), "no dcc oracle cases");
    let mut failures = Vec::new();
    let mut summaries = ENABLED_CODES
        .iter()
        .map(|code| ((*code).to_owned(), CodeSummary::default()))
        .collect::<BTreeMap<_, _>>();
    for dir in dirs {
        let case = load_case(&dir);
        let enabled = |finding: &Finding| ENABLED_CODES.contains(&finding.code.as_str());
        for version in &case.compilers {
            verify_project_switch_manifest(&case, version);
            let expected = parse_expected(&case, version)
                .into_iter()
                .filter(|finding| enabled(finding) && finding.file.eq_ignore_ascii_case(&case.unit))
                .collect::<Vec<_>>();
            let actual = actual_findings(&case, version)
                .into_iter()
                .filter(enabled)
                .collect::<Vec<_>>();
            let rtl_available = rtl_root(version).is_some();
            for code in ENABLED_CODES {
                let recall_enforced = case.recall_enforced_for(code, rtl_available);
                let case_summary = summarize_code(
                    code,
                    &expected,
                    &actual,
                    recall_enforced,
                    &case.silent_codes,
                );
                if case.name == "w1000_type_in_var_decl" {
                    match *code {
                        "W1000" => assert_eq!(
                            case_summary.waived, case_summary.expected,
                            "only W1000 recall is waived in its duplicate-reporting case"
                        ),
                        "W1036" => assert_eq!(
                            case_summary.waived, 0,
                            "the W1000 duplicate waiver must not waive W1036 recall"
                        ),
                        _ => {}
                    }
                }
                let summary = summaries
                    .get_mut(*code)
                    .expect("summary initialized for every enabled code");
                summary.expected += case_summary.expected;
                summary.matched += case_summary.matched;
                summary.invented += case_summary.invented;
                summary.missed += case_summary.missed;
                summary.waived += case_summary.waived;
            }

            let (invented, missed) = finding_differences(&expected, &actual, &case.silent_codes);
            if !invented.is_empty() {
                failures.push(format!(
                    "{} under {version}: invented findings not reported by dcc: {invented:?}",
                    case.name
                ));
            }
            let missed_with_recall_enforced = missed
                .into_iter()
                .filter(|finding| case.recall_enforced_for(&finding.code, rtl_available))
                .collect::<Vec<_>>();
            if !missed_with_recall_enforced.is_empty() {
                failures.push(format!(
                    "{} under {version}: dcc reported, we missed: {missed_with_recall_enforced:?}",
                    case.name,
                ));
            }
        }
    }
    for code in ENABLED_CODES {
        let summary = summaries
            .get(*code)
            .expect("summary initialized for every enabled code");
        eprintln!("{}", format_summary(code, summary));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_enabled_warning_and_hint_has_a_row_in_each_switch_table() {
    for (major, minor) in [(21, 0), (24, 0), (37, 0)] {
        let table = switch_table(major, minor)
            .unwrap_or_else(|| panic!("missing {major}.{minor} switch table"));
        for code in ENABLED_CODES
            .iter()
            .copied()
            .filter(|code| matches!(code.as_bytes().first(), Some(b'W' | b'H')))
        {
            assert!(
                table.code(code).is_some(),
                "{major}.{minor} switch table is missing enabled code {code}"
            );
        }
    }
}

#[test]
fn promoted_warning_suppresses_flow_hints_in_the_erroneous_routine() {
    let case = load_case(&cases_root().join("w1000_error_promotion"));
    let actual = actual_findings(&case, "37.0");

    assert!(
        actual.iter().any(|finding| finding.code == "E1000"),
        "the promoted warning remains an error: {actual:?}"
    );
    assert!(
        actual.iter().all(|finding| finding.code != "H2077"),
        "dcc withholds flow hints from the routine containing a promoted warning: {actual:?}"
    );
}
