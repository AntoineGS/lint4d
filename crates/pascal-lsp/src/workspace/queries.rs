use super::rename::{
    CANCELLATION_MESSAGE, RenameSnapshot, SnapshotMode, SnapshotSeed, WorkspaceInput,
    build_snapshot, input_source_is_readable, is_cancelled, query_binding_info_for_input,
    snapshot_records, source_for_input,
};
use crate::NavigationIndex;
use lsp_types::{DocumentHighlight, DocumentSymbol, Location, Position, SymbolInformation, Url};
use std::sync::atomic::AtomicBool;

pub(crate) fn references_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    include_declaration: bool,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<Location>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if !input_source_is_readable(&input, &uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let (_, record, _, ignored_or_empty) =
        match query_binding_info_for_input(&input, &uri, position) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if ignored_or_empty {
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            vec![record],
        );
    }

    let snapshot = match binding_snapshot(&input, &uri, position, false, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_reference_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value =
        snapshot
            .index
            .binding_locations_with_cancel(&uri, position, include_declaration, cancel);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn highlights_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<DocumentHighlight>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if !input_source_is_readable(&input, &uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let (_, record, _, ignored_or_empty) =
        match query_binding_info_for_input(&input, &uri, position) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if ignored_or_empty {
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            vec![record],
        );
    }

    let snapshot = match binding_snapshot(&input, &uri, position, true, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_document_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let locations = match snapshot
        .index
        .binding_locations_in_document_with_cancel(&uri, position, cancel)
    {
        Ok(locations) => locations,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) if error.contains("10000-entry limit") => {
            return with_records(
                source_generation,
                configuration_generation,
                Err(error),
                records,
            );
        }
        Err(_error) => {
            return with_records(
                source_generation,
                configuration_generation,
                Ok(Vec::new()),
                records,
            );
        }
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = locations
        .into_iter()
        .map(|location| DocumentHighlight {
            range: location.range,
            kind: None,
        })
        .collect();
    with_records(
        source_generation,
        configuration_generation,
        Ok(value),
        records,
    )
}

fn binding_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    document_local: bool,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    let uri = super::canonical_file_uri(uri);
    let (source, target_record, binding_info, _) =
        query_binding_info_for_input(input, &uri, position)?;
    let original_name = super::rename::identifier_at_position(&source, position)
        .ok_or_else(|| format!("no identifier at navigation position in {uri}"))?;
    let (local, mut candidate_names) = match binding_info {
        Some(info) => {
            let mut names = info.names;
            if names.is_empty() {
                names.push(original_name.clone());
            }
            (info.local, names)
        }
        None => (false, vec![original_name]),
    };
    candidate_names.sort();
    candidate_names.dedup();
    let mode = if document_local {
        if local {
            SnapshotMode::Local
        } else {
            SnapshotMode::LocalWithImports
        }
    } else if local {
        SnapshotMode::Local
    } else {
        SnapshotMode::Workspace
    };
    build_snapshot(
        input,
        std::slice::from_ref(&uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record)),
        &[],
        cancel,
    )
}

fn ensure_reference_ready(snapshot: &RenameSnapshot, uri: &Url) -> Result<(), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "reference document was not retained in the workspace snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "reference document is outside configured workspace roots: {uri}"
        ));
    }
    if let Some(error) = snapshot.include_errors.first() {
        return Err(format!("reference workspace scan incomplete: {error}"));
    }
    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .as_deref()
            .unwrap_or("bounded source discovery did not finish");
        return Err(format!("reference workspace scan incomplete: {reason}"));
    }
    Ok(())
}

fn ensure_document_ready(snapshot: &RenameSnapshot, uri: &Url) -> Result<(), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "highlight document was not retained in the local snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "highlight document is outside configured workspace roots: {uri}"
        ));
    }
    if snapshot.mode == SnapshotMode::LocalWithImports {
        if let Some(error) = snapshot.include_errors.first() {
            return Err(format!("highlight dependency scan incomplete: {error}"));
        }
        if !snapshot.complete {
            let reason = snapshot
                .incomplete_reason
                .as_deref()
                .unwrap_or("required import discovery did not finish");
            return Err(format!("highlight dependency scan incomplete: {reason}"));
        }
    }
    Ok(())
}

pub(crate) fn document_symbols_from_input(
    input: WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<DocumentSymbol>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if !input_source_is_readable(&input, &uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let (source, record) = match source_for_input(&input, &uri) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut index = NavigationIndex::new();
    if let Err(error) = index.update(uri.clone(), source) {
        return failed(
            source_generation,
            configuration_generation,
            format!("could not index document symbols for {uri}: {error}"),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = index.document_symbols_with_cancel(&uri, cancel);
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value,
        records: vec![record],
    }
}

pub(crate) fn workspace_symbols_from_input(
    input: WorkspaceInput,
    query: &str,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<SymbolInformation>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let snapshot = match build_snapshot(
        &input,
        &[],
        &[],
        SnapshotMode::WorkspaceSymbols,
        None,
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .as_deref()
            .unwrap_or("bounded source discovery did not finish");
        return failed(
            source_generation,
            configuration_generation,
            format!("workspace symbol search incomplete: {reason}"),
        );
    }

    let value = snapshot
        .index
        .workspace_symbols_with_cancel(query, cancel)
        .map(|symbols| {
            symbols
                .into_iter()
                .filter(|symbol| snapshot.readable.contains(&symbol.location.uri))
                .collect()
        });
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value,
        records: snapshot_records(&snapshot),
    }
}

fn cancelled<T>(
    source_generation: u64,
    configuration_generation: u64,
) -> super::rename::Computed<T> {
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value: Err(CANCELLATION_MESSAGE.to_string()),
        records: Vec::new(),
    }
}

fn failed<T>(
    source_generation: u64,
    configuration_generation: u64,
    error: String,
) -> super::rename::Computed<T> {
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value: Err(error),
        records: Vec::new(),
    }
}

fn with_records<T>(
    source_generation: u64,
    configuration_generation: u64,
    value: Result<T, String>,
    records: Vec<super::rename::SourceRecord>,
) -> super::rename::Computed<T> {
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value,
        records,
    }
}

#[cfg(test)]
mod tests {
    use super::{highlights_from_input, references_from_input};
    use crate::workspace::rename::{Computed, WorkspaceInput, revalidate_input};
    use crate::workspace::{Workspace, WorkspaceOptions};
    use lsp_types::{Position, Url};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    struct ReferenceFixture {
        _temp: TempDir,
        root: PathBuf,
        source: PathBuf,
        input: WorkspaceInput,
    }

    fn external_fixture() -> ReferenceFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let external = temp.path().join("library/src");
        let source = external.join("External.pas");
        let source_text =
            "unit External;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::create_dir_all(&external).expect("external source directory");
        fs::write(&source, source_text).expect("external source");

        let workspace = Workspace::new(
            vec![root.clone()],
            WorkspaceOptions {
                source_paths: vec![external.to_string_lossy().into_owned()],
                ..WorkspaceOptions::default()
            },
        );
        ReferenceFixture {
            _temp: temp,
            root,
            source,
            input: workspace.analysis_input(),
        }
    }

    fn source_uri(path: &Path) -> Url {
        Url::from_file_path(path).expect("source URI")
    }

    fn shared_value_position() -> Position {
        Position::new(2, 6)
    }

    fn compute_references(fixture: &ReferenceFixture) -> Computed<Vec<lsp_types::Location>> {
        let cancel = AtomicBool::new(false);
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.source),
            shared_value_position(),
            true,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "reference computation failed: {computed:?}"
        );
        assert!(
            !computed.records.is_empty(),
            "reference computation must retain observations"
        );
        computed
    }

    #[test]
    fn references_revalidation_observes_an_external_ancestor_project_candidate() {
        let fixture = external_fixture();
        let computed = compute_references(&fixture);
        let ancestor_project = fixture
            .root
            .parent()
            .expect("temporary parent")
            .join("library/App.dproj");
        fs::write(
            &ancestor_project,
            "<Project><PropertyGroup><MainSource>src/External.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("ancestor project");

        let cancel = AtomicBool::new(false);
        let error = revalidate_input(&fixture.input, &computed.records, &cancel).expect_err(
            "adding a project candidate above an external source root must stale references",
        );
        assert!(
            error.contains("membership") || error.contains("changed"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn references_revalidation_ignores_unrelated_external_ancestor_files() {
        let fixture = external_fixture();
        let computed = compute_references(&fixture);
        let unrelated = fixture
            .root
            .parent()
            .expect("temporary parent")
            .join("library/noise.txt");
        fs::write(&unrelated, "not a project candidate").expect("unrelated file");

        let cancel = AtomicBool::new(false);
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unrelated external ancestor files must not stale references");
    }

    struct QueryFixture {
        _temp: TempDir,
        provider: PathBuf,
        consumer: PathBuf,
        provider_source: String,
        consumer_source: String,
        input: WorkspaceInput,
    }

    fn query_fixture() -> QueryFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let provider = temp.path().join("Provider.pas");
        let consumer = temp.path().join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n".to_string();
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n".to_string();
        fs::create_dir_all(temp.path()).expect("workspace directory");
        fs::write(&provider, &provider_source).expect("provider source");
        fs::write(&consumer, &consumer_source).expect("consumer source");
        let workspace =
            Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        QueryFixture {
            _temp: temp,
            provider,
            consumer,
            provider_source,
            consumer_source,
            input: workspace.analysis_input(),
        }
    }

    #[test]
    fn references_revalidation_observes_a_new_pascal_consumer() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            true,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "reference computation: {computed:?}"
        );
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unchanged reference inputs must remain valid");

        let added_consumer = fixture
            .provider
            .parent()
            .expect("workspace directory")
            .join("AddedConsumer.pas");
        fs::write(
            &added_consumer,
            "unit AddedConsumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("new Pascal consumer");

        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("a new Pascal consumer must stale completed references");
        assert!(
            error.contains("membership") || error.contains("changed"),
            "unexpected new-consumer revalidation error: {error}"
        );
    }

    #[test]
    fn completed_reference_results_revalidate_changed_sources() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "reference worker must complete: {computed:?}"
        );
        assert!(
            !computed.records.is_empty(),
            "reference result must carry its read set"
        );

        fs::write(
            &fixture.consumer,
            fixture
                .consumer_source
                .replace("SharedValue", "ChangedValue"),
        )
        .expect("change consumer source");
        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("completed references must become stale after a source change");
        assert!(
            error.contains("changed") || error.contains("resolving"),
            "{error}"
        );
    }

    #[test]
    fn completed_highlight_results_revalidate_changed_sources() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = highlights_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "highlight worker must complete: {computed:?}"
        );
        assert_eq!(computed.value.as_ref().expect("highlights").len(), 1);
        assert!(
            !computed.records.is_empty(),
            "highlight result must carry its read set"
        );
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unchanged highlight inputs must remain valid");

        fs::write(
            &fixture.provider,
            fixture
                .provider_source
                .replace("SharedValue", "ChangedValue"),
        )
        .expect("change provider source");
        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("completed highlights must become stale after a source change");
        assert!(
            error.contains("changed") || error.contains("resolving"),
            "{error}"
        );
    }

    #[test]
    fn references_honor_preset_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(true);
        let computed = references_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn highlights_honor_preset_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(true);
        let computed = highlights_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn references_honor_cancellation_during_occurrence_collection() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = crate::navigation::test_cancel_after_checks(6);
        let computed = references_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn initial_context_observations_honor_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = crate::project::test_cancel_project_scan_after_checks(0);
        let computed = references_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert_eq!(computed.value, Err(super::CANCELLATION_MESSAGE.to_string()));
    }

    #[test]
    fn highlights_honor_cancellation_during_occurrence_collection() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = crate::navigation::test_cancel_in_phase(
            crate::navigation::TestCancellationPhase::OccurrenceCollection,
        );
        let computed = highlights_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }
}
