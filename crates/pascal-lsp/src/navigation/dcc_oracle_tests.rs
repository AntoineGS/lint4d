//! dcc-recorded fixtures (`crates/pascal-lsp/tests/dcc_oracle`): for the
//! codes in `ENABLED_CODES`, the semantic pass must report exactly what dcc
//! recorded (precision always; recall unless a case needs the RTL and
//! `PASCAL_RTL_<version>` is unset).
use super::dcc_surface::{SurfaceKey, SurfacePlatform, table_for, verified_system_context};
use super::*;
use pascal_project::{CompilerVersion, ConditionalFact};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_RTL_IMPORT_CLOSURE_UNITS: usize = 4096;
const MAX_RTL_IMPORT_CLOSURE_DEPTH: usize = 64;

/// dcc codes whose findings are compared. Grows as each rule lands.
const ENABLED_CODES: &[&str] = &[
    "E2003", "E2004", "E2037", "E2065", "E2137", "E2170", "E2211", "E2252", "E2254", "E2291",
    "E2301", "H2219", "H2443", "W1009", "W1010",
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
    recall_exempt: Vec<String>,
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

#[derive(Debug, Default)]
struct OracleUnitContext {
    namespaces: Vec<String>,
    aliases: HashMap<String, String>,
}

impl OracleUnitContext {
    fn candidate_tiers(&self, name: &str) -> Vec<Vec<String>> {
        let canonical = canonical_name(name);
        let primary = self.aliases.get(&canonical).map_or(name, String::as_str);
        let short = primary.rsplit('.').next().unwrap_or(primary);
        let mut tiers = vec![if short.eq_ignore_ascii_case(primary) {
            vec![primary.to_owned()]
        } else {
            vec![primary.to_owned(), short.to_owned()]
        }];
        if !primary.contains('.') {
            tiers.extend(self.namespaces.iter().filter_map(|namespace| {
                let namespace = namespace.trim().trim_matches('.');
                (!namespace.is_empty()).then(|| vec![format!("{namespace}.{primary}")])
            }));
        }
        for tier in &mut tiers {
            tier.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        }
        tiers
    }
}

fn oracle_unit_context(context: &ConditionalContext, root: &Path) -> OracleUnitContext {
    let namespaces = match context.compiler_version {
        Some(CompilerVersion { major: 24 | 37, .. }) => {
            vec!["System".to_owned(), "Winapi".to_owned()]
        }
        _ => Vec::new(),
    };
    let mut aliases = HashMap::new();
    let source_root = if root
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("win32"))
    {
        root.parent().unwrap_or(root)
    } else {
        root
    };
    if let Some(installation_root) = source_root.parent() {
        for config in [
            installation_root.join("bin/dcc32.cfg"),
            installation_root.join("Bin/dcc32.cfg"),
        ] {
            let Ok(contents) = std::fs::read_to_string(config) else {
                continue;
            };
            for line in contents.lines() {
                let Some(alias_list) = line.trim().get(2..).filter(|_| {
                    line.trim()
                        .get(..2)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("-a"))
                }) else {
                    continue;
                };
                for alias in alias_list.split(';') {
                    let Some((name, target)) = alias.split_once('=') else {
                        continue;
                    };
                    let name = name.trim();
                    let target = target.trim();
                    if !name.is_empty() && !target.is_empty() {
                        aliases.insert(canonical_name(name), target.to_owned());
                    }
                }
            }
        }
    }
    OracleUnitContext {
        namespaces,
        aliases,
    }
}

fn pascal_rtl_unit_with_context(
    root: &Path,
    name: &str,
    unit_context: &OracleUnitContext,
) -> Option<(Url, String)> {
    for tier in unit_context.candidate_tiers(name) {
        let mut paths = Vec::new();
        for candidate in tier {
            let file_name = format!("{candidate}.pas");
            for directory in ["rtl/sys", "rtl/common", "rtl/win", "rtl/objpas"] {
                let directory = root.join(directory);
                let Ok(entries) = std::fs::read_dir(directory) else {
                    continue;
                };
                paths.extend(
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.is_file()
                                && path.file_name().is_some_and(|actual| {
                                    actual.to_string_lossy().eq_ignore_ascii_case(&file_name)
                                })
                        }),
                );
            }
        }
        paths.sort();
        paths.dedup();
        match paths.as_slice() {
            [path] => {
                let source = std::fs::read_to_string(path)
                    .unwrap_or_else(|error| panic!("PASCAL_RTL source {path:?}: {error}"));
                let uri = Url::from_file_path(path).expect("PASCAL_RTL source URI");
                return Some((uri, source));
            }
            [] => {}
            _ => return None,
        }
    }
    None
}

fn index_rtl_imports(
    index: &mut NavigationIndex,
    root: &Path,
    context: &ConditionalContext,
    source_uris: &[Url],
    known_units: &mut HashSet<String>,
    cancel: &AtomicBool,
) -> Vec<Url> {
    let unit_context = oracle_unit_context(context, root);
    let mut pending = source_uris
        .iter()
        .cloned()
        .map(|uri| (uri, 0usize))
        .collect::<VecDeque<_>>();
    let mut visited = source_uris.iter().cloned().collect::<HashSet<_>>();
    let mut indexed = Vec::new();
    while let Some((source_uri, depth)) = pending.pop_front() {
        let imports = index
            .imports(&source_uri)
            .into_iter()
            .map(|import| import.name)
            .collect::<BTreeSet<_>>();
        for import in imports {
            let tiers = unit_context.candidate_tiers(&import);
            if tiers
                .iter()
                .flatten()
                .any(|candidate| known_units.contains(&canonical_name(candidate)))
            {
                continue;
            }
            let Some((uri, source)) = pascal_rtl_unit_with_context(root, &import, &unit_context)
            else {
                continue;
            };
            if !visited.insert(uri.clone()) {
                continue;
            }
            if depth >= MAX_RTL_IMPORT_CLOSURE_DEPTH {
                panic!(
                    "PASCAL_RTL import closure exceeded depth {MAX_RTL_IMPORT_CLOSURE_DEPTH} at {import}"
                );
            }
            if indexed.len() >= MAX_RTL_IMPORT_CLOSURE_UNITS {
                panic!(
                    "PASCAL_RTL import closure exceeded {MAX_RTL_IMPORT_CLOSURE_UNITS} source units"
                );
            }
            index
                .update_with_context_with_cancel(uri.clone(), source, context, cancel)
                .unwrap_or_else(|error| panic!("PASCAL_RTL {import} parses: {error}"));
            if let Some(name) = index.unit_name(&uri) {
                known_units.insert(canonical_name(&name));
            }
            pending.push_back((uri.clone(), depth + 1));
            indexed.push(uri);
        }
    }
    indexed
}

fn bind_rtl_imports(
    index: &mut NavigationIndex,
    source_uris: &[Url],
    unit_context: &OracleUnitContext,
) {
    let mut units: BTreeMap<String, Vec<Url>> = BTreeMap::new();
    for uri in source_uris {
        if let Some(name) = index.unit_name(uri) {
            units
                .entry(canonical_name(&name))
                .or_default()
                .push(uri.clone());
        }
    }
    for uris in units.values_mut() {
        uris.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        uris.dedup();
    }
    for uri in source_uris {
        let bindings = index
            .imports(uri)
            .into_iter()
            .filter_map(|import| {
                for tier in unit_context.candidate_tiers(&import.name) {
                    let mut providers = tier
                        .iter()
                        .filter_map(|candidate| units.get(&canonical_name(candidate)))
                        .flatten()
                        .cloned()
                        .collect::<Vec<_>>();
                    providers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                    providers.dedup();
                    match providers.as_slice() {
                        [provider] => return Some((import.name, provider.clone())),
                        [] => {}
                        _ => return None,
                    }
                }
                None
            })
            .collect::<Vec<_>>();
        index.bind_imports(uri, bindings);
    }
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
    let reason = value("recall_exempt_reason").or_else(|| value("reason"));
    assert!(
        (recall && recall_exempt.is_empty()) || reason.is_some(),
        "{name}: recall waivers require a reason"
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
        sources.insert(uri, source);
    }
    if let Some(root) = rtl_root(version).filter(|_| case.requires_rtl) {
        let mut known_units = sources
            .keys()
            .chain(system.iter().map(|(uri, _)| uri))
            .filter_map(|uri| index.unit_name(uri))
            .map(|name| canonical_name(&name))
            .collect::<HashSet<_>>();
        let fixture_uris = sources.keys().cloned().collect::<Vec<_>>();
        let rtl_providers = index_rtl_imports(
            &mut index,
            &root,
            &context,
            &fixture_uris,
            &mut known_units,
            &cancel,
        );
        for (uri, source) in &sources {
            index
                .update_with_context_with_cancel(uri.clone(), source.clone(), &context, &cancel)
                .expect("fixture reparses with PASCAL_RTL providers indexed");
        }
        let unit_context = oracle_unit_context(&context, &root);
        let mut indexed_uris = fixture_uris;
        indexed_uris.extend(rtl_providers);
        indexed_uris.extend(system.iter().map(|(uri, _)| uri.clone()));
        bind_rtl_imports(&mut index, &indexed_uris, &unit_context);
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
    recall_exempt: bool,
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
        waived: if silent {
            expected.len()
        } else if recall_enforced {
            0
        } else if recall_exempt {
            missed.len()
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
fn rtl_loader_resolves_scoped_unit_filenames() {
    let root = tempfile::tempdir().unwrap();
    let rtl_common = root.path().join("rtl/common");
    std::fs::create_dir_all(&rtl_common).unwrap();
    let path = rtl_common.join("System.AnsiStrings.pas");
    std::fs::write(
        &path,
        "unit System.AnsiStrings; interface implementation end.",
    )
    .unwrap();
    let context = ConditionalContext::default().with_compiler_version(CompilerVersion::new(24, 0));
    let unit_context = oracle_unit_context(&context, root.path());

    let (uri, source) = pascal_rtl_unit_with_context(root.path(), "AnsiStrings", &unit_context)
        .expect("compiler-scoped RTL source");

    assert_eq!(uri, Url::from_file_path(path).unwrap());
    assert!(source.starts_with("unit System.AnsiStrings;"));
}

#[test]
fn rtl_import_binding_uses_compiler_namespace_scopes_and_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let installation = temp.path().join("install");
    let root = installation.join("source");
    let rtl_common = root.join("rtl/common");
    std::fs::create_dir_all(&rtl_common).unwrap();
    std::fs::write(
        rtl_common.join("System.First.pas"),
        "unit System.First; interface implementation end.",
    )
    .unwrap();
    let dcc_bin = installation.join("bin");
    std::fs::create_dir_all(&dcc_bin).unwrap();
    std::fs::write(dcc_bin.join("dcc32.cfg"), "-aLegacyFirst=First\n").unwrap();
    let fixture_uri = Url::from_file_path(temp.path().join("Probe.pas")).unwrap();
    let context = ConditionalContext::default().with_compiler_version(CompilerVersion::new(24, 0));
    let cancel = AtomicBool::new(false);
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            fixture_uri.clone(),
            "unit Probe; interface uses LegacyFirst; implementation end.".to_owned(),
            &context,
            &cancel,
        )
        .expect("fixture parses");
    let mut known_units = HashSet::from(["probe".to_owned()]);
    let providers = index_rtl_imports(
        &mut index,
        &root,
        &context,
        std::slice::from_ref(&fixture_uri),
        &mut known_units,
        &cancel,
    );
    let unit_context = oracle_unit_context(&context, &root);
    let mut indexed_uris = vec![fixture_uri.clone()];
    indexed_uris.extend(providers);
    bind_rtl_imports(&mut index, &indexed_uris, &unit_context);

    assert_eq!(
        index.import_provider_uri(&fixture_uri, "LegacyFirst"),
        Some(&Url::from_file_path(rtl_common.join("System.First.pas")).unwrap())
    );
}

#[test]
fn rtl_loader_indexes_the_bounded_transitive_import_closure() {
    let root = tempfile::tempdir().unwrap();
    let rtl_common = root.path().join("rtl/common");
    std::fs::create_dir_all(&rtl_common).unwrap();
    for (name, source) in [
        (
            "First",
            "unit First; interface uses Second; implementation end.",
        ),
        (
            "Second",
            "unit Second; interface uses Third; implementation end.",
        ),
        ("Third", "unit Third; interface implementation end."),
    ] {
        std::fs::write(rtl_common.join(format!("{name}.pas")), source).unwrap();
    }
    let fixture_uri = Url::from_file_path(root.path().join("Probe.pas")).unwrap();
    let mut index = NavigationIndex::new();
    let context = ConditionalContext::default();
    let cancel = AtomicBool::new(false);
    index
        .update_with_context_with_cancel(
            fixture_uri.clone(),
            "unit Probe; interface uses First; implementation end.".to_owned(),
            &context,
            &cancel,
        )
        .expect("fixture parses");
    let mut known_units = HashSet::from(["probe".to_owned()]);

    let loaded = index_rtl_imports(
        &mut index,
        root.path(),
        &context,
        &[fixture_uri],
        &mut known_units,
        &cancel,
    );

    assert_eq!(loaded.len(), 3);
    assert!(
        loaded
            .iter()
            .any(|uri| index.unit_name(uri).as_deref() == Some("third"))
    );
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
        recall_exempt: Vec::new(),
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
        "unit = \"Synthetic.pas\"\n\
         compilers = [\"21.0\", \"24.0\"]\n\
         recall_exempt = [\"H2443\"]\n\
         recall_exempt_reason = \"The provider is incomplete.\"\n",
    );

    assert_eq!(case.compilers, ["21.0", "24.0"]);
    assert_eq!(case.recall_exempt, ["H2443"]);
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

    let e2004 = summarize_code("E2004", &expected, &actual, true, &[], false);
    let e2065 = summarize_code(
        "E2065",
        &expected,
        &actual,
        true,
        &["E2065".to_owned()],
        false,
    );

    assert_eq!(
        format_summary("E2004", &e2004),
        "E2004 expected=2 matched=1 invented=1 missed=1 waived=0"
    );
    assert_eq!(
        format_summary("E2065", &e2065),
        "E2065 expected=1 matched=0 invented=0 missed=0 waived=1"
    );

    let rtl_expected = vec![
        Finding {
            file: "RtlUse.pas".to_owned(),
            line: 8,
            code: "H2443".to_owned(),
            message: "Inline function 'AnsiCompareStr' has not been expanded".to_owned(),
        },
        Finding {
            file: "RtlUse.pas".to_owned(),
            line: 9,
            code: "H2443".to_owned(),
            message: "Inline function 'Other' has not been expanded".to_owned(),
        },
    ];
    let rtl_actual = vec![rtl_expected[0].clone()];
    let h2443 = summarize_code("H2443", &rtl_expected, &rtl_actual, false, &[], true);
    assert_eq!(
        format_summary("H2443", &h2443),
        "H2443 expected=2 matched=1 invented=0 missed=0 waived=1"
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
                let recall_exempt = case.recall_exempt.iter().any(|exempt| exempt == code);
                let case_summary = summarize_code(
                    code,
                    &expected,
                    &actual,
                    recall_enforced && !recall_exempt,
                    &case.silent_codes,
                    recall_exempt,
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
            let missed = missed
                .into_iter()
                .filter(|finding| {
                    !case
                        .recall_exempt
                        .iter()
                        .any(|exempt| exempt == &finding.code)
                })
                .collect::<Vec<_>>();
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
