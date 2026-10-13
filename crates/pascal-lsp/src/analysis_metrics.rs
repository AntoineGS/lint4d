//! Opt-in, bounded reporting of the process analysis counters.
//!
//! Aggregate scalars only: no paths, source text or document names are ever
//! part of a report.

use pascal_project::analysis_metrics::{METRIC_COUNT, MetricSnapshot, PROCESS_COUNTERS};
use std::io::Write;

const PROFILING_ENV: &str = "PASCAL_LSP_ANALYSIS_METRICS";
const PREFIX: &str = "pascal-lsp analysis-metrics ";
const MAX_LINE_BYTES: usize = 4096;

/// Wire names in stable metric-ID order.
const COUNTER_NAMES: [&str; METRIC_COUNT] = [
    "authorizedPayloadReadAttempts",
    "authorizedPayloadBytes",
    "authorizedPayloadReadFailures",
    "projectStampProbes",
    "legacyHashBytes",
    "coreParserInvocations",
    "lspDocumentPreparations",
    "resolverMetadataParses",
    "importBindingCalls",
    "importBindingPairs",
    "projectCacheProbeCalls",
    "projectCacheProbeNanos",
];

/// Enables the process counters when `PASCAL_LSP_ANALYSIS_METRICS=1`.
pub fn initialize_profiling() -> bool {
    let enabled = std::env::var_os(PROFILING_ENV).is_some_and(|value| value == "1");
    if enabled {
        PROCESS_COUNTERS.set_enabled(true);
    }
    enabled
}

/// Writes the single summary line to stderr after the server returned.
pub fn finish_profiling(enabled: bool, orderly_shutdown: bool) {
    if !enabled {
        return;
    }
    let moment = ReportMoment::ProcessExit { orderly_shutdown };
    let mut stderr = std::io::stderr().lock();
    let _ = match bounded_report(&PROCESS_COUNTERS.snapshot(), moment) {
        Ok(report) => writeln!(stderr, "{PREFIX}{report}"),
        Err(error) => writeln!(stderr, "pascal-lsp: analysis metrics unavailable: {error}"),
    };
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ReportMoment {
    ProcessExit {
        orderly_shutdown: bool,
    },
    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    WindowEndpoint,
}

/// Schema-v1 JSON for `snapshot`; the prefixed line including its newline
/// stays within [`MAX_LINE_BYTES`].
pub(crate) fn bounded_report(
    snapshot: &MetricSnapshot,
    moment: ReportMoment,
) -> Result<String, String> {
    let counters = COUNTER_NAMES
        .iter()
        .zip(snapshot.counts)
        .map(|(name, count)| ((*name).to_string(), serde_json::Value::from(count)))
        .collect::<serde_json::Map<_, _>>();
    let (count_scope, orderly_shutdown) = match moment {
        ReportMoment::ProcessExit { orderly_shutdown } => (
            "process_lifetime",
            serde_json::Value::Bool(orderly_shutdown),
        ),
        ReportMoment::WindowEndpoint => ("process_window_endpoint", serde_json::Value::Null),
    };
    let report = serde_json::json!({
        "schema": snapshot.schema,
        "counters": counters,
        "saturated": snapshot.saturated,
        "countScope": count_scope,
        "snapshotConsistency": "independent_atomics",
        "orderlyShutdown": orderly_shutdown,
        "indexCoverage": null,
        "storageUsage": null,
        "decodedPeakBytes": null,
        "semanticCandidateDocumentsBound": null,
        "unavailable": {
            "indexCoverage": "not_instrumented",
            "storageUsage": "persistence_not_implemented",
            "decodedPeakBytes": "not_instrumented",
            "semanticCandidateDocumentsBound": "not_instrumented",
        },
    });
    let text = serde_json::to_string(&report).map_err(|error| error.to_string())?;
    let line_bytes = PREFIX.len() + text.len() + 1;
    if line_bytes > MAX_LINE_BYTES {
        return Err(format!(
            "report line of {line_bytes} bytes exceeds {MAX_LINE_BYTES}"
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn snapshot(fill: u64, saturated: bool) -> MetricSnapshot {
        MetricSnapshot {
            schema: 1,
            counts: std::array::from_fn(|i| fill.saturating_sub(i as u64)),
            saturated,
        }
    }

    fn parse(report: &str) -> Value {
        serde_json::from_str(report).expect("report is JSON")
    }

    #[test]
    fn metrics_report_is_bounded_and_has_no_paths() {
        let report = bounded_report(
            &snapshot(u64::MAX, false),
            ReportMoment::ProcessExit {
                orderly_shutdown: true,
            },
        )
        .expect("report");
        let line = format!("{PREFIX}{report}\n");
        assert!(line.len() <= 4096, "{} bytes", line.len());
        assert!(line.starts_with("pascal-lsp analysis-metrics {"));
        let value = parse(&report);
        assert_eq!(value["schema"], 1);
        assert_eq!(value["countScope"], "process_lifetime");
        assert_eq!(value["snapshotConsistency"], "independent_atomics");
        assert_eq!(value["orderlyShutdown"], true);
        let counters = value["counters"].as_object().expect("counters");
        assert_eq!(counters.len(), METRIC_COUNT);
        for (index, name) in COUNTER_NAMES.iter().enumerate() {
            assert_eq!(
                counters[*name].as_u64(),
                Some(u64::MAX - index as u64),
                "{name}"
            );
        }
        assert!(!report.contains('/') && !report.contains('\\'));
    }

    #[test]
    fn metrics_report_marks_unavailable_fields_null() {
        let report =
            bounded_report(&snapshot(1, false), ReportMoment::WindowEndpoint).expect("report");
        let value = parse(&report);
        assert_eq!(value["countScope"], "process_window_endpoint");
        assert!(value["orderlyShutdown"].is_null());
        for (field, reason) in [
            ("indexCoverage", "not_instrumented"),
            ("storageUsage", "persistence_not_implemented"),
            ("decodedPeakBytes", "not_instrumented"),
            ("semanticCandidateDocumentsBound", "not_instrumented"),
        ] {
            assert!(value[field].is_null(), "{field}");
            assert_eq!(value["unavailable"][field], reason, "{field}");
        }
        assert_eq!(value["unavailable"].as_object().expect("map").len(), 4);
    }

    #[test]
    fn metrics_report_preserves_saturation() {
        let report = bounded_report(
            &snapshot(5, true),
            ReportMoment::ProcessExit {
                orderly_shutdown: false,
            },
        )
        .expect("report");
        let value = parse(&report);
        assert_eq!(value["saturated"], true);
        assert_eq!(value["orderlyShutdown"], false);
    }
}
