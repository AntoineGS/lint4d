//! dcc-recorded fixtures (`crates/pascal-lsp/tests/dcc_oracle`): for the
//! codes in `ENABLED_CODES`, the semantic pass must report exactly what dcc
//! recorded (precision always; recall unless a case needs the RTL and
//! `PASCAL_RTL_<version>` is unset).
use super::dcc_surface::{SurfaceKey, SurfacePlatform, table_for, verified_system_context};
use super::*;
use pascal_project::{CompilerVersion, ConditionalFact};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// dcc codes whose findings are compared. Grows as each rule lands.
const ENABLED_CODES: &[&str] = &[
    "E2003", "E2004", "E2037", "E2065", "E2137", "E2170", "E2211", "E2252", "E2254", "E2291",
    "E2301", "H2219", "W1009", "W1010",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    file: String,
    line: usize,
    code: String,
    message: String,
}

struct Case {
    name: String,
    dir: PathBuf,
    unit: String,
    compilers: Vec<String>,
    requires_rtl: bool,
    recall: bool,
    silent_codes: Vec<String>,
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
    assert!(
        recall || value("reason").is_some(),
        "{name}: recall = false requires a reason"
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
        requires_rtl: value("requires_rtl").as_deref() == Some("true"),
        recall,
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
            }
        })
        .collect()
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())].matches('\n').count() + 1
}

fn actual_findings(case: &Case, version: &str) -> Vec<Finding> {
    let mut index = NavigationIndex::new();
    let context = oracle_context(version);
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
        index
            .update_with_context_with_cancel(uri.clone(), source.clone(), &context, &cancel)
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
        requires_rtl: false,
        recall: true,
        silent_codes: vec!["E2065".to_owned()],
    };
    let finding = Finding {
        file: case.unit.clone(),
        line: 12,
        code: "E2065".to_owned(),
        message: "Unsatisfied forward or external declaration: 'TFoo.Hidden'".to_owned(),
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
        },
        Finding {
            file: "Probe.pas".to_owned(),
            line: 3,
            code: "E2004".to_owned(),
            message: "Duplicate identifier: 'Bar'".to_owned(),
        },
        Finding {
            file: "Probe.pas".to_owned(),
            line: 4,
            code: "E2065".to_owned(),
            message: "Unsatisfied forward or external declaration: 'TFoo.Missing'".to_owned(),
        },
    ];
    let actual = vec![
        expected[0].clone(),
        Finding {
            file: "Probe.pas".to_owned(),
            line: 5,
            code: "E2004".to_owned(),
            message: "Duplicate identifier: 'Baz'".to_owned(),
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
            let expected = parse_expected(&case, version)
                .into_iter()
                .filter(|finding| enabled(finding) && finding.file.eq_ignore_ascii_case(&case.unit))
                .collect::<Vec<_>>();
            let actual = actual_findings(&case, version)
                .into_iter()
                .filter(enabled)
                .collect::<Vec<_>>();
            let recall_enforced =
                case.recall && (!case.requires_rtl || rtl_root(version).is_some());
            for code in ENABLED_CODES {
                let case_summary = summarize_code(
                    code,
                    &expected,
                    &actual,
                    recall_enforced,
                    &case.silent_codes,
                );
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
            if recall_enforced && !missed.is_empty() {
                failures.push(format!(
                    "{} under {version}: dcc reported, we missed: {missed:?}",
                    case.name
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
