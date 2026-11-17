use super::dcc_surface::{
    SurfaceKey, SurfacePlatform, SurfaceTable, blake3_hex, table_for, verified_system_context,
    verified_system_key, with_test_table,
};
use super::*;
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, TargetPlatform,
};

const SYSTEM_SOURCE: &str = "unit System;\n\
interface\n\
{$IFDEF NOT_PREDEFINED}\n\
type TOnlyWhenOpen = Integer;\n\
{$ENDIF}\n\
type TObject = class\n\
  procedure Present;\n\
end;\n\
implementation\n\
procedure TObject.Present; begin end;\n\
end.\n";

const CONSUMER_SOURCE: &str = "unit SystemTrustConsumer;\n\
interface\n\
uses System;\n\
type TFoo = class(TObject) end;\n\
implementation\n\
procedure Probe;\n\
var Foo: TFoo;\n\
begin\n\
  Foo.Present;\n\
  Foo.Nope;\n\
end;\n\
end.\n";

fn context(platform: TargetPlatform) -> ConditionalContext {
    let version = CompilerVersion::new(37, 0);
    ConditionalContext {
        compiler_version: Some(version),
        layout: LayoutContext::for_target(Some(version), Some(&platform)),
        ..ConditionalContext::default()
    }
}

fn table_for_system(source: &str) -> SurfaceTable {
    let text = format!(
        "# dcc-surface v1\n\
# banner: Embarcadero Delphi for Win32 compiler version 37.0\n\
# script-version: 1\n\
# system-blake3: {}\n\
# sysinit-blake3: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
# candidates: 3\n\
[surface]\n\
Integer\n\
TObject\n\
[defines]\n\
MSWINDOWS\n\
[members TObject]\n\
Create\n\
DisposeOf\n\
Free\n\
Present\n\
[members IInterface]\n\
QueryInterface\n\
[members IUnknown]\n\
QueryInterface\n\
[members TInterfacedObject]\n\
_AddRef\n",
        blake3_hex(source.as_bytes())
    );
    SurfaceTable::parse(&text).expect("synthetic surface table")
}

fn context_without_surface_table() -> ConditionalContext {
    let version = CompilerVersion::new(38, 0);
    ConditionalContext {
        compiler_version: Some(version),
        layout: LayoutContext::for_target(Some(version), Some(&TargetPlatform::Win32)),
        ..ConditionalContext::default()
    }
}

fn update_probe(source: &str, context: &ConditionalContext) -> (NavigationIndex, Url) {
    let uri = Url::parse("file:///system-trust-tests/RootProbe.pas").unwrap();
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            context,
            &AtomicBool::new(false),
        )
        .expect("root probe indexes");
    (index, uri)
}

fn with_context_table<R>(context: &ConditionalContext, body: impl FnOnce() -> R) -> R {
    let key = SurfaceKey::from_context(context).expect("test context has a surface key");
    with_test_table(key, table_for_system(SYSTEM_SOURCE), body)
}

fn cross_document_ancestor_probe(
    provider_context: &ConditionalContext,
    context: &ConditionalContext,
) -> (NavigationIndex, Url) {
    let provider_uri = Url::parse("file:///system-trust-tests/AncestorProvider.pas").unwrap();
    let facade_uri = Url::parse("file:///system-trust-tests/AncestorFacade.pas").unwrap();
    let consumer_uri = Url::parse("file:///system-trust-tests/AncestorConsumer.pas").unwrap();
    let provider = concat!(
        "unit AncestorProvider;\n",
        "interface\n",
        "{$IFDEF UNKNOWN_PARENT_MEMBERS}\n",
        "const MaybePresent = 1;\n",
        "{$ENDIF}\n",
        "type TParent = class end;\n",
        "implementation\n",
        "end.\n",
    );
    let facade = concat!(
        "unit AncestorFacade;\n",
        "interface\n",
        "uses AncestorProvider;\n",
        "type TBase = class(TParent) end;\n",
        "implementation\n",
        "end.\n",
    );
    let consumer = concat!(
        "unit AncestorConsumer;\n",
        "interface\n",
        "uses AncestorFacade;\n",
        "type TFoo = class(TBase) end;\n",
        "implementation\n",
        "procedure Probe;\n",
        "var Foo: TFoo;\n",
        "begin\n",
        "  Foo.Nope;\n",
        "end;\n",
        "end.\n",
    );

    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            provider_uri.clone(),
            provider.to_owned(),
            provider_context,
            &AtomicBool::new(false),
        )
        .expect("ancestor provider indexes");
    index
        .update_with_context_with_cancel(
            facade_uri.clone(),
            facade.to_owned(),
            context,
            &AtomicBool::new(false),
        )
        .expect("ancestor facade indexes");
    index
        .update_with_context_with_cancel(
            consumer_uri.clone(),
            consumer.to_owned(),
            context,
            &AtomicBool::new(false),
        )
        .expect("ancestor consumer indexes");
    index.bind_imports(
        &facade_uri,
        std::iter::once(("AncestorProvider".to_owned(), provider_uri)),
    );
    index.bind_imports(
        &consumer_uri,
        std::iter::once(("AncestorFacade".to_owned(), facade_uri)),
    );
    (index, consumer_uri)
}

fn direct_ancestor_probe(
    provider_context: &ConditionalContext,
    context: &ConditionalContext,
) -> (NavigationIndex, Url) {
    let provider_uri = Url::parse("file:///system-trust-tests/DirectAncestorProvider.pas").unwrap();
    let consumer_uri = Url::parse("file:///system-trust-tests/DirectAncestorConsumer.pas").unwrap();
    let provider = concat!(
        "unit DirectAncestorProvider;\n",
        "interface\n",
        "{$IFDEF UNKNOWN_BASE_MEMBERS}\n",
        "const MaybePresent = 1;\n",
        "{$ENDIF}\n",
        "type TBase = class end;\n",
        "implementation\n",
        "end.\n",
    );
    let consumer = concat!(
        "unit DirectAncestorConsumer;\n",
        "interface\n",
        "uses DirectAncestorProvider;\n",
        "type TFoo = class(TBase) end;\n",
        "implementation\n",
        "procedure Probe;\n",
        "var Foo: TFoo;\n",
        "begin\n",
        "  Foo.Nope;\n",
        "end;\n",
        "end.\n",
    );

    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            provider_uri.clone(),
            provider.to_owned(),
            provider_context,
            &AtomicBool::new(false),
        )
        .expect("direct ancestor provider indexes");
    index
        .update_with_context_with_cancel(
            consumer_uri.clone(),
            consumer.to_owned(),
            context,
            &AtomicBool::new(false),
        )
        .expect("direct ancestor consumer indexes");
    index.bind_imports(
        &consumer_uri,
        std::iter::once(("DirectAncestorProvider".to_owned(), provider_uri)),
    );
    (index, consumer_uri)
}

fn update_system_and_consumer(
    system_source: &str,
    context: &ConditionalContext,
) -> (NavigationIndex, Url, Url) {
    let system_uri = Url::parse("file:///system-trust-tests/System.pas").unwrap();
    let consumer_uri = Url::parse("file:///system-trust-tests/Consumer.pas").unwrap();
    let cancel = AtomicBool::new(false);
    let mut index = NavigationIndex::new();
    index
        .update_with_context_with_cancel(
            system_uri.clone(),
            system_source.to_owned(),
            context,
            &cancel,
        )
        .expect("System source indexes");
    index
        .update_with_context_with_cancel(
            consumer_uri.clone(),
            CONSUMER_SOURCE.to_owned(),
            context,
            &cancel,
        )
        .expect("consumer source indexes");
    index.bind_imports(
        &consumer_uri,
        std::iter::once(("System".to_owned(), system_uri.clone())),
    );
    (index, system_uri, consumer_uri)
}

fn e2003(diagnostics: &[SemanticDiagnostic]) -> Vec<&SemanticDiagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind.code() == "E2003")
        .collect()
}

fn diagnostics(index: &NavigationIndex, uri: &Url) -> Vec<SemanticDiagnostic> {
    index
        .semantic_diagnostics_with_cancel(uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete")
}

fn proof_status(
    index: &NavigationIndex,
    uri: &Url,
    source: &str,
    name: &str,
) -> SemanticProofStatus {
    let offset = source
        .rfind(name)
        .unwrap_or_else(|| panic!("missing {name:?} in probe"));
    index
        .semantic_proof_status_at_with_cancel(
            uri,
            text::offset_to_position(source, offset).expect("proof position"),
            &AtomicBool::new(false),
        )
        .expect("semantic proof status")
}

fn span_description(source: &str, start: usize, end: usize) -> String {
    let line_number = source.as_bytes()[..start.min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    let line = source
        .lines()
        .nth(line_number - 1)
        .unwrap_or_default()
        .trim();
    format!("line {line_number} bytes {start}..{end}: {line}")
}

fn rtl_root(version: &str) -> Option<std::path::PathBuf> {
    let variable = format!("PASCAL_RTL_{}", version.replace('.', "_"));
    let root = std::env::var_os(&variable).or_else(|| {
        (version == "37.0")
            .then(|| std::env::var_os("PASCAL_RTL"))
            .flatten()
    })?;
    let root = std::path::PathBuf::from(root);
    Some(root.strip_prefix("~").map_or(root.clone(), |suffix| {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join(suffix)
    }))
}

fn member_probe_source(unit_name: &str, type_name: &str, members: &[String]) -> String {
    let calls = members
        .iter()
        .map(|member| format!("  V.{member};\n"))
        .collect::<String>();
    if matches!(type_name, "tobject" | "tinterfacedobject") {
        let base = if type_name == "tobject" {
            "TObject"
        } else {
            "TInterfacedObject"
        };
        format!(
            "unit {unit_name};\ninterface\nuses System;\ntype\n  TProbe = class({base})\n    procedure Check;\n  end;\nimplementation\nprocedure TProbe.Check;\nvar V: {type_name};\nbegin\n{calls}end;\nend.\n"
        )
    } else {
        format!(
            "unit {unit_name};\ninterface\nuses System;\nimplementation\nprocedure Check;\nvar V: {type_name};\nbegin\n{calls}end;\nend.\n"
        )
    }
}

#[test]
fn hash_matched_system_is_closed_and_proves_missing_root_members() {
    let context = context(TargetPlatform::Win32);
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for_system(SYSTEM_SOURCE);

    with_test_table(key, table, || {
        let (index, system_uri, consumer_uri) = update_system_and_consumer(SYSTEM_SOURCE, &context);
        let parsed_system = index.parsed_document(&system_uri).expect("System parsed");
        let inactive_member = SYSTEM_SOURCE.find("TOnlyWhenOpen").unwrap();
        assert!(
            parsed_system
                .conditionals
                .inactive_spans
                .iter()
                .any(|span| { span.start <= inactive_member && inactive_member < span.end }),
            "the non-predefined conditional must be inactive under the verified context"
        );
        assert!(parsed_system.conditionals.unknown_spans.is_empty());
        assert!(parsed_system.parser_recovery_spans.is_empty());

        let diagnostics = diagnostics(&index, &consumer_uri);
        let findings = e2003(&diagnostics);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    });
}

#[test]
fn parentless_class_uses_the_table_for_implicit_tobject_members() {
    let context = context(TargetPlatform::Win32);
    let source = concat!(
        "unit ImplicitRootTableConsumer;\n",
        "interface\n",
        "type TFoo = class end;\n",
        "implementation\n",
        "procedure Probe;\n",
        "var Foo: TFoo;\n",
        "begin\n",
        "  Foo.DisposeOf;\n",
        "  Foo.Nope;\n",
        "end;\n",
        "end.\n",
    );

    let (index, uri) = update_probe(source, &context);
    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));
    let findings = e2003(&diagnostics);

    assert_eq!(findings.len(), 1, "{diagnostics:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    assert_eq!(
        with_context_table(&context, || proof_status(&index, &uri, source, "DisposeOf")),
        SemanticProofStatus::Resolved
    );
    assert_eq!(
        with_context_table(&context, || proof_status(&index, &uri, source, "Nope")),
        SemanticProofStatus::ProvenAbsent
    );
}

#[test]
fn parentless_class_without_a_surface_table_keeps_root_member_proofs_incomplete() {
    let context = context_without_surface_table();
    let source = concat!(
        "unit ImplicitRootNoTableConsumer;\n",
        "interface\n",
        "type TFoo = class end;\n",
        "implementation\n",
        "procedure Probe;\n",
        "var Foo: TFoo;\n",
        "begin\n",
        "  Foo.DisposeOf;\n",
        "  Foo.Nope;\n",
        "end;\n",
        "end.\n",
    );

    let (index, uri) = update_probe(source, &context);
    let diagnostics = diagnostics(&index, &uri);
    let findings = e2003(&diagnostics);

    assert!(
        findings.is_empty(),
        "without a root member table, neither presence nor absence is provable: {findings:?}"
    );
    assert_eq!(
        proof_status(&index, &uri, source, "DisposeOf"),
        SemanticProofStatus::Incomplete
    );
    assert_eq!(
        proof_status(&index, &uri, source, "Nope"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn incomplete_transitive_ancestor_provider_cannot_prove_member_absence() {
    let context = context(TargetPlatform::Win32);
    let provider_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let (index, uri) = cross_document_ancestor_probe(&provider_context, &context);
    let provider = index
        .documents
        .values()
        .find(|document| document.unit_name == "ancestorprovider")
        .expect("ancestor provider indexed");
    assert!(!provider.conditionals.unknown_spans.is_empty());
    assert!(provider.parser_recovery_spans.is_empty());

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));
    let findings = e2003(&diagnostics);

    assert!(
        findings.is_empty(),
        "the transitive ancestor provider has an unknown conditional span: {findings:?}"
    );
    let source = &index.documents.get(&uri).expect("consumer document").source;
    assert_eq!(
        proof_status(&index, &uri, source, "Nope"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn complete_transitive_ancestor_provider_allows_proving_member_absence() {
    let context = context(TargetPlatform::Win32);
    let provider_context = context.clone().with_absent_define(ConditionalFact::False);
    let (index, uri) = cross_document_ancestor_probe(&provider_context, &context);
    let provider = index
        .documents
        .values()
        .find(|document| document.unit_name == "ancestorprovider")
        .expect("ancestor provider indexed");
    assert!(provider.conditionals.unknown_spans.is_empty());
    assert!(provider.parser_recovery_spans.is_empty());

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));
    let findings = e2003(&diagnostics);

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    let source = &index.documents.get(&uri).expect("consumer document").source;
    assert_eq!(
        proof_status(&index, &uri, source, "Nope"),
        SemanticProofStatus::ProvenAbsent
    );
}

#[test]
fn direct_ancestor_provider_completeness_gates_member_absence() {
    let context = context(TargetPlatform::Win32);
    let incomplete_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let (incomplete_index, incomplete_uri) = direct_ancestor_probe(&incomplete_context, &context);
    let provider = incomplete_index
        .documents
        .values()
        .find(|document| document.unit_name == "directancestorprovider")
        .expect("direct ancestor provider indexed");
    assert!(!provider.conditionals.unknown_spans.is_empty());
    assert!(provider.parser_recovery_spans.is_empty());
    let incomplete_diagnostics =
        with_context_table(&context, || diagnostics(&incomplete_index, &incomplete_uri));
    assert!(
        e2003(&incomplete_diagnostics).is_empty(),
        "an incomplete direct ancestor cannot prove absence: {incomplete_diagnostics:?}"
    );

    let complete_context = context.clone().with_absent_define(ConditionalFact::False);
    let (complete_index, complete_uri) = direct_ancestor_probe(&complete_context, &context);
    let provider = complete_index
        .documents
        .values()
        .find(|document| document.unit_name == "directancestorprovider")
        .expect("direct ancestor provider indexed");
    assert!(provider.conditionals.unknown_spans.is_empty());
    assert!(provider.parser_recovery_spans.is_empty());
    let complete_diagnostics =
        with_context_table(&context, || diagnostics(&complete_index, &complete_uri));
    let findings = e2003(&complete_diagnostics);
    assert_eq!(findings.len(), 1, "{complete_diagnostics:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
}

#[test]
fn interfaced_object_members_use_the_table_when_system_is_not_loaded() {
    let context = context(TargetPlatform::Win32);
    let source = concat!(
        "unit InterfacedObjectTableConsumer;\n",
        "interface\n",
        "type TFoo = class(TInterfacedObject) end;\n",
        "implementation\n",
        "procedure Probe;\n",
        "var Foo: TFoo;\n",
        "begin\n",
        "  Foo._AddRef;\n",
        "  Foo.Nope;\n",
        "end;\n",
        "end.\n",
    );

    let (index, uri) = update_probe(source, &context);
    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));
    let findings = e2003(&diagnostics);

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    assert_eq!(
        with_context_table(&context, || proof_status(&index, &uri, source, "_AddRef")),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn parentless_interface_uses_the_table_for_implicit_iinterface_members() {
    let context = context(TargetPlatform::Win32);
    let source = concat!(
        "unit ImplicitInterfaceTableConsumer;\n",
        "interface\n",
        "type IThing = interface end;\n",
        "implementation\n",
        "procedure Probe;\n",
        "var Value: IThing;\n",
        "begin\n",
        "  Value.QueryInterface;\n",
        "  Value.Nope;\n",
        "end;\n",
        "end.\n",
    );

    let (index, uri) = update_probe(source, &context);
    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));
    let findings = e2003(&diagnostics);

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    assert_eq!(
        with_context_table(&context, || proof_status(
            &index,
            &uri,
            source,
            "QueryInterface"
        )),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn a_system_source_hash_mismatch_falls_back_to_root_member_table() {
    let context = context(TargetPlatform::Win32);
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for_system(SYSTEM_SOURCE);
    let changed_source = format!("{SYSTEM_SOURCE} ");

    with_test_table(key, table, || {
        let (index, system_uri, consumer_uri) =
            update_system_and_consumer(&changed_source, &context);
        let parsed_system = index.parsed_document(&system_uri).expect("System parsed");
        assert!(!parsed_system.conditionals.unknown_spans.is_empty());
        let diagnostics = diagnostics(&index, &consumer_uri);
        let findings = e2003(&diagnostics);
        assert_eq!(findings.len(), 1, "{diagnostics:?}");
        assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    });
}

#[test]
fn a_system_hash_from_another_platform_does_not_close_the_parse() {
    let other_platform_context = context(TargetPlatform::Win64);
    let key = SurfaceKey {
        major: 37,
        minor: 0,
        platform: SurfacePlatform::Win32,
    };
    let table = table_for_system(SYSTEM_SOURCE);

    with_test_table(key, table, || {
        let (index, system_uri, consumer_uri) =
            update_system_and_consumer(SYSTEM_SOURCE, &other_platform_context);
        let parsed_system = index.parsed_document(&system_uri).expect("System parsed");
        assert!(!parsed_system.conditionals.unknown_spans.is_empty());
        let diagnostics = diagnostics(&index, &consumer_uri);
        let findings = e2003(&diagnostics);
        assert_eq!(findings.len(), 1, "{diagnostics:?}");
        assert_eq!(findings[0].message, "Undeclared identifier: 'Nope'");
    });
}

#[test]
#[ignore = "developer-only validation against installed Delphi RTL sources"]
fn system_pas_root_members_resolve_for_every_table() {
    let key_platforms = [
        (21, 0, SurfacePlatform::Win32),
        (24, 0, SurfacePlatform::Win32),
        (24, 0, SurfacePlatform::Win64),
        (37, 0, SurfacePlatform::Win32),
        (37, 0, SurfacePlatform::Win64),
    ];
    let configured_versions = ["21.0", "24.0", "37.0"]
        .into_iter()
        .filter_map(|version| rtl_root(version).map(|root| (version, root)))
        .collect::<Vec<_>>();
    assert!(
        !configured_versions.is_empty(),
        "set PASCAL_RTL_21_0, PASCAL_RTL_24_0, or PASCAL_RTL_37_0"
    );

    let mut failures = Vec::new();
    for (version, root) in configured_versions {
        let (major, minor) = match version {
            "21.0" => (21, 0),
            "24.0" => (24, 0),
            "37.0" => (37, 0),
            _ => unreachable!(),
        };
        let system_path = root.join("rtl/sys/System.pas");
        let sysinit_path = root.join("rtl/sys/SysInit.pas");
        let system_bytes = match std::fs::read(&system_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                failures.push(format!(
                    "{version}: cannot read {}: {error}",
                    system_path.display()
                ));
                continue;
            }
        };
        let sysinit_bytes = match std::fs::read(&sysinit_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                failures.push(format!(
                    "{version}: cannot read {}: {error}",
                    sysinit_path.display()
                ));
                continue;
            }
        };
        let system_source = match String::from_utf8(system_bytes.clone()) {
            Ok(source) if source.as_bytes() == system_bytes => source,
            Ok(_) => {
                failures.push(format!(
                    "{version}: System.pas text conversion changed bytes"
                ));
                continue;
            }
            Err(error) => {
                failures.push(format!("{version}: System.pas is not UTF-8: {error}"));
                continue;
            }
        };
        let sysinit_source = match String::from_utf8(sysinit_bytes.clone()) {
            Ok(source) if source.as_bytes() == sysinit_bytes => source,
            Ok(_) => {
                failures.push(format!(
                    "{version}: SysInit.pas text conversion changed bytes"
                ));
                continue;
            }
            Err(error) => {
                failures.push(format!("{version}: SysInit.pas is not UTF-8: {error}"));
                continue;
            }
        };
        let system_uri = Url::from_file_path(&system_path).expect("System.pas file URI");
        let sysinit_uri = Url::from_file_path(&sysinit_path).expect("SysInit.pas file URI");

        for (key_major, key_minor, surface_platform) in key_platforms
            .iter()
            .filter(|(key_major, key_minor, _)| *key_major == major && *key_minor == minor)
        {
            let key = SurfaceKey {
                major: *key_major,
                minor: *key_minor,
                platform: *surface_platform,
            };
            let Some(table) = table_for(&key) else {
                failures.push(format!("{}: missing surface table", key.file_stem()));
                continue;
            };
            let context = verified_system_context(&key, &table);
            let verified = verified_system_key(&context, &system_source);
            if verified.as_ref().map(|(verified_key, _)| verified_key) != Some(&key) {
                failures.push(format!(
                    "{}: System.pas hash mismatch (table {}, raw bytes {})",
                    key.file_stem(),
                    table.system_blake3,
                    blake3_hex(&system_bytes)
                ));
                continue;
            }

            let mut index = NavigationIndex::new();
            let cancel = AtomicBool::new(false);
            if let Err(error) = index.update_with_context_with_cancel(
                sysinit_uri.clone(),
                sysinit_source.clone(),
                &context,
                &cancel,
            ) {
                failures.push(format!(
                    "{}: SysInit.pas indexing failed: {error}",
                    key.file_stem()
                ));
                continue;
            }
            if let Err(error) = index.update_with_context_with_cancel(
                system_uri.clone(),
                system_source.clone(),
                &context,
                &cancel,
            ) {
                failures.push(format!(
                    "{}: System.pas indexing failed: {error}",
                    key.file_stem()
                ));
                continue;
            }
            index.bind_imports(
                &system_uri,
                std::iter::once(("SysInit".to_owned(), sysinit_uri.clone())),
            );
            let parsed_system = index.parsed_document(&system_uri).expect("System indexed");
            if !parsed_system.parser_recovery_spans.is_empty()
                || !parsed_system.conditionals.unknown_spans.is_empty()
            {
                let recovery = parsed_system
                    .parser_recovery_spans
                    .iter()
                    .take(8)
                    .map(|span| span_description(&system_source, span.start, span.end))
                    .collect::<Vec<_>>()
                    .join("; ");
                let unknown = parsed_system
                    .conditionals
                    .unknown_spans
                    .iter()
                    .take(8)
                    .map(|span| span_description(&system_source, span.start, span.end))
                    .collect::<Vec<_>>()
                    .join("; ");
                let unresolved_directives = parsed_system
                    .conditionals
                    .directives
                    .iter()
                    .filter(|directive| {
                        directive.kind == pascal_core::conditional::DirectiveKind::ConditionalStart
                            && (directive.condition.is_none()
                                || directive.condition == Some(ConditionalFact::Unknown))
                    })
                    .take(12)
                    .map(|directive| {
                        format!(
                            "{}: {} (condition {:?}, activity {:?})",
                            span_description(&system_source, directive.start, directive.end),
                            directive.body,
                            directive.condition,
                            directive.activity
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let active_includes = parsed_system
                    .conditionals
                    .directives
                    .iter()
                    .filter(|directive| {
                        directive.kind == pascal_core::conditional::DirectiveKind::Include
                            && directive.activity != ConditionalFact::False
                    })
                    .take(12)
                    .map(|directive| {
                        format!(
                            "{}: {}",
                            span_description(&system_source, directive.start, directive.end),
                            directive.body
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let mut conditional_stack = Vec::new();
                for directive in &parsed_system.conditionals.directives {
                    match directive.kind {
                        pascal_core::conditional::DirectiveKind::ConditionalStart => {
                            conditional_stack.push(directive)
                        }
                        pascal_core::conditional::DirectiveKind::ConditionalEnd => {
                            conditional_stack.pop();
                        }
                        _ => {}
                    }
                }
                let unclosed = conditional_stack
                    .into_iter()
                    .take(12)
                    .map(|directive| {
                        format!(
                            "{}: {}",
                            span_description(&system_source, directive.start, directive.end),
                            directive.body
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                failures.push(format!(
                    "{}: System.pas is incomplete (analysis complete {}; MSWINDOWS context {:?}, parsed {:?}; {} recovery spans: [{}]; {} unknown spans: [{}]; unknown conditions: [{}]; active includes: [{}]; unclosed conditionals: [{}])",
                    key.file_stem(),
                    parsed_system.conditionals.complete,
                    context.defines.get("MSWINDOWS"),
                    parsed_system.conditional_context.defines.get("MSWINDOWS"),
                    parsed_system.parser_recovery_spans.len(),
                    recovery,
                    parsed_system.conditionals.unknown_spans.len(),
                    unknown,
                    unresolved_directives,
                    active_includes,
                    unclosed
                ));
            }

            let mut members_by_type = Vec::new();
            for type_name in ["TObject", "IInterface", "IUnknown", "TInterfacedObject"] {
                let Some(members) = table.members(type_name) else {
                    failures.push(format!(
                        "{}: missing [members {type_name}] section",
                        key.file_stem()
                    ));
                    continue;
                };
                let mut members = members.iter().cloned().collect::<Vec<_>>();
                members.sort();
                members_by_type.push((type_name.to_ascii_lowercase(), members));
            }

            for (type_name, members) in members_by_type {
                let unit_name = format!(
                    "SystemTrustProbe{major}{minor}{}{}",
                    match surface_platform {
                        SurfacePlatform::Win32 => "Win32",
                        SurfacePlatform::Win64 => "Win64",
                    },
                    type_name
                );
                let source = member_probe_source(&unit_name, &type_name, &members);
                let uri = Url::parse(&format!("file:///system-trust-probes/{unit_name}.pas"))
                    .expect("probe URI");
                if let Err(error) =
                    index.update_with_context_with_cancel(uri.clone(), source, &context, &cancel)
                {
                    failures.push(format!(
                        "{} {type_name}: probe indexing failed: {error}",
                        key.file_stem()
                    ));
                    continue;
                }
                index.bind_imports(
                    &uri,
                    std::iter::once(("System".to_owned(), system_uri.clone())),
                );
                let diagnostics = diagnostics(&index, &uri);
                let missing = e2003(&diagnostics);
                if !missing.is_empty() {
                    failures.push(format!(
                        "{} {type_name}: table members produced E2003: {}",
                        key.file_stem(),
                        missing
                            .iter()
                            .map(|diagnostic| diagnostic.message.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let parsed_probe = index.parsed_document(&uri).expect("probe indexed");
                if !parsed_probe.parser_recovery_spans.is_empty()
                    || !parsed_probe.conditionals.unknown_spans.is_empty()
                {
                    failures.push(format!(
                        "{} {type_name}: probe incomplete ({} recovery spans, {} unknown spans)",
                        key.file_stem(),
                        parsed_probe.parser_recovery_spans.len(),
                        parsed_probe.conditionals.unknown_spans.len()
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "System.pas trust failures:\n{}",
        failures.join("\n")
    );
}
