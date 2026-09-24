use pascal_lsp::server::{TestBarrierConfig, run_stdio_with_test_barriers};
use std::path::PathBuf;

const TEST_NAVIGATION_BARRIER_ENV: &str = "PASCAL_LSP_TEST_NAVIGATION_BARRIER";
const TEST_FORMATTING_BARRIER_ENV: &str = "PASCAL_LSP_TEST_FORMATTING_BARRIER";
const TEST_DIAGNOSTICS_BARRIER_ENV: &str = "PASCAL_LSP_TEST_DIAGNOSTICS_BARRIER";
const TEST_SELECTION_BARRIER_ENV: &str = "PASCAL_LSP_TEST_SELECTION_BARRIER";
const TEST_COMPLETION_RESOLUTION_BARRIER_ENV: &str =
    "PASCAL_LSP_TEST_COMPLETION_RESOLUTION_BARRIER";
const TEST_WORKSPACE_SYMBOLS_BARRIER_ENV: &str = "PASCAL_LSP_TEST_WORKSPACE_SYMBOLS_BARRIER";
const TEST_REFERENCES_BARRIER_ENV: &str = "PASCAL_LSP_TEST_REFERENCES_BARRIER";
const TEST_PARTIAL_VALIDATION_BARRIER_ENV: &str = "PASCAL_LSP_TEST_PARTIAL_VALIDATION_BARRIER";
const TEST_OUTBOUND_WRITER_BARRIER_ENV: &str = "PASCAL_LSP_TEST_OUTBOUND_WRITER_BARRIER";
const TEST_DISPATCH_LOG_ENV: &str = "PASCAL_LSP_TEST_DISPATCH_LOG";
const TEST_WORKSPACE_FIFO_PROBE_ENV: &str = "PASCAL_LSP_TEST_WORKSPACE_FIFO_PROBE";

fn barrier_from_environment(variable: &str) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let Some(spec) = std::env::var_os(variable) else {
        return Ok(None);
    };
    let spec = spec.to_string_lossy();
    let Some((entered, release)) = spec.split_once('|') else {
        return Err(format!("{variable} must contain <entered>|<release>"));
    };
    Ok(Some((PathBuf::from(entered), PathBuf::from(release))))
}

fn outbound_writer_barrier_from_environment(
    variable: &str,
) -> Result<Option<(PathBuf, PathBuf, PathBuf, usize)>, String> {
    let Some(spec) = std::env::var_os(variable) else {
        return Ok(None);
    };
    let spec = spec.to_string_lossy();
    let mut paths = spec.splitn(4, '|');
    let (Some(armed), Some(entered), Some(release), Some(control_limit)) =
        (paths.next(), paths.next(), paths.next(), paths.next())
    else {
        return Err(format!(
            "{variable} must contain <armed>|<entered>|<release>|<control-limit>"
        ));
    };
    let control_limit = control_limit
        .parse::<usize>()
        .map_err(|_| format!("{variable} control limit must be an integer"))?;
    Ok(Some((
        PathBuf::from(armed),
        PathBuf::from(entered),
        PathBuf::from(release),
        control_limit,
    )))
}

fn workspace_fifo_probe_from_environment(
    variable: &str,
) -> Result<Option<(PathBuf, PathBuf, PathBuf, PathBuf)>, String> {
    let Some(spec) = std::env::var_os(variable) else {
        return Ok(None);
    };
    let spec = spec.to_string_lossy();
    let mut paths = spec.splitn(4, '|');
    let (Some(state), Some(reader_pending), Some(worker_entered), Some(worker_release)) =
        (paths.next(), paths.next(), paths.next(), paths.next())
    else {
        return Err(format!(
            "{variable} must contain <state>|<reader-pending>|<worker-entered>|<worker-release>"
        ));
    };
    Ok(Some((
        PathBuf::from(state),
        PathBuf::from(reader_pending),
        PathBuf::from(worker_entered),
        PathBuf::from(worker_release),
    )))
}

fn test_barrier_config() -> Result<TestBarrierConfig, String> {
    Ok(TestBarrierConfig::new(
        barrier_from_environment(TEST_NAVIGATION_BARRIER_ENV)?,
        barrier_from_environment(TEST_FORMATTING_BARRIER_ENV)?,
        barrier_from_environment(TEST_DIAGNOSTICS_BARRIER_ENV)?,
    )
    .with_selection(barrier_from_environment(TEST_SELECTION_BARRIER_ENV)?)
    .with_completion_resolution(barrier_from_environment(
        TEST_COMPLETION_RESOLUTION_BARRIER_ENV,
    )?)
    .with_workspace_symbols(barrier_from_environment(
        TEST_WORKSPACE_SYMBOLS_BARRIER_ENV,
    )?)
    .with_references(barrier_from_environment(TEST_REFERENCES_BARRIER_ENV)?)
    .with_partial_validation(barrier_from_environment(
        TEST_PARTIAL_VALIDATION_BARRIER_ENV,
    )?)
    .with_outbound_writer(outbound_writer_barrier_from_environment(
        TEST_OUTBOUND_WRITER_BARRIER_ENV,
    )?)
    .with_workspace_fifo_probe(workspace_fifo_probe_from_environment(
        TEST_WORKSPACE_FIFO_PROBE_ENV,
    )?)
    .with_dispatch(std::env::var_os(TEST_DISPATCH_LOG_ENV).map(PathBuf::from)))
}

fn main() {
    let barriers = match test_barrier_config() {
        Ok(barriers) => barriers,
        Err(error) => {
            eprintln!("pascal-lsp-test-server: {error}");
            std::process::exit(1);
        }
    };
    match run_stdio_with_test_barriers(barriers) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("pascal-lsp-test-server: {error}");
            std::process::exit(1);
        }
    }
}
