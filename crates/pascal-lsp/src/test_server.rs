fn main() {
    match pascal_lsp::server::run_stdio() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("pascal-lsp-test-server: {error}");
            std::process::exit(1);
        }
    }
}
