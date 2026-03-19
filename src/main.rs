use std::fs;
use std::path::PathBuf;
use std::process;

use clap::{CommandFactory, Parser};
use lint4d::config::Config;
use lint4d::discovery::discover_files;
use lint4d::engine::{run_lint, Severity};
use lint4d::output::json::format_json_output;
use lint4d::output::text::format_diagnostics;
use lint4d::rules::RuleRegistry;
use rayon::prelude::*;

/// Exit codes
const EXIT_OK: i32 = 0;
const EXIT_ISSUES: i32 = 1;
const EXIT_ERROR: i32 = 2;

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

    /// Generate a baseline file (not yet implemented)
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

    // --generate-baseline: placeholder
    if cli.generate_baseline {
        eprintln!("lint4d: --generate-baseline is not yet implemented");
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

    // --project: not yet implemented (Task 21)
    if cli.project.is_some() {
        eprintln!("lint4d: --project is not yet implemented");
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

    // Build file list from CLI paths
    let files = match discover_files(&cli.paths, &config.exclude) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("lint4d: file discovery error: {}", e);
            process::exit(EXIT_ERROR);
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
            let diagnostics = run_lint(file, &source, &config);
            Some((file.path.to_string_lossy().to_string(), source, diagnostics))
        })
        .collect();

    // Sort by file path for deterministic output
    file_results.sort_by(|a, b| a.0.cmp(&b.0));

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
    println!("{:<30} {:<10} {}", "RULE ID", "SEVERITY", "DESCRIPTION");
    println!("{}", "-".repeat(80));
    for rule in registry.all_rules() {
        let meta = rule.meta();
        println!(
            "{:<30} {:<10} {}",
            meta.id,
            meta.default_severity,
            meta.description
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
