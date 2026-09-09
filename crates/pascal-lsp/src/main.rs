use std::env;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!(
        "pascal-lsp {VERSION}\n\nNative Linux Delphi/Object Pascal language server.\n\nUSAGE:\n    pascal-lsp [--stdio]\n\nOPTIONS:\n    --stdio       Use JSON-RPC over stdin/stdout (the default)\n    -h, --help    Print this help message\n    -V, --version Print version information"
    );
}

fn main() {
    for argument in env::args().skip(1) {
        match argument.as_str() {
            "--stdio" => {}
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-V" | "--version" => {
                println!("pascal-lsp {VERSION}");
                return;
            }
            _ => {
                eprintln!("pascal-lsp: unknown argument {argument:?}\n");
                eprintln!("Use --help for usage.");
                std::process::exit(2);
            }
        }
    }

    match pascal_lsp::server::run_stdio() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("pascal-lsp: {error}");
            std::process::exit(1);
        }
    }
}
