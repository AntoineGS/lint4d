use pascal_lsp::cache_admin::Invocation;
use std::env;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!(
        "pascal-lsp {VERSION}\n\nNative Linux Delphi/Object Pascal language server.\n\nUSAGE:\n    pascal-lsp [--stdio]\n    pascal-lsp (--cache-status | --cache-prune | --cache-reset) [--cache-directory DIR]\n    pascal-lsp [--cache-max-disk-bytes N] [--cache-wal-threshold-bytes N] [--cache-directory DIR]\n\nOPTIONS:\n    --stdio       Use JSON-RPC over stdin/stdout (the default)\n    -h, --help    Print this help message\n    -V, --version Print version information\n\nCACHE ADMINISTRATION (the server is not started; one JSON line on stdout):\n    --cache-status                  Report the persistent cache store without creating it\n    --cache-prune                   Run bounded maintenance cycles until the store is within budget\n    --cache-reset                   Reset this build's namespace; live servers reopen\n    --cache-max-disk-bytes N        Set the store's disk budget (creates the store)\n    --cache-wal-threshold-bytes N   Set the store's WAL threshold (creates the store)\n    --cache-directory DIR           Absolute store directory (default: the user cache directory)\n\nEXIT CODES:\n    0  done (an absent store is a success)\n    1  refused or failed (see the error field)\n    2  usage error"
    );
}

fn usage_error(error: impl std::fmt::Display) -> ! {
    eprintln!("pascal-lsp: {error}\n");
    eprintln!("Use --help for usage.");
    std::process::exit(2);
}

fn main() {
    // Before any thread exists: two glibc arenas instead of up to eight per
    // core (post-crawl high water mark 7,017 -> 5,412 MiB on an 8,670-member
    // project, measured with MALLOC_ARENA_MAX=2); an explicit
    // MALLOC_ARENA_MAX wins.
    pascal_lsp::allocator::limit_malloc_arenas();
    let arguments: Vec<String> = env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .unwrap_or_else(|_| usage_error("arguments must be valid UTF-8"))
        })
        .collect();
    match pascal_lsp::cache_admin::parse_arguments(&arguments) {
        Err(error) => usage_error(error),
        Ok(Invocation::Help) => print_help(),
        Ok(Invocation::Version) => println!("pascal-lsp {VERSION}"),
        Ok(Invocation::Cache { directory, mode }) => {
            let outcome = pascal_lsp::cache_admin::run(directory, mode);
            println!("{}", outcome.stdout);
            if let Some(line) = outcome.stderr {
                eprintln!("{line}");
            }
            std::process::exit(outcome.code);
        }
        Ok(Invocation::Server) => run_server(),
    }
}

fn run_server() {
    let profiling = pascal_lsp::analysis_metrics::initialize_profiling();
    // The main loop also analyses (opening a document warms it), so it runs
    // with the analysis threads' stack (l4d-4nbs review I4).
    let result = std::thread::Builder::new()
        .name("PascalLspMain".to_string())
        .stack_size(pascal_lsp::server::ANALYSIS_THREAD_STACK_BYTES)
        .spawn(pascal_lsp::server::run_stdio)
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
        .and_then(|server| {
            server
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    pascal_lsp::analysis_metrics::finish_profiling(profiling, matches!(&result, Ok(true)));
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("pascal-lsp: {error}");
            std::process::exit(1);
        }
    }
}
