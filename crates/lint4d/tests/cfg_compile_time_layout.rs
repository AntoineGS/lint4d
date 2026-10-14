use lint4d::cfg::project_snapshot::{
    CfgSnapshotOptions, CfgSnapshotStatus, to_cfg_project_snapshot,
};
use pascal_core::{FilesystemSourceStore, NoCancellation, ResolverLimits, UnitResolver};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, LayoutPlatform,
    LayoutSettings, ProjectContext, ProjectOptions,
};

fn edit_context<R>(
    context: &mut pascal_project::ProjectContext,
    edit: impl FnOnce(&mut pascal_project::ProjectContextParts) -> R,
) {
    let mut parts = std::mem::take(context).into_parts();
    edit(&mut parts);
    *context = pascal_project::ProjectContext::from_parts(parts);
}

fn context() -> ConditionalContext {
    ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(37, 0))
        .with_layout(LayoutContext {
            platform: Some(LayoutPlatform::Win32),
            defaults: LayoutSettings {
                long_strings: ConditionalFact::True,
                real_compatibility: ConditionalFact::False,
                ..LayoutSettings::default()
            },
        })
}

fn resolve(dir: &std::path::Path, text: &str) -> pascal_core::ResolvedProject {
    let path = dir.join("App.pas");
    std::fs::write(&path, text).unwrap();
    let project_options = ProjectOptions {
        source_paths: vec![dir.to_string_lossy().into_owned()],
        build_config: Some("Debug".into()),
        platform: Some("Win32".into()),
        conditional_context: context(),
        ..ProjectOptions::default()
    };
    let mut ctx = ProjectContext::discover(&path, &[dir.into()], &project_options).unwrap();
    edit_context(&mut ctx.context, |p| p.conditional_context = context());
    let sites = text
        .find("uses Types")
        .map(|start| pascal_core::ImportSite {
            byte_range: start + 5..start + 10,
            requested_name: "Types".into(),
            section: pascal_core::ImportSection::Module,
        })
        .into_iter()
        .collect::<Vec<_>>();
    UnitResolver::new(
        ctx.context,
        vec![dir.into()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    )
    .resolve_project_from_source(&path, "App", &sites, None, &NoCancellation)
    .unwrap()
}

fn options() -> CfgSnapshotOptions {
    CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("explicit-win32".into()),
        conditional_context: context(),
        ..CfgSnapshotOptions::default()
    }
}

const INITIALIZER: &str = "program App;\nconst Revision = {$I revision.inc};\n{$IF (Revision = 7) AND (SizeOf(LongWord) = 4)}\nprocedure Chosen; begin end;\n{$ELSE}\nprocedure Forbidden; begin end;\n{$ENDIF}\nbegin Chosen; end.\n";

#[test]
fn initializer_include_and_sizeof_choose_first_pass_and_physical_origins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("revision.inc"), "7").unwrap();
    let project = resolve(dir.path(), INITIALIZER);
    let root_id = project.root.source.id.as_str().to_string();
    let include_id = project.include_sources[0].id.as_str().to_string();
    let snapshot = to_cfg_project_snapshot(project, options()).unwrap();
    assert_eq!(snapshot.status, CfgSnapshotStatus::Complete);
    let input = &snapshot.snapshot.units()[0];
    assert!(input.is_prepared());
    let text = std::str::from_utf8(input.source()).unwrap();
    assert!(text.contains("procedure Chosen"));
    assert!(!text.contains("procedure Forbidden"));
    for (token, id, original) in [
        ("Chosen", root_id, INITIALIZER.find("Chosen").unwrap()),
        ("7", include_id, 0),
    ] {
        let start = text.find(token).unwrap();
        let mapped = input
            .source_map()
            .unwrap()
            .map_range(start..start + token.len())
            .unwrap();
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].original().unwrap().source_id().as_str(), id);
        assert_eq!(
            mapped[0].original().unwrap().byte_range(),
            original..original + token.len()
        );
    }
}

#[test]
fn missing_initializer_retains_raw_incomplete_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = to_cfg_project_snapshot(resolve(dir.path(), INITIALIZER), options()).unwrap();
    assert!(matches!(
        snapshot.status,
        CfgSnapshotStatus::Incomplete { .. }
    ));
    assert!(
        snapshot
            .snapshot
            .units()
            .iter()
            .all(|input| !input.is_prepared())
    );
}

fn assert_incomplete(project: pascal_core::ResolvedProject) {
    let snapshot = to_cfg_project_snapshot(project, options()).unwrap();
    assert!(
        matches!(snapshot.status, CfgSnapshotStatus::Incomplete { .. }),
        "{:?}",
        snapshot.status
    );
    assert!(
        snapshot
            .snapshot
            .units()
            .iter()
            .all(|input| !input.is_prepared())
    );
}

#[test]
fn denied_initializer_cannot_reuse_supplied_but_unauthorized_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("revision.inc"), "7").unwrap();
    let mut project = resolve(dir.path(), INITIALIZER);
    // Even with a complete report, a denied binding is not source authority.
    project.includes[0].target = pascal_core::ResolutionTarget::Unavailable;
    assert_incomplete(project);
}

#[test]
fn unknown_layout_never_guesses_host_or_options_layout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("revision.inc"), "7").unwrap();
    let mut project = resolve(dir.path(), INITIALIZER);
    project.conditional_contexts.insert(
        project.root.source.id.clone(),
        ConditionalContext::default(),
    );
    assert_incomplete(project);
}

#[test]
fn cycle_is_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("revision.inc"), "{$I revision.inc}").unwrap();
    assert_incomplete(resolve(dir.path(), INITIALIZER));
}

#[test]
fn repeated_include_in_different_entry_scopes_is_an_explicit_limitation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("body.inc"), "const M = N;").unwrap();
    let project = resolve(
        dir.path(),
        "program App; const N=1; {$I body.inc} procedure P; const N=2; {$I body.inc} begin end; begin end.",
    );
    let snapshot = to_cfg_project_snapshot(project, options()).unwrap();
    let CfgSnapshotStatus::Incomplete { reason } = snapshot.status else {
        panic!("reused lexical entry")
    };
    assert!(
        reason.contains("multiple entry contexts/occurrences"),
        "{reason}"
    );
    assert!(!snapshot.snapshot.units()[0].is_prepared());
}

#[test]
fn snapshot_replays_exact_authorized_revision_without_live_reads() {
    let dir = tempfile::tempdir().unwrap();
    let include = dir.path().join("revision.inc");
    std::fs::write(&include, "7").unwrap();
    let project = resolve(dir.path(), INITIALIZER);
    let observations = project.report.observations.clone();
    std::fs::write(&include, "9").unwrap();
    std::fs::remove_file(dir.path().join("App.pas")).unwrap();
    let snapshot = to_cfg_project_snapshot(project, options()).unwrap();
    assert_eq!(snapshot.status, CfgSnapshotStatus::Complete);
    assert_eq!(snapshot.resolution.observations, observations);
    let text = std::str::from_utf8(snapshot.snapshot.units()[0].source()).unwrap();
    assert!(text.contains('7'));
    assert!(!text.contains('9'));
    assert!(text.contains("procedure Chosen"));
    assert!(!text.contains("procedure Forbidden"));
}

#[test]
fn root_legacy_options_context_is_not_a_provider_context() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Types.pas"),
        "unit Types; interface type T = Pointer; implementation end.",
    )
    .unwrap();
    let mut project = resolve(
        dir.path(),
        "program App; uses Types; {$IF SizeOf(Types.T)=4} procedure Chosen; begin end; {$ELSE} procedure Forbidden; begin end; {$ENDIF} begin end.",
    );
    assert_eq!(project.units.len(), 1, "{project:#?}");
    let snapshot = to_cfg_project_snapshot(project.clone(), options()).unwrap();
    assert_eq!(snapshot.status, CfgSnapshotStatus::Complete);
    project.conditional_contexts.clear();
    assert_incomplete(project);
}

#[test]
fn provider_uses_its_own_target_and_authorized_qualified_binding() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Types.pas"),
        "unit Types; interface type T = Pointer; implementation end.",
    )
    .unwrap();
    let mut project = resolve(
        dir.path(),
        "program App; uses Types; type T=Byte; {$IF SizeOf(Types.T)=8} procedure Chosen; begin end; {$ELSE} procedure Forbidden; begin end; {$ENDIF} begin end.",
    );
    assert_eq!(project.units.len(), 1, "{project:#?}");
    let provider_id = project.units[0].source.id.clone();
    let mut provider_context = context();
    provider_context.layout.platform = Some(LayoutPlatform::Win64);
    project
        .conditional_contexts
        .insert(provider_id, provider_context);
    let snapshot = to_cfg_project_snapshot(project, options()).unwrap();
    assert_eq!(snapshot.status, CfgSnapshotStatus::Complete);
    let text = std::str::from_utf8(snapshot.snapshot.units()[0].source()).unwrap();
    assert!(text.contains("procedure Chosen"));
    assert!(!text.contains("procedure Forbidden"));
}

#[test]
fn unknown_condition_cannot_publish_complete() {
    let dir = tempfile::tempdir().unwrap();
    assert_incomplete(resolve(
        dir.path(),
        "program App; {$IF SizeOf(Missing)=4} procedure Chosen; begin end; {$ENDIF} begin end.",
    ));
}

#[test]
fn qualified_unit_name_shadow_is_not_a_provider_layout_guess() {
    let dir = tempfile::tempdir_in("/tmp/opencode").unwrap();
    std::fs::write(
        dir.path().join("Types.pas"),
        "unit Types; interface type T=Pointer; implementation end.",
    )
    .unwrap();
    assert_incomplete(resolve(
        dir.path(),
        "program App; uses Types; type Types=Byte; {$IF SizeOf(Types.T)=4} procedure Forbidden; begin end; {$ENDIF} begin end.",
    ));
}

#[test]
fn resolver_denied_initializer_include_retains_raw_fallback() {
    let dir = tempfile::tempdir_in("/tmp/opencode").unwrap();
    let outside = tempfile::tempdir_in("/tmp/opencode").unwrap();
    let denied = outside.path().join("revision.inc");
    std::fs::write(&denied, "7").unwrap();
    let text = INITIALIZER.replace("revision.inc", denied.to_str().unwrap());
    let project = resolve(dir.path(), &text);
    assert!(
        !project.complete,
        "include outside authorized workspace must be denied"
    );
    assert!(project.include_sources.is_empty());
    assert_incomplete(project);
}
