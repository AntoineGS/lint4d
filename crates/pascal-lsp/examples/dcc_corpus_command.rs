use pascal_core::{
    FileInfo, FilesystemSourceStore, ImportSection, ImportSite, NoCancellation, ResolvedUnit,
    ResolverLimits, SourceKind, UnitResolver, parser,
};
use pascal_project::{
    CompilerVersion, InstallationSelection, ProjectContext, ProjectOptions, config_directories,
    delphi_overrides::{OverrideSession, user_config_path_from_env},
    installations::compiler_version_for_installation,
    read_config,
};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage:\n  dcc_corpus_command <dproj> [--config C] [--platform P]\n  dcc_corpus_command --list [manifest]\n\nPrints a JSON command description, or name<TAB>dproj<TAB>config<TAB>platform rows in --list mode.\n";
const LSP_CONFIG_MAX_BYTES: usize = 1024 * 1024;
const CORPUS_MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;
const MAX_CORPUS_PROJECT_UNITS: usize = 256;
const LSP_SOURCE_PATHS: &[&str] = &[
    "C:/DelphiSources/rtl/sys",
    "C:/DelphiSources/rtl/common",
    "C:/DelphiSources/rtl/win",
    "C:/DelphiSources/vcl",
    "C:/DelphiSources/db",
    "C:/DelphiSources/db/ado",
    "C:/DelphiSources/db/dsnap",
    "C:/DelphiSources/db/dbx",
    "C:/DelphiSources/db/vclctrls",
    "C:/DelphiSources/IBX",
    "C:/DelphiSources/internet",
    "C:/DelphiSources/soap",
    "C:/DelphiSources/xml",
];

#[derive(Default)]
struct Arguments {
    dproj: Option<PathBuf>,
    config: Option<String>,
    platform: Option<String>,
    list_manifest: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("dcc_corpus_command: {error}\n\n{USAGE}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let arguments = parse_args(&env::args().skip(1).collect::<Vec<_>>())?;
    if let Some(manifest) = arguments.list_manifest {
        return list_manifest(&manifest);
    }

    let dproj = arguments
        .dproj
        .ok_or_else(|| "missing project file".to_string())?;
    let output = project_command(
        &dproj,
        arguments.config.as_deref(),
        arguments.platform.as_deref(),
    );
    println!(
        "{}",
        serde_json::to_string(&output).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn parse_args(args: &[String]) -> Result<Arguments, String> {
    if args == ["--help"] || args == ["-h"] {
        print!("{USAGE}");
        std::process::exit(0);
    }

    let mut parsed = Arguments::default();
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        index += 1;
        match option {
            "--list" => {
                if parsed.list_manifest.is_some() || parsed.dproj.is_some() {
                    return Err("--list cannot be combined with a project file".to_string());
                }
                parsed.list_manifest = Some(match args.get(index) {
                    Some(path) if !path.starts_with("--") => {
                        index += 1;
                        PathBuf::from(path)
                    }
                    _ => default_manifest_path()?,
                });
                if index < args.len() {
                    return Err(format!("unexpected argument {:?}", args[index]));
                }
            }
            "--config" => {
                parsed.config = Some(take_value(args, &mut index, option)?);
            }
            "--platform" => {
                parsed.platform = Some(take_value(args, &mut index, option)?);
            }
            value if value.starts_with("--") => {
                return Err(format!("unknown option {value:?}"));
            }
            value if parsed.dproj.is_none() => {
                parsed.dproj = Some(PathBuf::from(value));
            }
            value => return Err(format!("unexpected argument {value:?}")),
        }
    }
    Ok(parsed)
}

fn take_value(args: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    let value = args
        .get(*index)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or_else(|| format!("{option} requires a value"))?;
    *index += 1;
    Ok(value)
}

fn default_manifest_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("LINT4D_CORPUS_MANIFEST") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("lint4d")
        .join("corpus.toml"))
}

fn list_manifest(path: &Path) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read manifest {}: {error}", path.display()))?;
    let manifest = pascal_lsp::corpus::parse_manifest(&text)?;
    for project in manifest.projects {
        let fields = [
            project.name,
            project.dproj.to_string_lossy().into_owned(),
            project.config.unwrap_or_default(),
            project.platform.unwrap_or_default(),
        ];
        if fields
            .iter()
            .any(|field| field.contains(['\t', '\r', '\n']))
        {
            return Err("manifest list values may not contain tabs or newlines".to_string());
        }
        println!("{}", fields.join("\t"));
    }
    Ok(())
}

fn project_command(dproj: &Path, config: Option<&str>, platform: Option<&str>) -> Value {
    match build_project_command(dproj, config, platform) {
        Ok(command) => command,
        Err(reason) => json!({ "skip": reason }),
    }
}

fn build_project_command(
    dproj: &Path,
    config: Option<&str>,
    platform: Option<&str>,
) -> Result<Value, String> {
    let dproj = fs::canonicalize(dproj)
        .map_err(|error| format!("could not resolve project {}: {error}", dproj.display()))?;
    if !dproj
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"))
    {
        return Err("project file must be a .dproj".to_string());
    }
    let project_root = dproj
        .parent()
        .ok_or_else(|| "project file has no parent directory".to_string())?
        .to_path_buf();
    let workspace_root = nearest_lsp_workspace_root(&project_root);
    let source_anchor = project_main_source(&dproj, &project_root, &workspace_root)?;
    let options = ProjectOptions {
        project_file: Some(dproj.clone()),
        build_config: config.map(str::to_owned),
        platform: platform.map(str::to_owned),
        source_paths: LSP_SOURCE_PATHS
            .iter()
            .map(|path| (*path).to_owned())
            .collect(),
        ..ProjectOptions::default()
    };
    let discovered = ProjectContext::discover(
        &source_anchor,
        std::slice::from_ref(&workspace_root),
        &options,
    )
    .map_err(|error| {
        format!(
            "project discovery failed for {} (workspace {}): {error}",
            source_anchor.display(),
            workspace_root.display()
        )
    })?;
    let context = discovered.context;
    if context.project_file().is_none() {
        return Err("project discovery did not select a project file".to_string());
    }

    let mut warnings = discovered.observations.warnings.clone();
    warnings.extend(lsp_configuration_warnings(
        &source_anchor,
        &project_root,
        &workspace_root,
    ));

    let main_source = context
        .main_source()
        .ok_or_else(|| "project context did not resolve a main source".to_string())?
        .to_path_buf();
    let main_source = fs::canonicalize(&main_source).map_err(|error| {
        format!(
            "could not resolve main source {}: {error}",
            main_source.display()
        )
    })?;
    let project_root = fs::canonicalize(&project_root)
        .map_err(|error| format!("could not resolve project root: {error}"))?;
    if !main_source.starts_with(&project_root) {
        return Err(format!(
            "main source {} is outside project root {}",
            main_source.display(),
            project_root.display()
        ));
    }

    let compiler_version = context
        .effective_conditional_context()
        .compiler_version
        .or_else(|| selected_compiler_version(&context));
    let compiler = compiler_version
        .map(format_compiler_version)
        .ok_or_else(|| "could not determine the selected compiler version".to_string())?;
    let selected_platform = context
        .platform()
        .map(str::to_owned)
        .or_else(|| platform.map(str::to_owned))
        .ok_or_else(|| "could not determine the selected compiler platform".to_string())?;
    let dcc = compiler_executable(&context, &dproj, &compiler, &selected_platform)?;

    let walk = reachable_project_files(
        &context,
        &main_source,
        &project_root,
        &workspace_root,
        &discovered.observations,
        &mut warnings,
    );
    let dcc_unit_paths = compiler_unit_paths(&context, &workspace_root, &options.source_paths);
    let dcc_include_paths = compiler_include_paths(&context, &dcc_unit_paths);
    // Include lookup uses explicit DCC include paths plus project unit
    // search paths (units may include .inc files from their search dirs).
    // pascal-project does not expose DCC_ResourcePath or DCC_ObjPath.
    Ok(json!({
        "compiler": compiler,
        "platform": selected_platform,
        "dcc": dcc.display().to_string(),
        "main_source": main_source.display().to_string(),
        "unit_paths": dcc_unit_paths
            .iter().map(|path| path_string(path)).collect::<Vec<_>>(),
        "include_paths": dcc_include_paths
            .iter().map(|path| path_string(path)).collect::<Vec<_>>(),
        "resource_paths": Vec::<String>::new(),
        "object_paths": Vec::<String>::new(),
        "namespaces": context.unit_namespaces(),
        "defines": context.defines(),
        "files": walk.files.iter().map(|path| path_string(path)).collect::<Vec<_>>(),
        "incomplete": !walk.complete,
        "warnings": warnings,
    }))
}

fn nearest_lsp_workspace_root(project_root: &Path) -> PathBuf {
    project_root
        .ancestors()
        .find(|directory| {
            directory.join(".lint4d.toml").is_file() || directory.join(".git").exists()
        })
        .unwrap_or(project_root)
        .to_path_buf()
}

fn project_main_source(
    dproj: &Path,
    project_root: &Path,
    workspace_root: &Path,
) -> Result<PathBuf, String> {
    let text = fs::read_to_string(dproj)
        .map_err(|error| format!("could not read project {}: {error}", dproj.display()))?;
    let main_source = xml_tag_value(&text, "MainSource")
        .filter(|value| !value.is_empty() && !value.contains("$("))
        .or_else(|| {
            let stem = dproj.file_stem()?;
            ["dpr", "dpk", "pas"]
                .into_iter()
                .map(|extension| project_root.join(stem).with_extension(extension))
                .find(|path| path.is_file())
                .map(|path| path.to_string_lossy().into_owned())
        })
        .ok_or_else(|| "project file does not identify an existing main source".to_string())?;

    let user_config = user_config_path_from_env().ok();
    let overrides = OverrideSession::new(user_config);
    let effective = overrides
        .effective_for(Some(workspace_root), Some(project_root))
        .map_err(|error| format!("could not resolve Delphi path mappings: {error}"))?;
    let resolved = effective
        .resolve_path(&main_source, project_root)
        .map_err(|error| format!("could not resolve main source path: {error}"))?;
    fs::canonicalize(&resolved.path).map_err(|error| {
        format!(
            "main source {} does not exist: {error}",
            resolved.path.display()
        )
    })
}

fn xml_tag_value(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let value = text[start..end].trim();
    if value.is_empty() {
        return None;
    }
    Some(
        value
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'"),
    )
}

fn lsp_configuration_warnings(
    dproj: &Path,
    project_root: &Path,
    workspace_root: &Path,
) -> Vec<String> {
    let roots = vec![workspace_root.to_path_buf()];
    let directories = match config_directories(dproj, Some(project_root), &roots) {
        Ok(directories) => directories,
        Err(error) => return vec![format!("could not resolve LSP config directories: {error}")],
    };
    let mut warnings = Vec::new();
    for filename in [".lint4d.toml", ".fmt4d.toml"] {
        if let Err(error) = read_config(&directories, filename, LSP_CONFIG_MAX_BYTES) {
            warnings.push(error);
        }
    }
    warnings
}

fn selected_compiler_version(context: &ProjectContext) -> Option<CompilerVersion> {
    match context.installation_selection()? {
        InstallationSelection::Selected { id, .. } => compiler_version_for_installation(id),
        InstallationSelection::Legacy => context.installation_evidence().compiler_version,
        InstallationSelection::NeedsChoice { .. } | InstallationSelection::Invalid { .. } => None,
    }
}

fn format_compiler_version(version: CompilerVersion) -> String {
    format!("{}.{}", version.major, version.minor)
}

fn compiler_executable(
    context: &ProjectContext,
    dproj: &Path,
    compiler: &str,
    platform: &str,
) -> Result<PathBuf, String> {
    let exe_stem = if platform.to_ascii_lowercase().contains("64") {
        "dcc64"
    } else {
        "dcc32"
    };
    let install_root = installation_root(context, dproj, compiler)?;
    for name in [format!("{exe_stem}.exe"), format!("{exe_stem}.EXE")] {
        let candidate = install_root.join("bin").join(name);
        if candidate.is_file() {
            return fs::canonicalize(&candidate).map_err(|error| {
                format!("could not resolve dcc {}: {error}", candidate.display())
            });
        }
    }
    Err(format!(
        "no {exe_stem} executable found under {}",
        install_root.display()
    ))
}

fn installation_root(
    context: &ProjectContext,
    dproj: &Path,
    compiler: &str,
) -> Result<PathBuf, String> {
    let project_directory = dproj.parent().unwrap_or_else(|| Path::new("."));
    let configured_bds = context
        .overrides()
        .properties
        .get("bds")
        .or(context.installation_evidence().bds_root.as_ref());
    if let Some(bds) = configured_bds {
        let resolved = context
            .overrides()
            .resolve_path(bds, project_directory)
            .map_err(|error| format!("could not resolve BDS profile path: {error}"))?;
        if resolved.path.is_dir() {
            return Ok(resolved.path);
        }
    }

    let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
    let base = PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("Headless-Delphi")
        .join("files");
    let fallback = match compiler {
        "21.0" => base.join("RAD Studio/7.0"),
        "24.0" => base.join("RAD Studio/10.0"),
        "36.0" => base.join("Studio/23.0"),
        "37.0" => base.join("Studio/37.0"),
        _ => {
            return Err(format!(
                "no standard Headless-Delphi installation mapping for compiler {compiler}"
            ));
        }
    };
    if fallback.is_dir() {
        Ok(fallback)
    } else {
        Err(format!(
            "selected installation root was not found for compiler {compiler}"
        ))
    }
}

struct ProjectWalk {
    files: Vec<PathBuf>,
    complete: bool,
}

fn reachable_project_files(
    context: &ProjectContext,
    main_source: &Path,
    project_root: &Path,
    workspace_root: &Path,
    observations: &pascal_project::ProjectObservations,
    warnings: &mut Vec<String>,
) -> ProjectWalk {
    let mut files = HashSet::new();
    files.insert(main_source.to_path_buf());
    let mut complete = context.discovery_complete()
        && context.binding_metadata_complete()
        && !context.unit_directories_unknown()
        && context.path_issues().is_empty();
    let bytes = match fs::read(main_source) {
        Ok(bytes) => bytes,
        Err(error) => {
            warnings.push(format!(
                "could not read main source {} for dependency walk: {error}",
                main_source.display()
            ));
            complete = false;
            return ProjectWalk {
                files: sorted_files(files),
                complete,
            };
        }
    };
    let (root_sites, declared_name) = match parse_source_imports(main_source, &bytes) {
        Ok((sites, diagnostic_count, declared_name)) => {
            if diagnostic_count > 0 {
                warnings.push(format!(
                    "main source has {} parser diagnostic(s); dependency walk may be incomplete",
                    diagnostic_count
                ));
                complete = false;
            }
            (sites, declared_name)
        }
        Err(error) => {
            warnings.push(format!("could not parse main source imports: {error}"));
            return ProjectWalk {
                files: sorted_files(files),
                complete: false,
            };
        }
    };
    // Keep the walk's shared conditional-analysis/cache budget finite while
    // allowing larger source units than the interactive 1 MiB request cap.
    let limits = ResolverLimits {
        max_dependency_units: MAX_CORPUS_PROJECT_UNITS,
        max_compile_time_retained_bytes: CORPUS_MAX_RETAINED_BYTES,
        ..ResolverLimits::default()
    };
    let mut resolver = UnitResolver::new(
        context.clone(),
        vec![workspace_root.to_path_buf(), project_root.to_path_buf()],
        FilesystemSourceStore::new(),
        limits,
    )
    .with_context_observations(observations);
    let requested_name = main_source
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Project");
    let root_source =
        match resolver.load_source(main_source, None, SourceKind::Unit, &NoCancellation) {
            Ok(source) => source,
            Err(error) => {
                warnings.push(format!(
                    "could not load main source for dependency walk: {error}"
                ));
                return ProjectWalk {
                    files: sorted_files(files),
                    complete: false,
                };
            }
        };
    let root_id = root_source.id.clone();
    let root = ResolvedUnit {
        requested_name: requested_name.to_owned(),
        declared_name,
        source: root_source,
    };
    let mut seen = HashSet::from([root_id]);
    let mut queue = VecDeque::from([(root, Some(root_sites))]);
    'walk: while let Some((unit, known_sites)) = queue.pop_front() {
        let sites = if let Some(sites) = known_sites {
            sites
        } else {
            match parse_source_imports(&unit.source.path, &unit.source.bytes) {
                Ok((sites, diagnostic_count, _)) => {
                    if diagnostic_count > 0 {
                        warnings.push(format!(
                            "dependency source has {} parser diagnostic(s)",
                            diagnostic_count
                        ));
                        complete = false;
                    }
                    sites
                }
                Err(error) => {
                    warnings.push(format!(
                        "could not parse dependency source imports: {error}"
                    ));
                    complete = false;
                    continue;
                }
            }
        };
        let resolved = match resolver.resolve_imports(&unit, &sites, &NoCancellation) {
            Ok(resolved) => resolved,
            Err(error) => {
                warnings.push(format!("could not resolve dependency imports: {error}"));
                complete = false;
                continue;
            }
        };
        complete &= resolved.complete;
        for dependency in resolved.dependencies {
            if !dependency.source.path.starts_with(project_root) {
                continue;
            }
            let path = match fs::canonicalize(&dependency.source.path) {
                Ok(path) if path.starts_with(project_root) => path,
                Ok(_) => {
                    warnings.push("resolved project unit escaped the project root".to_string());
                    complete = false;
                    continue;
                }
                Err(_) => {
                    warnings.push("could not canonicalize a resolved project unit".to_string());
                    complete = false;
                    continue;
                }
            };
            if seen.contains(&dependency.source.id) {
                continue;
            }
            if seen.len() >= MAX_CORPUS_PROJECT_UNITS {
                warnings.push(format!(
                    "project dependency walk stopped at the {MAX_CORPUS_PROJECT_UNITS}-unit budget"
                ));
                complete = false;
                break 'walk;
            }
            files.insert(path);
            seen.insert(dependency.source.id.clone());
            queue.push_back((dependency, None));
        }
    }
    let report = resolver.report();
    warnings.extend(report.warnings);
    if !report.complete {
        complete = false;
    }
    ProjectWalk {
        files: sorted_files(files),
        complete,
    }
}

fn parse_source_imports(
    path: &Path,
    source: &[u8],
) -> Result<(Vec<ImportSite>, usize, String), String> {
    let (tree, diagnostics) = parser::parse_file(&FileInfo::new(path.to_path_buf()), source)
        .map_err(|error| format!("could not parse source {}: {error}", path.display()))?;
    let declared_name = declared_module_name(tree.root_node(), source)
        .or_else(|| {
            path.file_stem()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or_else(|| format!("source {} has no module name", path.display()))?;
    Ok((
        import_sites(tree.root_node(), source),
        diagnostics.len(),
        declared_name,
    ))
}

fn declared_module_name(root: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut modules = Vec::new();
    collect_module_names(root, &mut modules);
    modules
        .into_iter()
        .find(|node| !has_ancestor_kind(*node, "declUses"))
        .and_then(|node| node.utf8_text(source).ok())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn compiler_unit_paths(
    context: &ProjectContext,
    workspace_root: &Path,
    source_paths: &[String],
) -> Vec<PathBuf> {
    let navigation_paths = source_paths
        .iter()
        .filter_map(|raw| context.overrides().resolve_path(raw, workspace_root).ok())
        .map(|resolved| canonical_or_lexical(&resolved.path))
        .collect::<Vec<_>>();
    let navigation_entries = context
        .search_path_entries()
        .iter()
        .filter(|entry| {
            let entry_path = canonical_or_lexical(&entry.path);
            navigation_paths.contains(&entry_path)
        })
        .cloned()
        .collect::<Vec<_>>();

    // The project context deliberately retains navigation and IDE paths for
    // source resolution. DCC consumes project unit paths followed by the IDE
    // library path, not the caller's LSP sourcePaths.
    context
        .compiler_unit_path_entries(&navigation_entries)
        .iter()
        .map(|entry| canonical_or_lexical(&entry.path))
        .collect()
}

fn compiler_include_paths(
    context: &ProjectContext,
    compiler_unit_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in context
        .include_paths()
        .into_iter()
        .chain(compiler_unit_paths.iter().cloned())
    {
        let path = canonical_or_lexical(&path);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn import_sites(root: tree_sitter::Node<'_>, source: &[u8]) -> Vec<ImportSite> {
    let mut modules = Vec::new();
    collect_module_names(root, &mut modules);
    modules
        .into_iter()
        .filter(|node| has_ancestor_kind(*node, "declUses"))
        .filter_map(|node| {
            let requested_name = node.utf8_text(source).ok()?.trim().to_owned();
            if requested_name.is_empty() {
                return None;
            }
            let section = if has_ancestor_kind(node, "interface") {
                ImportSection::Interface
            } else if has_ancestor_kind(node, "implementation") {
                ImportSection::Implementation
            } else {
                ImportSection::Module
            };
            Some(ImportSite {
                byte_range: node.start_byte()..node.end_byte(),
                requested_name,
                section,
            })
        })
        .collect()
}

fn collect_module_names<'tree>(
    node: tree_sitter::Node<'tree>,
    output: &mut Vec<tree_sitter::Node<'tree>>,
) {
    if node.kind() == "moduleName" {
        output.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_module_names(child, output);
    }
}

fn has_ancestor_kind(mut node: tree_sitter::Node<'_>, kind: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return true;
        }
        node = parent;
    }
    false
}

fn sorted_files(files: HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut files = files.into_iter().collect::<Vec<_>>();
    files.sort();
    files
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn project_walk_follows_explicit_and_transitive_uses_but_excludes_external_units() {
        let temp = tempdir().expect("temporary project");
        let root = temp.path().join("project");
        let unit_dir = root.join("units");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let library_dir = root.join("ide-library");
        fs::create_dir_all(&library_dir).unwrap();
        let dproj = root.join("Main.dproj");
        let main = root.join("Main.dpr");
        let unit_a = root.join("A.pas");
        let unit_b = unit_dir.join("B.pas");
        let external_sysutils = external_dir.join("SysUtils.pas");
        let shared_include = unit_dir.join("Shared.inc");
        fs::write(
            &dproj,
            "<Project><PropertyGroup><MainSource>Main.dpr</MainSource><DCC_UnitSearchPath>units</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .unwrap();
        fs::write(&main, "program Main; uses A in 'A.pas'; begin end.").unwrap();
        fs::write(&unit_a, "unit A; interface uses B; implementation end.").unwrap();
        fs::write(
            &unit_b,
            "unit B; interface uses SysUtils; implementation {$I Shared.inc} end.",
        )
        .unwrap();
        fs::write(&shared_include, "// shared compiler include\n").unwrap();
        fs::write(
            &external_sysutils,
            "unit SysUtils; interface implementation end.",
        )
        .unwrap();

        let options = ProjectOptions {
            project_file: Some(dproj),
            source_paths: vec![external_dir.to_string_lossy().into_owned()],
            ..ProjectOptions::default()
        };
        let discovered = ProjectContext::discover(&main, &[root.to_path_buf()], &options).unwrap();
        let mut context_parts = (*discovered.context).clone().into_parts();
        context_parts
            .search_path_entries
            .push(pascal_project::ProjectPathEntry::configured(
                library_dir.clone(),
            ));
        let context = ProjectContext::from_parts(context_parts);
        let mut warnings = Vec::new();

        let files = reachable_project_files(
            &context,
            &main,
            &root,
            &root,
            &discovered.observations,
            &mut warnings,
        );

        assert!(files.complete);
        assert_eq!(files.files, vec![unit_a, main, unit_b]);
        let dcc_paths = compiler_unit_paths(&context, &root, &options.source_paths);
        assert_eq!(dcc_paths, vec![root, unit_dir, library_dir]);
        let dcc_include_paths = compiler_include_paths(&context, &dcc_paths);
        assert!(shared_include.is_file());
        assert!(dcc_include_paths.contains(&shared_include.parent().unwrap().to_path_buf()));
        assert!(!dcc_include_paths.contains(&external_dir));
    }

    #[test]
    fn project_walk_marks_unresolved_import_incomplete() {
        let temp = tempdir().expect("temporary project");
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        let dproj = root.join("Main.dproj");
        let main = root.join("Main.dpr");
        fs::write(
            &dproj,
            "<Project><PropertyGroup><MainSource>Main.dpr</MainSource></PropertyGroup></Project>",
        )
        .unwrap();
        fs::write(&main, "program Main; uses MissingUnit; begin end.").unwrap();

        let options = ProjectOptions {
            project_file: Some(dproj),
            ..ProjectOptions::default()
        };
        let discovered = ProjectContext::discover(&main, &[root.clone()], &options).unwrap();
        let mut warnings = Vec::new();

        let walk = reachable_project_files(
            &discovered.context,
            &main,
            &root,
            &root,
            &discovered.observations,
            &mut warnings,
        );

        assert!(!walk.complete);
        assert_eq!(walk.files, vec![main]);
    }

    #[test]
    fn project_walk_stops_at_the_total_unit_budget() {
        let temp = tempdir().expect("temporary project");
        let root = temp.path().join("project");
        let units = root.join("units");
        fs::create_dir_all(&units).unwrap();
        let dproj = root.join("Main.dproj");
        let main = root.join("Main.dpr");
        fs::write(
            &dproj,
            "<Project><PropertyGroup><MainSource>Main.dpr</MainSource><DCC_UnitSearchPath>units</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .unwrap();
        fs::write(&main, "program Main; uses U000; begin end.").unwrap();
        let unit_count = 257;
        for index in 0..unit_count {
            let unit_name = format!("U{index:03}");
            let uses = if index + 1 < unit_count {
                format!(" uses U{:03};", index + 1)
            } else {
                String::new()
            };
            fs::write(
                units.join(format!("{unit_name}.pas")),
                format!("unit {unit_name}; interface{uses} implementation end."),
            )
            .unwrap();
        }
        let options = ProjectOptions {
            project_file: Some(dproj),
            ..ProjectOptions::default()
        };
        let discovered = ProjectContext::discover(&main, &[root.clone()], &options).unwrap();
        let mut warnings = Vec::new();

        let walk = reachable_project_files(
            &discovered.context,
            &main,
            &root,
            &root,
            &discovered.observations,
            &mut warnings,
        );

        assert!(!walk.complete);
        assert!(walk.files.len() < unit_count + 1);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unit budget"))
        );
    }
}
