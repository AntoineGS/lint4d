use pascal_core::{
    FileInfo, FilesystemSourceStore, ImportSection, ImportSite, NoCancellation, ResolverLimits,
    UnitResolver, parser,
};
use pascal_project::{
    CompilerVersion, InstallationSelection, ProjectContext, ProjectOptions, config_directories,
    delphi_overrides::{OverrideSession, user_config_path_from_env},
    installations::compiler_version_for_installation,
    read_config,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage:\n  dcc_corpus_command <dproj> [--config C] [--platform P]\n  dcc_corpus_command --list [manifest]\n\nPrints a JSON command description, or name<TAB>dproj<TAB>config<TAB>platform rows in --list mode.\n";
const LSP_CONFIG_MAX_BYTES: usize = 1024 * 1024;
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

    let files = reachable_project_files(
        &context,
        &main_source,
        &project_root,
        &workspace_root,
        &discovered.observations,
        &mut warnings,
    );

    // pascal-project currently exposes unit and include paths, but not the
    // Delphi DCC_ResourcePath and DCC_ObjPath properties. Keep explicit empty
    // arrays rather than mislabeling library/source paths as compiler options.
    Ok(json!({
        "compiler": compiler,
        "platform": selected_platform,
        "dcc": dcc.display().to_string(),
        "main_source": main_source.display().to_string(),
        "unit_paths": context.search_paths().iter().map(|path| path_string(path)).collect::<Vec<_>>(),
        "include_paths": context.include_paths().iter().map(|path| path_string(path)).collect::<Vec<_>>(),
        "resource_paths": Vec::<String>::new(),
        "object_paths": Vec::<String>::new(),
        "namespaces": context.unit_namespaces(),
        "defines": context.defines(),
        "files": files.iter().map(|path| path_string(path)).collect::<Vec<_>>(),
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

fn reachable_project_files(
    context: &ProjectContext,
    main_source: &Path,
    project_root: &Path,
    workspace_root: &Path,
    observations: &pascal_project::ProjectObservations,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut files = HashSet::new();
    files.insert(main_source.to_path_buf());
    let bytes = match fs::read(main_source) {
        Ok(bytes) => bytes,
        Err(error) => {
            warnings.push(format!(
                "could not read main source {} for dependency walk: {error}",
                main_source.display()
            ));
            return sorted_files(files);
        }
    };
    let (tree, diagnostics) =
        match parser::parse_file(&FileInfo::new(main_source.to_path_buf()), &bytes) {
            Ok(parsed) => parsed,
            Err(error) => {
                warnings.push(format!("could not parse main source imports: {error}"));
                return sorted_files(files);
            }
        };
    if !diagnostics.is_empty() {
        warnings.push(format!(
            "main source has {} parser diagnostic(s); dependency walk may be incomplete",
            diagnostics.len()
        ));
    }
    let sites = import_sites(tree.root_node(), &bytes);

    let mut resolver = UnitResolver::new(
        context.clone(),
        vec![workspace_root.to_path_buf(), project_root.to_path_buf()],
        FilesystemSourceStore::new(),
        ResolverLimits::default(),
    )
    .with_context_observations(observations);
    let requested_name = main_source
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Project");
    match resolver.resolve_project_from_source(
        main_source,
        requested_name,
        &sites,
        None,
        &NoCancellation,
    ) {
        Ok(project) => {
            for unit in project.units {
                if unit.source.path.starts_with(project_root)
                    && let Ok(path) = fs::canonicalize(&unit.source.path)
                    && path.starts_with(project_root)
                {
                    files.insert(path);
                }
            }
            let report = resolver.report();
            warnings.extend(report.warnings);
            if !project.complete {
                warnings.push("project unit dependency walk was incomplete".to_string());
            }
        }
        Err(error) => warnings.push(format!("project unit dependency walk failed: {error}")),
    }
    sorted_files(files)
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
