use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use clap::{CommandFactory, Parser};
use lint4d::config::baseline::Baseline;
use lint4d::config::Config;
use lint4d::discovery::discover_files;
use lint4d::discovery::dproj::parse_dproj;
use lint4d::engine::{run_lint_with_context, FileInfo, Severity};
use lint4d::fix::fix_file;
use lint4d::output::json::format_json_output;
use lint4d::output::text::format_diagnostics;
use lint4d::rules::RuleRegistry;
use rayon::prelude::*;

/// Exit codes
const EXIT_OK: i32 = 0;
const EXIT_ISSUES: i32 = 1;
const EXIT_ERROR: i32 = 2;

const BASELINE_FILENAME: &str = ".lint4d-baseline.json";

#[derive(Parser)]
#[command(name = "lint4d", version, about = "An open-source Delphi linter")]
struct Cli {
    /// Files or directories to lint
    paths: Vec<PathBuf>,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    format: String,

    /// Minimum severity to cause a non-zero exit: error, warning, or hint
    #[arg(long, default_value = "warning")]
    fail_on: String,

    /// Create a default .lint4d.toml in the current directory
    #[arg(long)]
    init: bool,

    /// Generate a baseline file from current violations
    #[arg(long)]
    generate_baseline: bool,

    /// List all available lint rules
    #[arg(long)]
    list_rules: bool,

    /// Explain a specific rule by its ID
    #[arg(long)]
    explain: Option<String>,

    /// Lint files from a .dproj project file
    #[arg(long)]
    project: Option<PathBuf>,

    /// Explicit DCU directory path (repeatable)
    #[arg(long = "dcu-path")]
    dcu_paths: Vec<PathBuf>,

    /// Override auto-detected target platform (e.g., Win64)
    #[arg(long)]
    platform: Option<String>,

    /// Override build configuration (e.g., Debug, Release)
    #[arg(long = "build-config")]
    build_config: Option<String>,

    /// Explicit RAD Studio (BDS) installation root
    #[arg(long = "bds-path")]
    bds_path: Option<PathBuf>,

    /// Enable colored output
    #[arg(long)]
    color: bool,

    /// Auto-fix naming convention violations in-place
    #[arg(long)]
    fix_fmt: bool,
}

fn main() {
    // Spawn the real main on a thread with a larger stack to handle
    // deeply nested DCU parsing (e.g., large VCL units in debug builds).
    let builder = std::thread::Builder::new()
        .name("lint4d-main".to_string())
        .stack_size(16 * 1024 * 1024);
    let handler = builder
        .spawn(real_main)
        .expect("failed to spawn main thread");
    let result = handler.join();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn exit_err(msg: &str, code: i32) -> ! {
    eprintln!("lint4d: {}", msg);
    std::process::exit(code);
}

fn real_main() {
    let cli = Cli::parse();

    // --init: create default config and exit
    if cli.init {
        run_init();
        return;
    }

    // --list-rules: print all rules and exit
    if cli.list_rules {
        run_list_rules();
        return;
    }

    // --explain <rule-id>: print rule details and exit
    if let Some(ref rule_id) = cli.explain {
        run_explain(rule_id);
        return;
    }

    let threshold = validate_cli(&cli);

    // Discover config: start from the .dproj directory when --project is given,
    // so that a .lint4d.toml next to the project file is found automatically.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config_start = cli
        .project
        .as_ref()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| cwd.clone());
    let (config, _project_root) = match Config::discover(&config_start) {
        Ok(result) => result,
        Err(e) => exit_err(&format!("config error: {}", e), EXIT_ERROR),
    };

    let project_context = resolve_dcu_context(&cli, &config);
    let files = discover_source_files(&cli, &config);

    // --fix-fmt: run fix pipeline instead of lint pipeline
    if cli.fix_fmt {
        run_fix_fmt(&files, &config);
        return;
    }

    run_lint_pipeline(&cli, &config, &cwd, files, project_context, threshold);
}

/// Validates CLI arguments and returns the parsed severity threshold.
/// Exits with EXIT_ERROR if any argument is invalid.
fn validate_cli(cli: &Cli) -> Severity {
    // Require at least one path (or --project)
    if cli.paths.is_empty() && cli.project.is_none() {
        let mut cmd = Cli::command();
        let help = cmd.render_help();
        eprintln!("{}", help);
        process::exit(EXIT_ERROR);
    }

    // Parse the fail-on threshold
    let threshold = match cli.fail_on.parse::<Severity>() {
        Ok(s) => s,
        Err(e) => exit_err(&format!("invalid --fail-on value: {}", e), EXIT_ERROR),
    };

    // Validate format
    if cli.format != "text" && cli.format != "json" {
        exit_err(
            &format!(
                "invalid --format value: '{}' (expected 'text' or 'json')",
                cli.format
            ),
            EXIT_ERROR,
        );
    }

    // --fix-fmt: validate mutual exclusivity
    if cli.fix_fmt {
        let mut conflicts = Vec::new();
        if cli.format != "text" {
            conflicts.push("--format");
        }
        if cli.generate_baseline {
            conflicts.push("--generate-baseline");
        }
        if cli.fail_on != "warning" {
            conflicts.push("--fail-on");
        }
        if !conflicts.is_empty() {
            exit_err(
                &format!("--fix-fmt cannot be combined with {}", conflicts.join(", ")),
                EXIT_ERROR,
            );
        }
    }

    threshold
}

/// Resolves DCU paths and builds a ProjectContext if any DCU directories are found.
fn resolve_dcu_context(cli: &Cli, config: &Config) -> Option<lint4d::dcu::ProjectContext> {
    // MSBuild auto-discovery: only attempt if --project is provided
    let discovered_paths =
        if cli.dcu_paths.is_empty() && config.dcu_paths().is_empty() && cli.project.is_some() {
            discover_dcu_paths_from_project(
                cli.project.as_ref().unwrap(),
                cli.bds_path
                    .as_deref()
                    .or_else(|| config.bds_path().map(std::path::Path::new)),
                cli.platform.as_deref().or(config.platform()),
                cli.build_config.as_deref().or(config.build_config()),
            )
        } else {
            Vec::new()
        };

    // Warn if BDS-related flags are provided without --project
    if cli.project.is_none()
        && (cli.bds_path.is_some() || cli.platform.is_some() || cli.build_config.is_some())
    {
        eprintln!(
            "lint4d: warning: --bds-path/--platform/--build-config have no effect without --project"
        );
    }

    // Resolve DCU paths using priority cascade
    let dcu_dirs =
        lint4d::discovery::resolve_dcu_dirs(&cli.dcu_paths, config.dcu_paths(), discovered_paths);

    if !dcu_dirs.is_empty() {
        match lint4d::dcu::ProjectContext::from_dcu_paths(&dcu_dirs) {
            Ok(ctx) => Some(ctx),
            Err(e) => {
                eprintln!("lint4d: warning: failed to index DCU paths: {}", e);
                None
            }
        }
    } else {
        None
    }
}

/// Discovers source files to lint, either from a .dproj file or via glob discovery.
/// Exits with EXIT_ERROR on failure.
fn discover_source_files(cli: &Cli, config: &Config) -> Vec<FileInfo> {
    if let Some(ref project_path) = cli.project {
        match parse_dproj(project_path) {
            Ok(f) => f,
            Err(e) => exit_err(&format!("project file error: {}", e), EXIT_ERROR),
        }
    } else {
        match discover_files(&cli.paths, &config.exclude) {
            Ok(f) => f,
            Err(e) => exit_err(&format!("file discovery error: {}", e), EXIT_ERROR),
        }
    }
}

/// Runs the parallel lint pipeline: lints files, applies baseline, formats output, exits.
fn run_lint_pipeline(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    files: Vec<FileInfo>,
    project_context: Option<lint4d::dcu::ProjectContext>,
    threshold: Severity,
) {
    if files.is_empty() {
        // No Delphi files found -- not an error, just nothing to do
        if cli.format == "json" {
            println!("{}", format_json_output(&[]));
        }
        process::exit(EXIT_OK);
    }

    // Build the rule registry once and share across all files.
    let registry = RuleRegistry::new();

    if project_context.is_none() {
        for rule in registry.all_rules() {
            if rule.requires_context() {
                eprintln!(
                    "lint4d: info: rule '{}' skipped: no DCU paths configured \
                     (use --dcu-path or --project)",
                    rule.meta().id
                );
            }
        }
    }

    // Process files in parallel
    let mut file_results: Vec<_> = files
        .par_iter()
        .filter_map(|file| {
            let source = fs::read(&file.path).ok()?;
            let diagnostics =
                run_lint_with_context(file, &source, config, project_context.as_ref(), &registry);
            Some((file.path.to_string_lossy().to_string(), source, diagnostics))
        })
        .collect();

    // Sort by file path for deterministic output
    file_results.sort_by(|a, b| a.0.cmp(&b.0));

    // --generate-baseline: collect all diagnostics, write baseline, and exit
    if cli.generate_baseline {
        run_generate_baseline(cwd, &file_results);
        return;
    }

    // Load baseline if it exists
    let baseline = load_baseline(cwd);

    // Filter diagnostics against baseline
    if let Some(ref bl) = baseline {
        for (_file_path_str, source, diagnostics) in &mut file_results {
            let source_text = String::from_utf8_lossy(source);
            let source_lines: Vec<&str> = source_text.lines().collect();
            diagnostics.retain(|diag| {
                let line_content = source_lines.get(diag.line.saturating_sub(1)).unwrap_or(&"");
                let file_path = &_file_path_str;
                !bl.is_suppressed(file_path, diag, line_content)
            });
        }
    }

    // Check threshold and produce output
    let mut has_issues_above_threshold = false;
    let mut all_file_diagnostics: Vec<(String, Vec<lint4d::engine::Diagnostic>)> = Vec::new();

    for (file_path_str, source, diagnostics) in &file_results {
        for diag in diagnostics {
            if diag.severity >= threshold {
                has_issues_above_threshold = true;
            }
        }

        if cli.format == "text" {
            let output = format_diagnostics(file_path_str, source, diagnostics, cli.color);
            if !output.is_empty() {
                print!("{}", output);
            }
        } else {
            all_file_diagnostics.push((file_path_str.clone(), diagnostics.clone()));
        }
    }

    // JSON: output all at once
    if cli.format == "json" {
        println!("{}", format_json_output(&all_file_diagnostics));
    }

    if has_issues_above_threshold {
        process::exit(EXIT_ISSUES);
    }
}

fn run_fix_fmt(files: &[FileInfo], config: &Config) {
    use lint4d::engine::FileType;

    let results: Vec<_> = files
        .par_iter()
        .filter_map(|file| {
            if matches!(file.file_type, FileType::Dpr | FileType::Dpk) {
                return None;
            }
            let source = fs::read(&file.path).ok()?;
            match fix_file(file, &source, config) {
                Ok((new_source, count)) => {
                    if count > 0 {
                        Some((file.path.clone(), new_source, count))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    eprintln!(
                        "lint4d: warning: failed to fix {}: {}",
                        file.path.display(),
                        e
                    );
                    None
                }
            }
        })
        .collect();

    for (path, new_source, count) in &results {
        if let Err(e) = fs::write(path, new_source) {
            eprintln!("lint4d: error: failed to write {}: {}", path.display(), e);
            process::exit(EXIT_ERROR);
        }
        eprintln!("Fixed {} identifier(s) in {}", count, path.display());
    }
}

fn run_generate_baseline(
    cwd: &std::path::Path,
    file_results: &[(String, Vec<u8>, Vec<lint4d::engine::Diagnostic>)],
) {
    let mut baseline = Baseline::new();
    let mut total_violations = 0;

    for (file_path_str, source, diagnostics) in file_results {
        if diagnostics.is_empty() {
            continue;
        }

        let source_text = String::from_utf8_lossy(source);
        let source_lines: Vec<&str> = source_text.lines().collect();

        let diag_refs: Vec<&lint4d::engine::Diagnostic> = diagnostics.iter().collect();
        let line_contents: Vec<&str> = diagnostics
            .iter()
            .map(|d| {
                source_lines
                    .get(d.line.saturating_sub(1))
                    .copied()
                    .unwrap_or("")
            })
            .collect();

        let file_baseline = Baseline::from_diagnostics(file_path_str, &diag_refs, &line_contents);
        total_violations += file_baseline.violations.len();
        baseline.violations.extend(file_baseline.violations);
    }

    let baseline_path = cwd.join(BASELINE_FILENAME);
    let json = baseline.to_json();
    if let Err(e) = fs::write(&baseline_path, &json) {
        exit_err(&format!("failed to write baseline: {}", e), EXIT_ERROR);
    }

    eprintln!(
        "Baseline generated: {} violation(s) recorded in {}",
        total_violations, BASELINE_FILENAME
    );
}

fn load_baseline(cwd: &std::path::Path) -> Option<Baseline> {
    let baseline_path = cwd.join(BASELINE_FILENAME);
    if !baseline_path.exists() {
        return None;
    }

    let content = match fs::read_to_string(&baseline_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "lint4d: warning: failed to read {}: {}",
                BASELINE_FILENAME, e
            );
            return None;
        }
    };

    match Baseline::from_json(&content) {
        Ok(bl) => {
            eprintln!(
                "Loaded baseline with {} suppressed violation(s)",
                bl.violations.len()
            );
            Some(bl)
        }
        Err(e) => {
            eprintln!("lint4d: warning: {}", e);
            None
        }
    }
}

fn run_init() {
    let config_path = PathBuf::from(".lint4d.toml");
    if config_path.exists() {
        eprintln!("lint4d: .lint4d.toml already exists");
        process::exit(EXIT_ERROR);
    }

    let default_config = r#"version = 1

[lint4d]
paths = ["."]
exclude = ["**/test/**", "**/tests/**"]
# dcu_paths = []
# platform = "Win64"
# build_config = "Debug"
# bds_path = "C:/Program Files (x86)/Embarcadero/Studio/23.0"

[rules]
# Override rule severity: "error", "warning", "hint", or "off"
# empty-except = "warning"
# bare-except = "warning"
# with-statement = "warning"
# resource-leak-unprotected = "error"
# resource-leak-no-try = "warning"
# type-prefix = "hint"
# interface-prefix = "hint"
# constant-naming = "hint"
"#;

    if let Err(e) = fs::write(&config_path, default_config) {
        exit_err(&format!("failed to write .lint4d.toml: {}", e), EXIT_ERROR);
    }

    println!("Created .lint4d.toml");
}

fn run_list_rules() {
    let registry = RuleRegistry::new();
    println!("{:<35} {:<10} DESCRIPTION", "RULE ID", "SEVERITY");
    println!("{}", "-".repeat(80));
    for rule in registry.all_rules() {
        let meta = rule.meta();
        let id_display = if rule.requires_context() {
            format!("{} [DCU]", meta.id)
        } else {
            meta.id.to_string()
        };
        println!(
            "{:<35} {:<10} {}",
            id_display, meta.default_severity, meta.description
        );
    }
}

fn discover_dcu_paths_from_project(
    dproj_path: &Path,
    bds_path_override: Option<&Path>,
    platform_override: Option<&str>,
    config_override: Option<&str>,
) -> Vec<PathBuf> {
    use lint4d::discovery::bds;
    use lint4d::discovery::dproj::parse_project_version;
    use lint4d::discovery::msbuild;

    // Extract ProjectVersion from dproj for BDS version lookup
    let project_version = match parse_project_version(dproj_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lint4d: warning: failed to read ProjectVersion: {}", e);
            None
        }
    };

    // Discover BDS root
    let bds_root = bds::discover_bds_root(bds_path_override, project_version.as_deref());

    let bds_root = match bds_root {
        Some(root) => root,
        None => {
            eprintln!(
                "lint4d: warning: RAD Studio installation not found — \
                 skipping DCU auto-discovery. Use --bds-path or configure dcu_paths manually."
            );
            return Vec::new();
        }
    };

    let rsvars = bds::rsvars_bat_path(&bds_root);
    if !rsvars.is_file() {
        eprintln!(
            "lint4d: warning: rsvars.bat not found at {} — \
             is RAD Studio installed correctly?",
            rsvars.display()
        );
        return Vec::new();
    }

    let paths = msbuild::discover_dcu_paths_via_msbuild(
        dproj_path,
        &rsvars,
        platform_override,
        config_override,
    );

    if paths.is_empty() {
        eprintln!("lint4d: warning: MSBuild auto-discovery found no DCU directories");
    } else {
        eprintln!(
            "Auto-discovered {} DCU director{}",
            paths.len(),
            if paths.len() == 1 { "y" } else { "ies" }
        );
    }

    paths
}

fn run_explain(rule_id: &str) {
    let registry = RuleRegistry::new();
    match registry.get(rule_id) {
        Some(rule) => {
            let meta = rule.meta();
            println!("Rule: {}", meta.id);
            println!("Name: {}", meta.name);
            println!("Category: {:?}", meta.category);
            println!("Default severity: {}", meta.default_severity);
            println!();
            println!("{}", meta.description);
        }
        None => {
            eprintln!(
                "Unknown rule: '{}'. Use --list-rules to see available rules.",
                rule_id
            );
            process::exit(EXIT_ISSUES);
        }
    }
}
