//! dcc-recorded fixtures (`crates/pascal-lsp/tests/dcc_oracle`): for the
//! codes in `ENABLED_CODES`, the semantic pass must report exactly what dcc
//! recorded (precision always; recall unless a case needs the RTL and
//! `PASCAL_RTL` is unset).
use super::*;
use std::path::{Path, PathBuf};

/// dcc codes whose findings are compared. Grows as each rule lands.
const ENABLED_CODES: &[&str] = &[];

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
    requires_rtl: bool,
}

fn cases_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dcc_oracle")
}

fn load_case(dir: &Path) -> Case {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let toml = std::fs::read_to_string(dir.join("case.toml"))
        .unwrap_or_else(|error| panic!("{name}: case.toml: {error}"));
    let value = |key: &str| {
        toml.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
        })
    };
    Case {
        unit: value("unit").unwrap_or_else(|| panic!("{name}: case.toml has no unit")),
        requires_rtl: value("requires_rtl").as_deref() == Some("true"),
        name,
        dir: dir.to_path_buf(),
    }
}

/// `File.pas(23) E2037 message` lines; `#` lines are headers.
fn parse_expected(case: &Case) -> Vec<Finding> {
    let path = case.dir.join("expected.dcc");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{}: no expected.dcc; run scripts/dcc-oracle-record.sh {}",
            case.name, case.name
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

fn diagnostic_code(kind: SemanticDiagnosticKind) -> &'static str {
    match kind {
        SemanticDiagnosticKind::UnresolvedIdentifier => "pascal-unresolved-identifier",
        SemanticDiagnosticKind::MissingMember => "pascal-missing-member",
        SemanticDiagnosticKind::TypeMismatch => "pascal-type-mismatch",
        SemanticDiagnosticKind::IncompatibleArgument => "pascal-incompatible-argument",
        SemanticDiagnosticKind::InvalidOverride => "pascal-invalid-override",
        SemanticDiagnosticKind::MissingInterfaceImplementation => {
            "pascal-missing-interface-implementation"
        }
    }
}

fn actual_findings(case: &Case) -> Vec<Finding> {
    let mut index = NavigationIndex::new();
    let mut unit_uri = None;
    let mut sources = HashMap::new();
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
            .update(uri.clone(), source.clone())
            .expect("fixture parses");
        if path.file_name().unwrap().to_string_lossy() == case.unit {
            unit_uri = Some(uri.clone());
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
            code: diagnostic_code(diagnostic.kind).to_owned(),
            message: diagnostic.message,
        })
        .collect()
}

fn finding_differences(expected: &[Finding], actual: &[Finding]) -> (Vec<Finding>, Vec<Finding>) {
    let mut expected = expected.to_vec();
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

    let (invented, missed) = finding_differences(&one, &two);
    assert_eq!(invented, vec![finding.clone()]);
    assert!(missed.is_empty());

    let (invented, missed) = finding_differences(&two, &one);
    assert!(invented.is_empty());
    assert_eq!(missed, vec![finding]);
}

#[test]
fn dcc_oracle_cases_match_recorded_dcc_output() {
    let rtl_available = std::env::var_os("PASCAL_RTL").is_some();
    let mut dirs = std::fs::read_dir(cases_root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    assert!(!dirs.is_empty(), "no dcc oracle cases");
    let mut failures = Vec::new();
    for dir in dirs {
        let case = load_case(&dir);
        let enabled = |finding: &Finding| ENABLED_CODES.contains(&finding.code.as_str());
        let expected = parse_expected(&case)
            .into_iter()
            .filter(|finding| enabled(finding) && finding.file.eq_ignore_ascii_case(&case.unit))
            .collect::<Vec<_>>();
        let actual = actual_findings(&case)
            .into_iter()
            .filter(enabled)
            .collect::<Vec<_>>();
        let (invented, missed) = finding_differences(&expected, &actual);
        if !invented.is_empty() {
            failures.push(format!("{}: not reported by dcc: {invented:?}", case.name));
        }
        if (!case.requires_rtl || rtl_available) && !missed.is_empty() {
            failures.push(format!(
                "{}: dcc reported, we missed: {missed:?}",
                case.name
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
