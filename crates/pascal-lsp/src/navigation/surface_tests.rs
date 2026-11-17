use super::dcc_surface::{SurfaceKey, SurfacePlatform, SurfaceTable, with_test_table};
use super::*;
use pascal_project::{CompilerVersion, ConditionalFact, LayoutContext, TargetPlatform};

const TEST_TABLE: &str = "# dcc-surface v1\n\
# banner: Embarcadero Delphi for Win32 compiler version 37.0\n\
# script-version: 1\n\
# system-blake3: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
# sysinit-blake3: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
# candidates: 3\n\
[surface]\n\
Length\n\
TObject\n\
Writeln\n\
[defines]\n\
[members TObject]\n\
[members IInterface]\n\
[members IUnknown]\n\
[members TInterfacedObject]\n";

fn context(version: (u32, u32), platform: Option<TargetPlatform>) -> ConditionalContext {
    let compiler_version = CompilerVersion::new(version.0, version.1);
    ConditionalContext {
        compiler_version: Some(compiler_version),
        layout: LayoutContext::for_target(Some(compiler_version), platform.as_ref()),
        ..ConditionalContext::default()
    }
    .with_absent_define(ConditionalFact::False)
}

fn test_table() -> SurfaceTable {
    SurfaceTable::parse(TEST_TABLE).expect("synthetic dcc surface table")
}

fn uri(name: &str) -> Url {
    Url::parse(&format!("file:///surface-tests/{name}.pas")).expect("test URI")
}

fn update(
    index: &mut NavigationIndex,
    name: &str,
    source: &str,
    context: &ConditionalContext,
) -> Url {
    let uri = uri(name);
    index
        .update_with_context_with_cancel(
            uri.clone(),
            source.to_owned(),
            context,
            &AtomicBool::new(false),
        )
        .expect("surface test unit parses");
    uri
}

fn diagnostics(index: &NavigationIndex, uri: &Url) -> Vec<SemanticDiagnostic> {
    index
        .semantic_diagnostics_with_cancel(uri, &AtomicBool::new(false))
        .expect("semantic diagnostics complete")
}

fn with_context_table<R>(context: &ConditionalContext, body: impl FnOnce() -> R) -> R {
    let key = SurfaceKey::from_context(context).expect("test context has a surface key");
    with_test_table(key, test_table(), body)
}

fn e2003(diagnostics: &[SemanticDiagnostic]) -> Vec<&SemanticDiagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind.code() == "E2003")
        .collect()
}

fn proof_status(
    index: &NavigationIndex,
    uri: &Url,
    source: &str,
    spelling: &str,
) -> SemanticProofStatus {
    let offset = source
        .rfind(spelling)
        .unwrap_or_else(|| panic!("missing {spelling:?} in test source"));
    index
        .semantic_proof_status_at_with_cancel(
            uri,
            text::offset_to_position(source, offset).expect("identifier position"),
            &AtomicBool::new(false),
        )
        .expect("semantic proof status")
}

#[test]
fn e2003_reported_for_an_unknown_unqualified_name() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = "unit SurfaceUnknown; interface implementation procedure P; var X: Integer; begin X := ZzSurfaceAbsent; end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceUnknown", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    let findings = e2003(&diagnostics);
    assert_eq!(findings.len(), 1, "{diagnostics:?}");
    assert_eq!(
        findings[0].message,
        "Undeclared identifier: 'ZzSurfaceAbsent'"
    );
    assert_eq!(
        proof_status(&index, &uri, source, "ZzSurfaceAbsent"),
        SemanticProofStatus::ProvenAbsent
    );
}

#[test]
fn e2003_silent_for_a_table_builtin() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source =
        "unit SurfaceBuiltin; interface implementation procedure P; begin Writeln('ok'); end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceBuiltin", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
    assert_eq!(
        proof_status(&index, &uri, source, "Writeln"),
        SemanticProofStatus::Resolved
    );
}

#[test]
fn e2003_silent_without_a_table_for_the_key() {
    let context = context((21, 0), Some(TargetPlatform::Win64));
    let source = "unit SurfaceUnsupportedKey; interface implementation procedure P; begin ZzNoTable := 1; end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceUnsupportedKey", source, &context);

    let diagnostics = diagnostics(&index, &uri);

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
    assert_eq!(
        proof_status(&index, &uri, source, "ZzNoTable"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn e2003_silent_without_a_platform() {
    let context = context((37, 0), None);
    let source = "unit SurfaceNoPlatform; interface implementation procedure P; begin ZzNoPlatform := 1; end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceNoPlatform", source, &context);

    let diagnostics = diagnostics(&index, &uri);

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
    assert_eq!(
        proof_status(&index, &uri, source, "ZzNoPlatform"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn e2003_silent_when_a_used_unit_is_not_indexed() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = "unit SurfaceMissingProvider; interface implementation uses MissingProvider; procedure P; begin ZzFromUnknownProvider := 1; end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceMissingProvider", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_when_a_used_unit_has_unknown_conditionals() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let provider_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let mut index = NavigationIndex::new();
    let provider =
        "unit Provider; interface {$IFDEF MAYBE} const MaybeName = 1; {$ENDIF} implementation end.";
    let consumer = "unit SurfaceUnknownProvider; interface implementation uses Provider; procedure P; begin ZzFromUnknownProvider := 1; end; end.";
    update(&mut index, "Provider", provider, &provider_context);
    let uri = update(&mut index, "SurfaceUnknownProvider", consumer, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_when_unit_scope_alias_is_unresolved() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let mut index = NavigationIndex::new();
    update(
        &mut index,
        "VclForms",
        "unit Vcl.Forms; interface const KnownFormValue = 1; implementation end.",
        &context,
    );
    let source = "unit SurfaceUnresolvedAlias; interface implementation uses Forms; procedure P; begin ZzFromUnresolvedAlias := 1; end; end.";
    let uri = update(&mut index, "SurfaceUnresolvedAlias", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_inside_asm() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source =
        "unit SurfaceAsm; interface implementation procedure P; asm mov eax, ZzInsideAsm end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceAsm", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_in_an_unknown_conditional_region() {
    let context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context((37, 0), Some(TargetPlatform::Win32))
    };
    let source = "unit SurfaceUnknownConditional; interface implementation procedure P; begin {$IFDEF MAYBE} ZzConditional := 1; {$ENDIF} end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceUnknownConditional", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_after_parser_recovery() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = concat!(
        "unit SurfaceRecovery;\n",
        "interface\nimplementation\n",
        "procedure P;\nvar B: Integer;\nbegin\n",
        "  B := ;\n",
        "  ZzAfterRecovery := 1;\n",
        "end;\nend.\n",
    );
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceRecovery", source, &context);
    assert!(!index.documents[&uri].parser_recovery_spans.is_empty());

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_for_attribute_names() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = "unit SurfaceAttribute; interface implementation type TAttributed = class [MissingAttr] procedure P; end; procedure TAttributed.P; begin end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceAttribute", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_silent_inside_with_of_unknown_type() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = "unit SurfaceUnknownWith; interface implementation procedure P; begin with TObject.Create do MissingInUnknownWith := 1; end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceUnknownWith", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_reported_for_an_unknown_qualified_system_name() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = "unit SurfaceQualifiedSystem; interface implementation procedure P; var X: Integer; begin X := System.ZzQualified; end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceQualifiedSystem", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    let findings = e2003(&diagnostics);
    assert_eq!(findings.len(), 1, "{diagnostics:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'ZzQualified'");
}

#[test]
fn qualified_system_source_exports_resolve_before_surface_fallback() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let system = "unit System; interface const KnownSystem = 1; implementation end.";
    let consumer = concat!(
        "unit SurfaceQualifiedSystemSource; interface implementation ",
        "procedure P; var X: Integer; begin ",
        "X := System.KnownSystem; X := System.KnownSystem; X := System.Zz; ",
        "end; end.",
    );
    let mut index = NavigationIndex::new();
    let system_uri = update(&mut index, "System", system, &context);
    let consumer_uri = update(
        &mut index,
        "SurfaceQualifiedSystemSource",
        consumer,
        &context,
    );

    let (definition, first_reference, second_reference, references, absent_proof) =
        with_context_table(&context, || {
            let definition = index.navigate(
                &system_uri,
                text::offset_to_position(
                    system,
                    system.find("KnownSystem").expect("source declaration"),
                )
                .expect("declaration position"),
                NavigationTarget::Definition,
            );
            let first_offset = consumer
                .find("KnownSystem")
                .expect("first source reference");
            let first_reference = index.navigate(
                &consumer_uri,
                text::offset_to_position(consumer, first_offset).expect("first reference position"),
                NavigationTarget::Definition,
            );
            let second_offset = consumer
                .match_indices("KnownSystem")
                .nth(1)
                .expect("second source reference")
                .0;
            let second_reference = index.navigate(
                &consumer_uri,
                text::offset_to_position(consumer, second_offset)
                    .expect("second reference position"),
                NavigationTarget::Definition,
            );
            let references = index
                .binding_locations(
                    &consumer_uri,
                    text::offset_to_position(consumer, first_offset)
                        .expect("first reference position"),
                    true,
                )
                .expect("source-backed qualified references");
            let absent_proof = proof_status(&index, &consumer_uri, consumer, "Zz");
            (
                definition,
                first_reference,
                second_reference,
                references,
                absent_proof,
            )
        });

    assert_eq!(
        definition.len(),
        1,
        "System.KnownSystem definition should resolve to indexed System source"
    );
    assert_eq!(definition[0].uri, system_uri);
    assert_eq!(
        first_reference, definition,
        "the first qualified source reference should navigate to its definition"
    );
    assert_eq!(
        second_reference, definition,
        "the second qualified source reference should navigate to its definition"
    );
    assert_eq!(
        references
            .iter()
            .filter(|location| location.uri == consumer_uri)
            .count(),
        2,
        "source-backed reference search should retain both System.KnownSystem uses"
    );
    assert_eq!(absent_proof, SemanticProofStatus::ProvenAbsent);
}

#[test]
fn qualified_system_source_exports_survive_unknown_spans_and_table_still_proves_misses() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let system_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let system = concat!(
        "unit System;\ninterface\n",
        "{$IFDEF SOMETHING_UNKNOWN}\nconst MaybeSystem = 1;\n{$ENDIF}\n",
        "const KnownSystem = 1;\nimplementation\nend.\n",
    );
    let consumer = concat!(
        "unit SurfaceQualifiedIncompleteSystem;\ninterface\n",
        "implementation\nprocedure P; var X: Integer; begin\n",
        "X := System.KnownSystem; X := System.Zz;\nend; end.\n",
    );
    let mut index = NavigationIndex::new();
    let system_uri = update(&mut index, "System", system, &system_context);
    let consumer_uri = update(
        &mut index,
        "SurfaceQualifiedIncompleteSystem",
        consumer,
        &context,
    );
    let provider = index.documents.get(&system_uri).expect("indexed System");
    assert!(!provider.conditionals.unknown_spans.is_empty());
    assert!(
        !provider
            .conditionals
            .is_unknown_at(system.find("KnownSystem").expect("known export"))
    );

    let (definition, diagnostics, absent_proof) = with_context_table(&context, || {
        let definition = index.navigate(
            &consumer_uri,
            text::offset_to_position(
                consumer,
                consumer
                    .find("KnownSystem")
                    .expect("qualified known export"),
            )
            .expect("known export position"),
            NavigationTarget::Definition,
        );
        let diagnostics = diagnostics(&index, &consumer_uri);
        let absent_proof = proof_status(&index, &consumer_uri, consumer, "Zz");
        (definition, diagnostics, absent_proof)
    });
    let findings = e2003(&diagnostics);

    assert_eq!(
        definition.len(),
        1,
        "known System export must still navigate"
    );
    assert_eq!(definition[0].uri, system_uri);
    assert_eq!(absent_proof, SemanticProofStatus::ProvenAbsent);
    assert_eq!(findings.len(), 1, "the table proves System.Zz absent");
    assert_eq!(findings[0].message, "Undeclared identifier: 'Zz'");
}

#[test]
fn qualified_system_miss_without_importer_table_stays_incomplete_for_unknown_source() {
    let context = context((21, 0), Some(TargetPlatform::Win64));
    let system_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let system = concat!(
        "unit System;\ninterface\n",
        "{$IFDEF SOMETHING_UNKNOWN}\nconst MaybeSystem = 1;\n{$ENDIF}\n",
        "const KnownSystem = 1;\nimplementation\nend.\n",
    );
    let consumer = concat!(
        "unit SurfaceQualifiedIncompleteNoTable;\ninterface\n",
        "implementation\nprocedure P; var X: Integer; begin\n",
        "X := System.KnownSystem; X := System.Zz;\nend; end.\n",
    );
    let mut index = NavigationIndex::new();
    let system_uri = update(&mut index, "System", system, &system_context);
    let consumer_uri = update(
        &mut index,
        "SurfaceQualifiedIncompleteNoTable",
        consumer,
        &context,
    );

    let definition = index.navigate(
        &consumer_uri,
        text::offset_to_position(
            consumer,
            consumer
                .find("KnownSystem")
                .expect("qualified known export"),
        )
        .expect("known export position"),
        NavigationTarget::Definition,
    );
    let diagnostics = diagnostics(&index, &consumer_uri);

    assert_eq!(
        definition.len(),
        1,
        "known System export must still navigate"
    );
    assert_eq!(definition[0].uri, system_uri);
    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
    assert_eq!(
        proof_status(&index, &consumer_uri, consumer, "Zz"),
        SemanticProofStatus::Incomplete
    );
}

#[test]
fn qualified_imported_system_exports_survive_unknown_spans() {
    let context = context((21, 0), Some(TargetPlatform::Win64));
    let system_context = ConditionalContext {
        absent_define: ConditionalFact::Unknown,
        ..context.clone()
    };
    let system = concat!(
        "unit System;\ninterface\n",
        "{$IFDEF SOMETHING_UNKNOWN}\nconst MaybeSystem = 1;\n{$ENDIF}\n",
        "const KnownSystem = 1;\nimplementation\nend.\n",
    );
    let consumer = concat!(
        "unit SurfaceQualifiedImportedSystem;\ninterface\nimplementation\nuses System;\n",
        "procedure P; var X: Integer; begin\n",
        "X := System.KnownSystem;\nend; end.\n",
    );
    let mut index = NavigationIndex::new();
    let system_uri = update(&mut index, "System", system, &system_context);
    let consumer_uri = update(
        &mut index,
        "SurfaceQualifiedImportedSystem",
        consumer,
        &context,
    );
    index.bind_imports(
        &consumer_uri,
        std::iter::once(("System".to_owned(), system_uri.clone())),
    );
    let provider = index.documents.get(&system_uri).expect("indexed System");
    assert_eq!(provider.unit_name, "system");
    assert!(!provider.conditionals.unknown_spans.is_empty());
    assert!(
        index
            .documents
            .get(&consumer_uri)
            .expect("indexed consumer")
            .active_uses(Region::Implementation)
            .iter()
            .any(|used| used.as_str() == "system")
    );

    let definition = index.navigate(
        &consumer_uri,
        text::offset_to_position(
            consumer,
            consumer
                .find("KnownSystem")
                .expect("qualified known export"),
        )
        .expect("known export position"),
        NavigationTarget::Definition,
    );

    assert_eq!(
        definition.len(),
        1,
        "known System export must still navigate"
    );
    assert_eq!(definition[0].uri, system_uri);
}

#[test]
fn e2003_silent_for_a_qualified_system_builtin() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = "unit SurfaceQualifiedBuiltin; interface implementation procedure P; begin System.Writeln('ok'); end; end.";
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceQualifiedBuiltin", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    assert!(e2003(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn e2003_reported_once_for_a_repeated_unknown_name() {
    let context = context((37, 0), Some(TargetPlatform::Win32));
    let source = concat!(
        "unit SurfaceRepeatedName; interface implementation\n",
        "procedure P; var X: Integer; begin\n",
        "  X := ZzRepeated + ZzRepeated;\n",
        "  X := ZzRepeated;\n",
        "end; end.",
    );
    let mut index = NavigationIndex::new();
    let uri = update(&mut index, "SurfaceRepeatedName", source, &context);

    let diagnostics = with_context_table(&context, || diagnostics(&index, &uri));

    let findings = e2003(&diagnostics);
    assert_eq!(findings.len(), 1, "{diagnostics:?}");
    assert_eq!(findings[0].message, "Undeclared identifier: 'ZzRepeated'");
    assert_eq!(
        findings[0].span.start,
        source.find("ZzRepeated").expect("first occurrence")
    );
}

#[test]
fn closed_unit_receiver_scope_restores_compiler_surface_proof() {
    let mut state = ResolutionState::new();
    let scope = state.begin_receiver_lookup_scope();
    state.mark_compiler_surface_resolved();

    state.complete_receiver_lookup_as_closed_unit(scope);

    assert!(!state.compiler_surface_resolved);
}

#[test]
fn surface_key_uses_the_document_compiler_and_platform() {
    let context = context((37, 0), Some(TargetPlatform::Win32));

    let key = SurfaceKey::from_context(&context).expect("surface key");

    assert_eq!(key.platform, SurfacePlatform::Win32);
    assert_eq!(key.file_stem(), "37.0-Win32");
}
