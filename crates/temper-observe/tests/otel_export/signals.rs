//! Switching a signal off with `OTEL_*_EXPORTER`.

use crate::*;

/// With `variable` set to `none`, nothing reaches `silent_path`, the other
/// two signals are still exported, and the process logs as before.
fn assert_only_one_signal_is_off(variable: &str, silent_path: &str, exported: &str) {
    let run = child::run(&[(variable, "none")]);
    let messages = log_messages(&run);
    for expected in [
        format!("OTEL initialised ({exported})").as_str(),
        "test log inside a span",
        "test log outside a span",
    ] {
        assert!(
            messages.iter().any(|message| message == expected),
            "{variable}=none: no log line {expected:?} in {messages:?}"
        );
    }
    for path in SIGNAL_PATHS {
        let requests = run.requests_to(path);
        if path == silent_path {
            assert_eq!(requests, 0, "{variable}=none still exported to {path}");
        } else {
            assert!(requests > 0, "{variable}=none stopped the export to {path}");
        }
    }
    assert_eq!(warnings(&run), Vec::<String>::new());
}

#[test]
fn traces_exporter_none_switches_only_traces_off() {
    assert_only_one_signal_is_off("OTEL_TRACES_EXPORTER", "/v1/traces", "metrics + logs");
}

#[test]
fn metrics_exporter_none_switches_only_metrics_off() {
    assert_only_one_signal_is_off("OTEL_METRICS_EXPORTER", "/v1/metrics", "traces + logs");
}

#[test]
fn logs_exporter_none_switches_only_logs_off() {
    assert_only_one_signal_is_off("OTEL_LOGS_EXPORTER", "/v1/logs", "traces + metrics");
}

/// `otlp` is the default spelled out: everything is exported as when the
/// variables are unset, and nothing is reported.
#[test]
fn exporters_set_to_otlp_export_as_by_default() {
    let run = child::run(&[
        ("OTEL_TRACES_EXPORTER", "otlp"),
        ("OTEL_METRICS_EXPORTER", "OTLP"),
        ("OTEL_LOGS_EXPORTER", " otlp "),
    ]);
    assert_matches_baseline(&run, "the export with every exporter set to otlp");
}
