//! Developer-only precision check against installed dcc-clean RTL and VCL.

use super::dcc_surface::{SurfaceKey, SurfacePlatform, table_for, verified_system_context};
use super::{
    DccMessage, NavigationIndex, SemanticDiagnosticKind, SemanticProofCounters, SemanticProofStatus,
};
use lsp_types::Url;
use pascal_project::{ConditionalContext, ConditionalFact};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const CORPUS_DIRECTORIES: &[&str] = &["rtl/sys", "rtl/common", "rtl/win", "vcl"];

#[test]
fn semantic_diagnostics_resolve_imported_record_alias_fields_in_with() {
    let provider_uri = Url::parse("file:///tmp/corpus-windows.pas").expect("provider URI");
    let consumer_uri = Url::parse("file:///tmp/corpus-consumer.pas").expect("consumer URI");
    let provider = concat!(
        "unit Windows;\n",
        "interface\n",
        "type\n",
        "  _SYSTEMTIME = record wYear: Word; end;\n",
        "  TSystemTime = _SYSTEMTIME;\n",
        "implementation\n",
        "end.\n",
    );
    let consumer = concat!(
        "unit Consumer;\n",
        "interface\n",
        "uses Windows;\n",
        "implementation\n",
        "function YearOf: Word;\n",
        "var SysTime: TSystemTime;\n",
        "begin\n",
        "  with SysTime do Result := wYear;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("record provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("record consumer parses");
    index.bind_imports(&consumer_uri, [("Windows".to_owned(), provider_uri)]);
    let diagnostics = index
        .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)),
        "imported record fields are resolved through with: {diagnostics:?}"
    );
    let offset = consumer.rfind("wYear").expect("field use");
    let position = super::text::offset_to_position(consumer, offset).expect("field position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&consumer_uri, position, &AtomicBool::new(false),)
            .expect("field proof status"),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn semantic_diagnostics_resolve_variant_record_fields() {
    let uri = Url::parse("file:///tmp/corpus-variant-record.pas").expect("fixture URI");
    let source = concat!(
        "unit VariantRecord;\n",
        "interface\n",
        "type\n",
        "  TVector = packed record\n",
        "    case Integer of\n",
        "      0: (x: Integer; y: Integer; z: Integer);\n",
        "      1: (dx: Integer; dy: Integer; dz: Integer);\n",
        "  end;\n",
        "implementation\n",
        "function Add(const Left, Right: TVector): TVector;\n",
        "begin\n",
        "  Result.x := Left.x + Right.x;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("variant record fixture parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)),
        "variant record fields are resolved: {diagnostics:?}"
    );
    let offset = source.find("Result.x").expect("field use") + "Result.".len();
    let position = super::text::offset_to_position(source, offset).expect("field position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&uri, position, &AtomicBool::new(false))
            .expect("field proof status"),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn semantic_diagnostics_keep_indexed_array_types_through_builtin_aliases() {
    let uri = Url::parse("file:///tmp/corpus-builtin-index-alias.pas").expect("fixture URI");
    let source = concat!(
        "unit BuiltinIndexAlias;\n",
        "interface\n",
        "type TIndex = Integer;\n",
        "implementation\n",
        "procedure Run;\n",
        "var Expected: Integer; Values: array[0..1] of Boolean; Idx: TIndex;\n",
        "begin\n",
        "  Expected := Values[Idx];\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("builtin-index alias fixture parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::TypeMismatch)
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        mismatches,
        ["type mismatch: cannot assign 'Boolean' to 'Integer'"],
        "a proven integer alias must preserve the array element type: {diagnostics:?}"
    );
}

#[test]
fn semantic_diagnostics_resolve_members_through_record_type_aliases() {
    let uri = Url::parse("file:///tmp/corpus-record-alias.pas").expect("fixture URI");
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for(&key).expect("37.0 Win32 surface table");
    let context = verified_system_context(&key, &table);
    let source = concat!(
        "unit RecordAlias;\n",
        "interface\n",
        "type\n",
        "  TStorageVector = packed record\n",
        "    case Integer of\n",
        "      0: (x: Integer; y: Integer; z: Integer);\n",
        "      1: (dx: Integer; dy: Integer; dz: Integer);\n",
        "  end;\n",
        "  TVector = TStorageVector;\n",
        "implementation\n",
        "function Add(const Left, Right: TVector): TVector;\n",
        "begin\n",
        "  Result.x := Left.x + Right.x;\n",
        "  with Left do x := x + 1;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("aliased variant record fixture parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)),
        "members of a record type alias are resolved: {diagnostics:?}"
    );
    let offset = source.find("Result.x").expect("field use") + "Result.".len();
    let position = super::text::offset_to_position(source, offset).expect("field position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&uri, position, &AtomicBool::new(false))
            .expect("field proof status"),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn semantic_diagnostics_do_not_claim_implicit_pointer_record_fields_absent() {
    let uri = Url::parse("file:///tmp/corpus-pointer-record.pas").expect("fixture URI");
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for(&key).expect("37.0 Win32 surface table");
    let context = verified_system_context(&key, &table);
    let source = concat!(
        "unit PointerRecord;\n",
        "interface\n",
        "type\n",
        "  TRecord = record Value: Integer; end;\n",
        "  PRecord = ^TRecord;\n",
        "implementation\n",
        "procedure ReadRecord(Rec: PRecord);\n",
        "var Value: Integer;\n",
        "begin\n",
        "  Value := Rec.Value;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("pointer record fixture parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)),
        "unknown pointee member shape must not produce E2003: {diagnostics:?}"
    );
    let offset = source.find("Rec.Value").expect("pointer field access") + "Rec.".len();
    let position = super::text::offset_to_position(source, offset).expect("field position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&uri, position, &AtomicBool::new(false))
            .expect("field proof status"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn semantic_diagnostics_resolve_private_self_qualified_unit_routines() {
    let uri = Url::parse("file:///tmp/corpus-self-qualified-unit.pas").expect("fixture URI");
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for(&key).expect("37.0 Win32 surface table");
    let context = verified_system_context(&key, &table);
    let source = concat!(
        "unit SelfQualified;\n",
        "interface\n",
        "implementation\n",
        "function Hidden: Integer;\n",
        "begin\n",
        "  Result := 1;\n",
        "end;\n",
        "function Caller: Integer;\n",
        "begin\n",
        "  Result := SelfQualified.Hidden;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("self-qualified unit fixture parses");

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)),
        "a unit can qualify its own implementation routines: {diagnostics:?}"
    );
    let offset = source
        .find("SelfQualified.Hidden")
        .expect("self-qualified call")
        + "SelfQualified.".len();
    let position = super::text::offset_to_position(source, offset).expect("routine position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&uri, position, &AtomicBool::new(false))
            .expect("routine proof status"),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn semantic_diagnostics_resolve_sysinit_bootstrap_exports_from_incomplete_system_source() {
    let system_uri = Url::parse("file:///tmp/corpus-system.pas").expect("System URI");
    let sysinit_uri = Url::parse("file:///tmp/corpus-sysinit.pas").expect("SysInit URI");
    let regular_uri = Url::parse("file:///tmp/corpus-regular-unit.pas").expect("regular URI");
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win64,
    };
    let table = table_for(&key).expect("37.0 Win64 surface table");
    assert!(!table.has_name("_StartExe"));
    assert!(!table.has_name("_StartLib"));
    let context = verified_system_context(&key, &table);
    let system_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let system = concat!(
        "unit System;\n",
        "interface\n",
        "procedure _StartExe;\n",
        "procedure _StartLib;\n",
        "{$IFDEF MODEL_UNKNOWN}\n",
        "procedure MaybeSystem;\n",
        "{$ENDIF}\n",
        "implementation\n",
        "end.\n",
    );
    let sysinit = concat!(
        "unit SysInit;\n",
        "interface\n",
        "implementation\n",
        "procedure Start;\n",
        "begin\n",
        "  _StartExe;\n",
        "  _StartLib;\n",
        "end;\n",
        "end.\n",
    );
    let regular = concat!(
        "unit Regular;\n",
        "interface\n",
        "implementation\n",
        "procedure Start;\n",
        "begin\n",
        "  _StartExe;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            system_uri.clone(),
            system.to_owned(),
            &system_context,
            &AtomicBool::new(false),
        )
        .expect("System fixture parses");
    index
        .update_with_context_with_cancel(
            sysinit_uri.clone(),
            sysinit.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("SysInit fixture parses");
    index
        .update_with_context_with_cancel(
            regular_uri.clone(),
            regular.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("regular unit fixture parses");
    assert!(
        !index.documents[&system_uri]
            .conditionals
            .unknown_spans
            .is_empty(),
        "System fixture must be incomplete outside the bootstrap declarations"
    );

    let sysinit_diagnostics = index
        .semantic_diagnostics_with_cancel(&sysinit_uri, &AtomicBool::new(false))
        .expect("SysInit diagnostics complete");
    let regular_diagnostics = index
        .semantic_diagnostics_with_cancel(&regular_uri, &AtomicBool::new(false))
        .expect("regular unit diagnostics complete");

    assert!(
        sysinit_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)),
        "SysInit can use the bootstrap exports declared by implicit System: {sysinit_diagnostics:?}"
    );
    let start_exe = sysinit.find("_StartExe").expect("SysInit bootstrap call");
    let position = super::text::offset_to_position(sysinit, start_exe).expect("call position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&sysinit_uri, position, &AtomicBool::new(false))
            .expect("SysInit proof status"),
        SemanticProofStatus::Resolved
    );
    assert_eq!(
        regular_diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::Dcc(DccMessage::E2003)
            })
            .count(),
        1,
        "ordinary units still obey the closed dcc surface: {regular_diagnostics:?}"
    );
}

#[test]
fn semantic_diagnostics_fail_closed_for_record_helper_targets_through_aliases() {
    let uri = Url::parse("file:///tmp/corpus-record-helper-operator.pas").expect("fixture URI");
    let key = SurfaceKey {
        major: 21,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for(&key).expect("21.0 Win32 surface table");
    let context = verified_system_context(&key, &table);
    let source = concat!(
        "unit RecordHelperOperator;\n",
        "interface\n",
        "type\n",
        "  TMatrix = record\n",
        "    class operator Multiply(const Left, Right: TMatrix): TMatrix;\n",
        "  end;\n",
        "  TMatrixAlias = TMatrix;\n",
        "  TMatrixHelper = record helper for TMatrixAlias\n",
        "    class function SetProduct(A, B: TMatrixAlias): TMatrixAlias; static;\n",
        "  end;\n",
        "implementation\n",
        "class operator TMatrix.Multiply(const Left, Right: TMatrix): TMatrix;\n",
        "begin\n",
        "  Result := SetProduct(Left, Right);\n",
        "end;\n",
        "class function TMatrixHelper.SetProduct(A, B: TMatrixAlias): TMatrixAlias;\n",
        "begin\n",
        "  Result := A;\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("record helper operator fixture parses");
    assert!(
        index.documents[&uri].parser_recovery_spans.is_empty(),
        "record helper operator fixture must parse without recovery: {:?}",
        index.documents[&uri].parser_recovery_spans
    );

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");
    let helper_call = source.find("SetProduct(Left").expect("record helper call");
    let e2003_at_helper_call = diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::Dcc(DccMessage::E2003)
            && diagnostic.span.start == helper_call
    });
    assert!(
        !e2003_at_helper_call,
        "an unresolved record-helper alias must not prove the method absent: {diagnostics:?}"
    );
    let position =
        super::text::offset_to_position(source, helper_call).expect("helper call position");
    assert_eq!(
        index
            .semantic_proof_status_at_with_cancel(&uri, position, &AtomicBool::new(false))
            .expect("helper call proof status"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn semantic_diagnostics_recognize_exception_handler_variables() {
    let provider_uri = Url::parse("file:///tmp/corpus-sysutils.pas").expect("provider URI");
    let consumer_uri =
        Url::parse("file:///tmp/corpus-exception-handler.pas").expect("consumer URI");
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for(&key).expect("37.0 Win32 surface table");
    let context = verified_system_context(&key, &table);
    let provider = concat!(
        "unit SysUtils;\n",
        "interface\n",
        "type Exception = class Message: string; end;\n",
        "implementation\n",
        "end.\n",
    );
    let consumer = concat!(
        "unit ExceptionHandler;\n",
        "interface\n",
        "uses SysUtils;\n",
        "implementation\n",
        "procedure Run;\n",
        "var ErrorCode: Integer;\n",
        "begin\n",
        "  try\n",
        "    ErrorCode := 0;\n",
        "  except\n",
        "    on E: Exception do\n",
        "      if E.Message = '' then ErrorCode := 1;\n",
        "  end;\n",
        "  ErrorCode := Length(E.Message);\n",
        "end;\n",
        "end.\n",
    );
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            provider_uri.clone(),
            provider.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("exception type provider parses");
    index
        .update_with_context_with_cancel(
            consumer_uri.clone(),
            consumer.to_owned(),
            &context,
            &AtomicBool::new(false),
        )
        .expect("exception handler fixture parses");
    index.bind_imports(&consumer_uri, [("SysUtils".to_owned(), provider_uri)]);
    assert!(
        index.documents[&consumer_uri]
            .parser_recovery_spans
            .is_empty(),
        "minimal exception handler fixture must parse without recovery: {:?}",
        index.documents[&consumer_uri].parser_recovery_spans
    );

    let diagnostics = index
        .semantic_diagnostics_with_cancel(&consumer_uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete");
    let exception_offset = consumer
        .find("on E: Exception do")
        .expect("exception handler")
        + "on ".len();
    let outside_exception_offset = consumer
        .rfind("E.Message")
        .expect("reference outside the exception handler");
    assert!(
        diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::Dcc(DccMessage::E2003)
            })
            .map(|diagnostic| diagnostic.span.start)
            .collect::<Vec<_>>()
            == [outside_exception_offset],
        "the handler variable is local to its handler body: {diagnostics:?}; declaration at {exception_offset}"
    );
}

#[derive(Debug, Default)]
struct ScanResult {
    files: Vec<PathBuf>,
    missing_directories: Vec<(PathBuf, &'static str, bool)>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct ExpectedCorpusSkip {
    unit: &'static str,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct CorpusVersionPolicy {
    major: u32,
    minor: u32,
    expected_absent_directories: &'static [(&'static str, &'static str)],
    expected_skips: &'static [ExpectedCorpusSkip],
}

const SYSTEM_INTERNAL_UNWINDER_REASON: &str = "System.Internal.Unwinder declares unwind only under LINUX/MACOS but references it unconditionally; the Windows corpus cannot provide that library binding";

const CORPUS_VERSION_POLICIES: &[CorpusVersionPolicy] = &[
    CorpusVersionPolicy {
        major: 21,
        minor: 0,
        expected_absent_directories: &[],
        expected_skips: &[],
    },
    CorpusVersionPolicy {
        major: 24,
        minor: 0,
        expected_absent_directories: &[],
        expected_skips: &[ExpectedCorpusSkip {
            unit: "System.Internal.Unwinder",
            reason: SYSTEM_INTERNAL_UNWINDER_REASON,
        }],
    },
    CorpusVersionPolicy {
        major: 37,
        minor: 0,
        expected_absent_directories: &[],
        expected_skips: &[],
    },
];

#[derive(Debug, Default, PartialEq, Eq)]
struct ImportCoverage {
    resolved_entries: usize,
    unresolved_entries: usize,
    units_with_active_uses: usize,
    units_with_all_active_uses_resolved: usize,
}

#[derive(Debug, Default)]
struct KeyReport {
    key: String,
    major: u32,
    minor: u32,
    source_roots: Vec<PathBuf>,
    files_seen: usize,
    files_indexed: usize,
    skipped_files: Vec<(PathBuf, String)>,
    lossy_files: Vec<PathBuf>,
    missing_directories: Vec<(PathBuf, &'static str, bool)>,
    import_coverage: ImportCoverage,
    duplicate_unit_names: Vec<String>,
    scan_errors: Vec<String>,
    diagnostic_errors: Vec<(PathBuf, String)>,
    e2003: Vec<String>,
    proof_counters: SemanticProofCounters,
}

fn corpus_version_policy(major: u32, minor: u32) -> Option<&'static CorpusVersionPolicy> {
    CORPUS_VERSION_POLICIES
        .iter()
        .find(|policy| policy.major == major && policy.minor == minor)
}

fn directory_absence_reason(
    policy: &CorpusVersionPolicy,
    relative_directory: &str,
) -> Option<&'static str> {
    policy
        .expected_absent_directories
        .iter()
        .find(|(directory, _)| *directory == relative_directory)
        .map(|(_, reason)| *reason)
}

fn expected_absent_directory_reason(
    major: u32,
    minor: u32,
    relative_directory: &str,
) -> Option<&'static str> {
    corpus_version_policy(major, minor)
        .and_then(|policy| directory_absence_reason(policy, relative_directory))
}

fn expected_corpus_skip(
    major: u32,
    minor: u32,
    unit_name: &str,
) -> Option<&'static ExpectedCorpusSkip> {
    corpus_version_policy(major, minor)?
        .expected_skips
        .iter()
        .find(|skip| skip.unit.eq_ignore_ascii_case(unit_name))
}

fn is_expected_corpus_skip(major: u32, minor: u32, unit_name: &str, reason: &str) -> bool {
    expected_corpus_skip(major, minor, unit_name).is_some_and(|expected| expected.reason == reason)
}

fn record_import_coverage(
    document: &super::Document,
    unique_units: &HashMap<String, Url>,
    coverage: &mut ImportCoverage,
) {
    let mut active_entries = 0;
    let mut every_active_entry_resolved = true;
    for import in &document.imports {
        if document
            .conditionals
            .inactive_spans
            .iter()
            .any(|span| span.start <= import.span.start && import.span.end <= span.end)
        {
            continue;
        }

        // Unknown activity remains a potential active entry and is counted as
        // unresolved rather than silently omitted from the coverage summary.
        active_entries += 1;
        let activity_is_known = document.conditionals.complete
            && !document
                .conditionals
                .unknown_spans
                .iter()
                .any(|span| span.start <= import.span.start && import.span.end <= span.end);
        let provider_is_unique = unique_units.contains_key(&import.name.to_ascii_lowercase());
        if activity_is_known && provider_is_unique {
            coverage.resolved_entries += 1;
        } else {
            coverage.unresolved_entries += 1;
            every_active_entry_resolved = false;
        }
    }

    if active_entries > 0 {
        coverage.units_with_active_uses += 1;
        if every_active_entry_resolved {
            coverage.units_with_all_active_uses_resolved += 1;
        }
    }
}

#[test]
#[ignore = "requires installed Delphi RTL/VCL sources; see tests/dcc_oracle/README.md"]
fn corpus_reports_no_e2003_on_dcc_clean_rtl() {
    let mut reports = Vec::new();
    let mut configured_keys = 0;

    for (major, minor, platforms) in [
        (21, 0, &[SurfacePlatform::Win32][..]),
        (24, 0, &[SurfacePlatform::Win32, SurfacePlatform::Win64][..]),
        (37, 0, &[SurfacePlatform::Win32, SurfacePlatform::Win64][..]),
    ] {
        let version = format!("{major}_{minor}");
        let variable = format!("PASCAL_RTL_{version}");
        let Some(root) = configured_path(&variable) else {
            continue;
        };
        for platform in platforms {
            configured_keys += 1;
            reports.push(check_key(major, minor, *platform, &root));
        }
    }

    assert_eq!(
        configured_keys, 5,
        "set PASCAL_RTL_21_0, PASCAL_RTL_24_0, and PASCAL_RTL_37_0 to cover all five Windows keys"
    );

    let mut failures = Vec::new();
    for report in &reports {
        println!(
            "{}: indexed={} seen={} skipped={} lossy_utf8={} E2003={} uses(resolved={}, unresolved={}) units_all_active_uses_resolved={}/{} proofs(resolved={}, proven_absent={}, table_backed={})",
            report.key,
            report.files_indexed,
            report.files_seen,
            report.skipped_files.len(),
            report.lossy_files.len(),
            report.e2003.len(),
            report.import_coverage.resolved_entries,
            report.import_coverage.unresolved_entries,
            report.import_coverage.units_with_all_active_uses_resolved,
            report.import_coverage.units_with_active_uses,
            report.proof_counters.resolved,
            report.proof_counters.proven_absent,
            report.proof_counters.table_backed_implicit_system
        );
        println!("  source roots: {}", display_paths(&report.source_roots));
        let policy = corpus_version_policy(report.major, report.minor)
            .expect("every scanned compiler version has an explicit corpus policy");
        for (path, relative, was_not_found) in &report.missing_directories {
            if *was_not_found && let Some(reason) = directory_absence_reason(policy, relative) {
                println!(
                    "  expected absent standard directory {}: {reason}",
                    path.display()
                );
            } else {
                println!("  required standard directory missing: {}", path.display());
                failures.push(format!(
                    "{} required standard directory missing: {}",
                    report.key,
                    path.display()
                ));
            }
        }
        for path in &report.lossy_files {
            println!("  decoded lossily: {}", path.display());
        }
        let mut observed_expected_skips = HashSet::new();
        for (path, reason) in &report.skipped_files {
            let unit_name = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unknown>".to_owned());
            if is_expected_corpus_skip(report.major, report.minor, &unit_name, reason) {
                println!("  expected skip {} ({unit_name}): {reason}", path.display());
                let expected = expected_corpus_skip(report.major, report.minor, &unit_name)
                    .expect("matched skip has a policy entry");
                if !observed_expected_skips.insert(expected.unit) {
                    failures.push(format!(
                        "{} expected skip {} was recorded more than once",
                        report.key, expected.unit
                    ));
                }
            } else {
                println!(
                    "  unexpected skip {} ({unit_name}): {reason}",
                    path.display()
                );
                failures.push(format!(
                    "{} unexpected skipped file {} ({unit_name}): {reason}",
                    report.key,
                    path.display()
                ));
            }
        }
        for expected in policy.expected_skips {
            if !observed_expected_skips.contains(expected.unit) {
                failures.push(format!(
                    "{} expected skip {} was not observed ({})",
                    report.key, expected.unit, expected.reason
                ));
            }
        }
        for name in &report.duplicate_unit_names {
            println!("  ambiguous unit stem left unbound: {name}");
        }
        for error in &report.scan_errors {
            println!("  corpus scan error: {error}");
            failures.push(format!("{} scan error: {error}", report.key));
        }
        for (path, error) in &report.diagnostic_errors {
            println!("  diagnostics skipped {}: {error}", path.display());
            failures.push(format!(
                "{} diagnostics failed for {}: {error}",
                report.key,
                path.display()
            ));
        }
        for finding in &report.e2003 {
            println!("  E2003 {finding}");
            failures.push(format!("{} E2003 {finding}", report.key));
        }
        if report.files_indexed == 0 {
            failures.push(format!("{} indexed no files", report.key));
        }
        if report.proof_counters.table_backed_implicit_system == 0 {
            failures.push(format!(
                "{} made no table-backed implicit System decisions",
                report.key
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "corpus precision failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn semantic_diagnostics_allow_bootstrap_exports_from_hash_verified_incomplete_system() {
    let key = SurfaceKey {
        major: 24,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let mut table = table_for(&key).expect("24.0 Win32 table").as_ref().clone();
    let system = concat!(
        "unit System;\n",
        "interface\n",
        "{$IFDEF MSWINDOWS}\n",
        "procedure _StartExe;\n",
        "{$ENDIF MSWINDOWS}\n",
        "implementation\n",
        "end.\n",
        "{$IFDEF INCOMPLETE_SYSTEM_TAIL}\n",
    );
    table.system_blake3 = super::dcc_surface::blake3_hex(system.as_bytes());
    assert!(!table.has_name("_StartExe"));

    super::dcc_surface::with_test_table(key.clone(), table, || {
        let table = table_for(&key).expect("injected test table");
        let context = verified_system_context(&key, &table);
        let system_uri = Url::parse("file:///tmp/System.pas").unwrap();
        let sysinit_uri = Url::parse("file:///tmp/SysInit.pas").unwrap();
        let regular_uri = Url::parse("file:///tmp/Regular.pas").unwrap();
        let sysinit = concat!(
            "unit SysInit;\n",
            "interface\n",
            "implementation\n",
            "procedure Start;\n",
            "begin\n",
            "  _StartExe;\n",
            "end;\n",
            "end.\n",
        );
        let regular = concat!(
            "unit Regular;\n",
            "interface\n",
            "implementation\n",
            "procedure Start;\n",
            "begin\n",
            "  _StartExe;\n",
            "end;\n",
            "end.\n",
        );
        let mut index = NavigationIndex::new();
        for (uri, source) in [
            (&system_uri, system),
            (&sysinit_uri, sysinit),
            (&regular_uri, regular),
        ] {
            index
                .update_with_context_with_cancel(
                    uri.clone(),
                    source.to_owned(),
                    &context,
                    &AtomicBool::new(false),
                )
                .expect("bootstrap fixture parses");
        }
        assert_eq!(
            index.documents[&system_uri].verified_system_surface,
            Some(key.clone()),
            "the System source hash is verified for the exact compiler/platform key"
        );
        assert!(
            !index.documents[&system_uri]
                .conditionals
                .unknown_spans
                .is_empty(),
            "the regression requires an incomplete System conditional analysis"
        );

        let diagnostics = index
            .semantic_diagnostics_with_cancel(&sysinit_uri, &AtomicBool::new(false))
            .expect("SysInit diagnostics complete");
        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003)
            }),
            "hash-verified System bootstrap declaration must not cause E2003: {diagnostics:?}"
        );
        let offset = sysinit.find("_StartExe").expect("bootstrap call");
        let position = super::text::offset_to_position(sysinit, offset).expect("call position");
        assert_eq!(
            index
                .semantic_proof_status_at_with_cancel(
                    &sysinit_uri,
                    position,
                    &AtomicBool::new(false),
                )
                .expect("bootstrap proof status"),
            SemanticProofStatus::Resolved
        );

        let regular_diagnostics = index
            .semantic_diagnostics_with_cancel(&regular_uri, &AtomicBool::new(false))
            .expect("ordinary unit diagnostics complete");
        assert_eq!(
            regular_diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::Dcc(DccMessage::E2003)
                })
                .count(),
            1,
            "the compiler-hidden export must remain absent from ordinary units"
        );

        let stale_system = format!("{system}// changed after table generation\n");
        let mut stale_index = NavigationIndex::new();
        for (uri, source) in [
            (&system_uri, stale_system.as_str()),
            (&sysinit_uri, sysinit),
        ] {
            stale_index
                .update_with_context_with_cancel(
                    uri.clone(),
                    source.to_owned(),
                    &context,
                    &AtomicBool::new(false),
                )
                .expect("changed-source fixture parses");
        }
        assert!(
            stale_index.documents[&system_uri]
                .verified_system_surface
                .is_none(),
            "a source hash mismatch must not inherit bootstrap trust"
        );
        let stale_diagnostics = stale_index
            .semantic_diagnostics_with_cancel(&sysinit_uri, &AtomicBool::new(false))
            .expect("changed-source SysInit diagnostics complete");
        assert_eq!(
            stale_diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::Dcc(DccMessage::E2003)
                })
                .count(),
            1,
            "the changed System source remains fail-closed for SysInit"
        );
    });
}

fn check_key(major: u32, minor: u32, platform: SurfacePlatform, root: &Path) -> KeyReport {
    let key = SurfaceKey {
        major,
        minor,
        platform,
    };
    let label = key.file_stem();
    let Some(table) = table_for(&key) else {
        return KeyReport {
            key: label,
            major,
            minor,
            scan_errors: vec!["no checked-in dcc surface table for key".to_owned()],
            ..KeyReport::default()
        };
    };
    let context = verified_system_context(&key, &table);
    let mut report = KeyReport {
        key: label,
        major,
        minor,
        ..KeyReport::default()
    };
    let mut scan = ScanResult::default();

    for relative in CORPUS_DIRECTORIES {
        let directory = root.join(relative);
        let expected_absence = expected_absent_directory_reason(major, minor, relative);
        match fs::metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {
                if let Some(reason) = expected_absence {
                    scan.errors.push(format!(
                        "{} is present but version policy declares it absent: {reason}",
                        directory.display()
                    ));
                } else {
                    collect_pascal_files(&directory, &mut scan);
                }
            }
            Ok(_) => scan.missing_directories.push((directory, relative, false)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                scan.missing_directories.push((directory, relative, true));
            }
            Err(error) => scan
                .errors
                .push(format!("{}: {error}", directory.display())),
        }
    }

    let corpus_variable = format!("PASCAL_CORPUS_{major}_{minor}");
    if let Some(corpus_root) = configured_path(&corpus_variable) {
        report.source_roots.push(corpus_root.clone());
        match fs::metadata(&corpus_root) {
            Ok(metadata) if metadata.is_dir() => collect_pascal_files(&corpus_root, &mut scan),
            Ok(_) => scan.errors.push(format!(
                "{} is not a directory (from {corpus_variable})",
                corpus_root.display()
            )),
            Err(error) => scan.errors.push(format!(
                "{} from {corpus_variable}: {error}",
                corpus_root.display()
            )),
        }
    }

    scan.files.sort();
    scan.files.dedup();
    report.source_roots.insert(0, root.to_path_buf());
    report.files_seen = scan.files.len();
    report.missing_directories = scan.missing_directories;
    report.scan_errors = scan.errors;

    let cancel = AtomicBool::new(false);
    let mut index = NavigationIndex::new();
    let mut indexed = Vec::<(Url, PathBuf, String)>::new();
    let mut unit_urls = HashMap::<String, Vec<Url>>::new();

    for path in scan.files {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                report
                    .skipped_files
                    .push((path, format!("read failed: {error}")));
                continue;
            }
        };
        if std::str::from_utf8(&bytes).is_err() {
            report.lossy_files.push(path.clone());
        }
        let source = String::from_utf8_lossy(&bytes).into_owned();
        let uri = match Url::from_file_path(&path) {
            Ok(uri) => uri,
            Err(()) => {
                report
                    .skipped_files
                    .push((path, "path cannot be represented as a file URI".to_owned()));
                continue;
            }
        };
        if let Err(error) =
            index.update_with_context_with_cancel(uri.clone(), source, &context, &cancel)
        {
            report
                .skipped_files
                .push((path, format!("indexing failed: {error}")));
            continue;
        }
        let Some(unit_name) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            report
                .skipped_files
                .push((path, "file has no unit-name stem".to_owned()));
            continue;
        };
        unit_urls
            .entry(unit_name.to_ascii_lowercase())
            .or_default()
            .push(uri.clone());
        indexed.push((uri, path, unit_name));
    }
    report.files_indexed = indexed.len();

    let mut unique_units = HashMap::<String, Url>::new();
    for (name, uris) in unit_urls {
        if uris.len() == 1 {
            unique_units.insert(name, uris[0].clone());
        } else {
            report.duplicate_unit_names.push(name);
        }
    }
    report.duplicate_unit_names.sort();

    for (uri, _, _) in &indexed {
        let Some(document) = index.documents.get(uri) else {
            continue;
        };
        record_import_coverage(document, &unique_units, &mut report.import_coverage);
        let imports = document
            .interface_uses
            .iter()
            .chain(&document.implementation_uses)
            .cloned()
            .collect::<HashSet<_>>();
        let bindings = imports
            .into_iter()
            .filter_map(|name| {
                unique_units
                    .get(&name.to_ascii_lowercase())
                    .map(|provider| (name, provider.clone()))
            })
            .collect::<Vec<_>>();
        index.bind_imports(uri, bindings);
    }

    super::test_reset_semantic_proof_counters();
    for (uri, path, _) in &indexed {
        let Some(document) = index.documents.get(uri) else {
            report.diagnostic_errors.push((
                path.clone(),
                "indexed document disappeared before diagnostics".to_owned(),
            ));
            continue;
        };
        if let Some(expected) = expected_corpus_skip(major, minor, &document.unit_name) {
            report
                .skipped_files
                .push((path.clone(), expected.reason.to_owned()));
            continue;
        }
        match index.semantic_diagnostics_with_cancel(uri, &cancel) {
            Ok(diagnostics) => {
                let source = document.source.as_ref();
                for diagnostic in diagnostics {
                    if diagnostic.kind != SemanticDiagnosticKind::Dcc(DccMessage::E2003) {
                        continue;
                    }
                    let offset = diagnostic.span.start.min(source.len());
                    let line = source[..offset].matches('\n').count() + 1;
                    report
                        .e2003
                        .push(format!("{}:{line}: {}", path.display(), diagnostic.message));
                }
            }
            Err(error) => report.diagnostic_errors.push((path.clone(), error)),
        }
    }
    report.proof_counters = super::test_semantic_proof_counters();
    report
}

#[test]
fn corpus_policy_requires_standard_directories_and_scopes_skips_by_version_unit_and_reason() {
    for (major, minor) in [(21, 0), (24, 0), (37, 0)] {
        for directory in CORPUS_DIRECTORIES {
            assert_eq!(
                expected_absent_directory_reason(major, minor, directory),
                None,
                "{major}.{minor} requires standard directory {directory} unless policy records a reason"
            );
        }
    }
    let explicitly_absent_directory = CorpusVersionPolicy {
        major: 99,
        minor: 0,
        expected_absent_directories: &[("rtl/win", "this install does not ship Windows RTL")],
        expected_skips: &[],
    };
    assert_eq!(
        directory_absence_reason(&explicitly_absent_directory, "rtl/win"),
        Some("this install does not ship Windows RTL")
    );

    let reason = "System.Internal.Unwinder declares unwind only under LINUX/MACOS but references it unconditionally; the Windows corpus cannot provide that library binding";
    assert!(is_expected_corpus_skip(
        24,
        0,
        "System.Internal.Unwinder",
        reason
    ));
    assert!(!is_expected_corpus_skip(24, 0, "Unwinder", reason));
    assert!(!is_expected_corpus_skip(
        24,
        0,
        "System.Internal.Unwinder",
        "different reason"
    ));
    assert!(!is_expected_corpus_skip(
        21,
        0,
        "System.Internal.Unwinder",
        reason
    ));
}

#[test]
fn corpus_import_coverage_counts_entries_and_units_with_all_imports_resolved() {
    let provider_uri = Url::parse("file:///tmp/coverage-provider.pas").expect("provider URI");
    let other_uri = Url::parse("file:///tmp/coverage-other.pas").expect("other URI");
    let complete_uri = Url::parse("file:///tmp/coverage-complete.pas").expect("complete URI");
    let partial_uri = Url::parse("file:///tmp/coverage-partial.pas").expect("partial URI");
    let mut index = NavigationIndex::new();
    for (uri, source) in [
        (
            provider_uri.clone(),
            "unit Provider;\ninterface\nimplementation\nend.\n",
        ),
        (
            other_uri.clone(),
            "unit Other;\ninterface\nimplementation\nend.\n",
        ),
        (
            complete_uri.clone(),
            "unit Complete;\ninterface\nuses Provider;\nimplementation\nuses Other;\nend.\n",
        ),
        (
            partial_uri.clone(),
            "unit Partial;\ninterface\nuses Provider, Missing;\nimplementation\nend.\n",
        ),
    ] {
        index
            .update(uri, source.to_owned())
            .expect("coverage fixture parses");
    }
    let unique_units = HashMap::from([
        ("provider".to_owned(), provider_uri),
        ("other".to_owned(), other_uri),
    ]);
    let mut coverage = ImportCoverage::default();
    for uri in [&complete_uri, &partial_uri] {
        record_import_coverage(
            index.documents.get(uri).expect("coverage unit indexed"),
            &unique_units,
            &mut coverage,
        );
    }

    assert_eq!(coverage.resolved_entries, 3);
    assert_eq!(coverage.unresolved_entries, 1);
    assert_eq!(coverage.units_with_active_uses, 2);
    assert_eq!(coverage.units_with_all_active_uses_resolved, 1);
}

fn configured_path(variable: &str) -> Option<PathBuf> {
    let value = std::env::var_os(variable)?;
    let path = PathBuf::from(value);
    let path = if path == Path::new("~") {
        std::env::var_os("HOME").map(PathBuf::from)?
    } else if let Ok(suffix) = path.strip_prefix("~") {
        std::env::var_os("HOME").map(PathBuf::from)?.join(suffix)
    } else {
        path
    };
    Some(if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    })
}

fn collect_pascal_files(directory: &Path, result: &mut ScanResult) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            result
                .errors
                .push(format!("{}: {error}", directory.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.errors.push(format!(
                    "directory entry in {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                result.errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        if file_type.is_dir() {
            collect_pascal_files(&path, result);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pas"))
        {
            result.files.push(path);
        }
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
