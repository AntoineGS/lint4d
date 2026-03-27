mod config;

use clap::{CommandFactory, Parser};
use std::path::PathBuf;
use std::process;

const EXIT_OK: i32 = 0;

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

    /// Enable colored output
    #[arg(long)]
    color: bool,
}

fn main() {
    let cli = Cli::parse();

    if cli.paths.is_empty() && cli.project.is_none() && !cli.stdin {
        Cli::command().print_help().ok();
        println!();
        process::exit(EXIT_OK);
    }

    process::exit(EXIT_OK);
}
