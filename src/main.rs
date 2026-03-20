use std::fs;
use std::path::PathBuf;
use std::process;

use clap::{CommandFactory, Parser};
use lint4d::config::baseline::Baseline;
use lint4d::config::Config;
use lint4d::discovery::discover_files;
use lint4d::discovery::dproj::parse_dproj;
use lint4d::engine::{run_lint_with_context, Severity};
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
}

fn main() {
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
        Err(e) => {
            eprintln!("lint4d: invalid --fail-on value: {}", e);
            process::exit(EXIT_ERROR);
        }
    };

    // Validate format
    if cli.format != "text" && cli.format != "json" {
        eprintln!(
            "lint4d: invalid --format value: '{}' (expected 'text' or 'json')",
            cli.format
        );
        process::exit(EXIT_ERROR);
    }

    // Discover config
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (config, _project_root) = match Config::discover(&cwd) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("lint4d: config error: {}", e);
            process::exit(EXIT_ERROR);
        }
    };

    // Build ProjectContext if dcu_paths are configured
    let project_context = if !config.dcu_paths().is_empty() {
        let dcu_dirs: Vec<std::path::PathBuf> = config.dcu_paths().iter().map(std::path::PathBuf::from).collect();
        match lint4d::dcu::ProjectContext::from_dcu_paths(&dcu_dirs) {
            Ok(ctx) => {
                let count = ctx.unit_count();
                if count > 0 {
                    eprintln!("Loaded DCU type info from {} unit(s)", count);
                }
                Some(ctx)
            }
            Err(e) => {
                eprintln!("lint4d: warning: failed to load DCU info: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Build file list: --project uses dproj parser, otherwise glob discovery
    let files = if let Some(ref project_path) = cli.project {
        match parse_dproj(project_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("lint4d: project file error: {}", e);
                process::exit(EXIT_ERROR);
            }
        }
    } else {
        match discover_files(&cli.paths, &config.exclude) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("lint4d: file discovery error: {}", e);
                process::exit(EXIT_ERROR);
            }
        }
    };

    if files.is_empty() {
        // No Delphi files found -- not an error, just nothing to do
        if cli.format == "json" {
            println!("{}", format_json_output(&[]));
        }
        process::exit(EXIT_OK);
    }

    // Process files in parallel
    let mut file_results: Vec<_> = files
        .par_iter()
        .filter_map(|file| {
            let source = fs::read(&file.path).ok()?;
            let diagnostics = run_lint_with_context(file, &source, &config, project_context.as_ref());
            Some((file.path.to_string_lossy().to_string(), source, diagnostics))
        })
        .collect();

    // Sort by file path for deterministic output
    file_results.sort_by(|a, b| a.0.cmp(&b.0));

    // --generate-baseline: collect all diagnostics, write baseline, and exit
    if cli.generate_baseline {
        run_generate_baseline(&cwd, &file_results);
        return;
    }

    // Load baseline if it exists
    let baseline = load_baseline(&cwd);

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
            let output = format_diagnostics(file_path_str, source, diagnostics, false);
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
        eprintln!("lint4d: failed to write baseline: {}", e);
        process::exit(EXIT_ERROR);
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
        eprintln!("lint4d: failed to write .lint4d.toml: {}", e);
        process::exit(EXIT_ERROR);
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
