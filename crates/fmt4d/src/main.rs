use clap::{CommandFactory, Parser};
use fmt4d::config::{EndOfLine, FmtConfig};
use fmt4d::formatter::format_source;
use fmt4d::uses;
use pascal_core::discovery::discover_files;
use pascal_core::FileInfo;
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};

const EXIT_OK: i32 = 0;
const EXIT_FORMAT_NEEDED: i32 = 1;
const EXIT_ERROR: i32 = 2;

#[derive(Parser)]
#[command(
    name = "fmt4d",
    version,
    about = "An opinionated Delphi/Object Pascal formatter"
)]
struct Cli {
    /// Files or directories to format
    paths: Vec<PathBuf>,

    /// Check if files are formatted (exit 1 if not)
    #[arg(long)]
    check: bool,

    /// Show diff of what would change
    #[arg(long)]
    diff: bool,

    /// Read from stdin, write to stdout
    #[arg(long)]
    stdin: bool,

    /// Format files from a .dproj project file
    #[arg(long)]
    project: Option<PathBuf>,

    /// Override indent size
    #[arg(long)]
    indent_size: Option<usize>,

    /// Override max line length
    #[arg(long)]
    max_line_length: Option<usize>,

    /// Line ending style: auto (preserve original), crlf, lf
    #[arg(long, value_parser = parse_end_of_line)]
    end_of_line: Option<EndOfLine>,

    /// Enable colored output
    #[arg(long)]
    color: bool,

    /// Create a default .fmt4d.toml in the current directory
    #[arg(long)]
    init: bool,
}

fn parse_end_of_line(s: &str) -> Result<EndOfLine, String> {
    match s.to_lowercase().as_str() {
        "auto" => Ok(EndOfLine::Auto),
        "crlf" => Ok(EndOfLine::Crlf),
        "lf" => Ok(EndOfLine::Lf),
        _ => Err(format!(
            "invalid end_of_line value '{}': expected auto, crlf, or lf",
            s
        )),
    }
}

fn main() {
    let cli = Cli::parse();

    if cli.init {
        process::exit(run_init());
    }

    if cli.stdin {
        process::exit(run_stdin(&cli));
    }

    if cli.paths.is_empty() && cli.project.is_none() {
        Cli::command().print_help().ok();
        println!();
        process::exit(EXIT_OK);
    }

    // Auto-detect .dproj files passed as positional args
    let mut cli = cli;
    if cli.project.is_none() {
        if let Some(pos) = cli.paths.iter().position(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("dproj"))
        }) {
            cli.project = Some(cli.paths.remove(pos));
        }
    }

    process::exit(run_files(&cli));
}

fn run_stdin(cli: &Cli) -> i32 {
    let mut input = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut input) {
        eprintln!("Error reading stdin: {}", e);
        return EXIT_ERROR;
    }

    let info = FileInfo::new(PathBuf::from("stdin.pas"));
    let config =
        FmtConfig::default().with_overrides(cli.indent_size, cli.max_line_length, cli.end_of_line);

    match format_source(input.as_bytes(), &info, &config, &HashSet::new()) {
        Ok(formatted) => {
            print!("{}", formatted);
            EXIT_OK
        }
        Err(e) => {
            eprintln!("Error formatting stdin: {}", e);
            EXIT_ERROR
        }
    }
}

fn run_files(cli: &Cli) -> i32 {
    let mut files = match discover_files(&cli.paths, &[]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error discovering files: {}", e);
            return EXIT_ERROR;
        }
    };

    // If --project is specified, discover files from the .dproj
    if let Some(ref dproj_path) = cli.project {
        match pascal_core::discovery_dproj::parse_dproj(dproj_path) {
            Ok(project_files) => {
                files.extend(project_files);
            }
            Err(e) => {
                eprintln!("Error parsing {}: {}", dproj_path.display(), e);
                return EXIT_ERROR;
            }
        }
    }

    if files.is_empty() {
        eprintln!("No Delphi source files found.");
        return EXIT_OK;
    }

    // Discover config from the project dir or first path's directory
    let config_dir = cli
        .project
        .as_ref()
        .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
        .or_else(|| {
            cli.paths.first().and_then(|p| {
                if p.is_dir() {
                    Some(p.clone())
                } else {
                    p.parent().map(|pp| pp.to_path_buf())
                }
            })
        })
        .unwrap_or_else(|| PathBuf::from("."));

    let config = FmtConfig::discover(&config_dir).with_overrides(
        cli.indent_size,
        cli.max_line_length,
        cli.end_of_line,
    );

    let external_units = if config.uses.group {
        match &config.project_root {
            Some(root) => uses::scan_external_paths(root, &config.uses.external_paths),
            None => HashSet::new(),
        }
    } else {
        HashSet::new()
    };

    let had_changes = AtomicBool::new(false);
    let had_errors = AtomicBool::new(false);

    files.par_iter().for_each(|file_info| {
        let source = match fs::read(&file_info.path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading {}: {}", file_info.path.display(), e);
                had_errors.store(true, Ordering::Relaxed);
                return;
            }
        };

        let formatted = match format_source(&source, file_info, &config, &external_units) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Error formatting {}: {}", file_info.path.display(), e);
                had_errors.store(true, Ordering::Relaxed);
                return;
            }
        };

        let original = String::from_utf8_lossy(&source);
        if formatted == original.as_ref() {
            return; // No changes needed
        }

        had_changes.store(true, Ordering::Relaxed);

        if cli.check {
            println!("Would reformat: {}", file_info.path.display());
        } else if cli.diff {
            print_diff(&file_info.path, &original, &formatted);
        } else {
            // Write formatted output back to the file
            if let Err(e) = fs::write(&file_info.path, &formatted) {
                eprintln!("Error writing {}: {}", file_info.path.display(), e);
                had_errors.store(true, Ordering::Relaxed);
                return;
            }
            println!("Formatted: {}", file_info.path.display());
        }
    });

    if had_errors.load(Ordering::Relaxed) {
        return EXIT_ERROR;
    }

    if cli.check && had_changes.load(Ordering::Relaxed) {
        return EXIT_FORMAT_NEEDED;
    }

    EXIT_OK
}

fn run_init() -> i32 {
    let config_path = PathBuf::from(".fmt4d.toml");
    if config_path.exists() {
        eprintln!("fmt4d: .fmt4d.toml already exists");
        return EXIT_ERROR;
    }

    let default_config = r#"# Configuration for fmt4d - Delphi/Object Pascal formatter

[format]
# Number of spaces or tabs per indentation level
# Default: 2
# indent_size = 2

# Indentation style
# Possible values: "space", "tab"
# Default: "space"
# indent_style = "space"

# Maximum line length before wrapping
# Default: 120
# max_line_length = 120

# Style for begin keyword placement
# Possible values: "next_line", "same_line"
# Default: "next_line"
# begin_style = "next_line"

# Line ending style
# Possible values: "auto" (preserve original), "crlf" (Windows), "lf" (Unix/Mac)
# Default: "auto"
# end_of_line = "auto"

[format.blank_lines]
# Number of blank lines between procedures/functions
# Default: 1
# between_procedures = 1

# Number of blank lines between sections (interface/implementation/etc)
# Default: 1
# between_sections = 1

# Maximum consecutive blank lines allowed
# Default: 1
# max_consecutive = 1

[format.uses]
# Sort uses clause alphabetically
# Default: true
# sort = true

# Group uses clause by internal/external units
# Default: false
# group = false

# Paths that should be considered external (for grouping)
# Default: []
# external_paths = ["vendor", "lib/third-party"]

# Prefixes that should be considered external (for grouping)
# Default: []
# external_prefixes = ["Spring", "Neon", "DevExpress"]
"#;

    if let Err(e) = fs::write(&config_path, default_config) {
        eprintln!("fmt4d: failed to write .fmt4d.toml: {}", e);
        return EXIT_ERROR;
    }

    println!("Created .fmt4d.toml");
    EXIT_OK
}

fn print_diff(path: &std::path::Path, original: &str, formatted: &str) {
    println!("--- {}", path.display());
    println!("+++ {}", path.display());

    let orig_lines: Vec<&str> = original.lines().collect();
    let fmt_lines: Vec<&str> = formatted.lines().collect();

    let max_len = orig_lines.len().max(fmt_lines.len());
    for i in 0..max_len {
        let orig_line = orig_lines.get(i).copied();
        let fmt_line = fmt_lines.get(i).copied();
        match (orig_line, fmt_line) {
            (Some(o), Some(f)) if o == f => {
                println!(" {}", o);
            }
            (Some(o), Some(f)) => {
                println!("-{}", o);
                println!("+{}", f);
            }
            (Some(o), None) => {
                println!("-{}", o);
            }
            (None, Some(f)) => {
                println!("+{}", f);
            }
            (None, None) => {}
        }
    }
}
