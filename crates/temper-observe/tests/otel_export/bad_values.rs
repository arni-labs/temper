//! Values that are not supported, empty values and credentials.

use crate::*;

/// The child started, reported `expected_warning` exactly once, on its own
/// log output and as an exported log record, and exported all three signals.
fn assert_one_warning(run: &Run, expected_warning: &str) {
    assert_eq!(warnings(run), [expected_warning]);
    let exported: Vec<String> = run
        .logs()
        .into_iter()
        .filter(|record| record.severity_text == "WARN" && record.scope == "temper_observe::otel")
        .map(|record| record.body)
        .collect();
    assert_eq!(exported, [expected_warning]);
    for path in SIGNAL_PATHS {
        assert!(run.requests_to(path) > 0, "no export reached {path}");
    }
}

/// An exporter that is not supported is reported once and the signal is
/// exported as by default.
#[test]
fn unsupported_exporter_is_reported_once_and_exported() {
    for variable in [
        "OTEL_TRACES_EXPORTER",
        "OTEL_METRICS_EXPORTER",
        "OTEL_LOGS_EXPORTER",
    ] {
        let run = child::run(&[(variable, "console")]);
        assert_one_warning(
            &run,
            &format!(
                "OTEL export setting: {variable}=console is not supported \
                 (expected otlp or none); exporting as otlp"
            ),
        );
        assert_eq!(span_names(&run), SPANS_FOLLOWING_THE_CALLER, "{variable}");
    }
}

/// A sampler that is not supported is reported once and the default sampler
/// is used.
#[test]
fn unsupported_sampler_is_reported_once_and_the_default_is_used() {
    let run = child::run(&[("OTEL_TRACES_SAMPLER", "jaeger_remote")]);
    assert_one_warning(
        &run,
        "OTEL export setting: OTEL_TRACES_SAMPLER=jaeger_remote is not supported \
         (expected always_on, always_off, traceidratio, parentbased_always_on, \
         parentbased_always_off or parentbased_traceidratio); using parentbased_always_on",
    );
    assert_eq!(span_names(&run), SPANS_FOLLOWING_THE_CALLER);
}

/// A sampler ratio that is not a number from 0 to 1 is reported once and the
/// ratio's default, 1, is used.
#[test]
fn bad_sampler_ratio_is_reported_once_and_one_is_used() {
    let run = child::run(&[
        ("OTEL_TRACES_SAMPLER", "traceidratio"),
        ("OTEL_TRACES_SAMPLER_ARG", "half"),
    ]);
    assert_one_warning(
        &run,
        "OTEL export setting: OTEL_TRACES_SAMPLER_ARG=half is not a ratio from 0 to 1; using 1",
    );
    assert_eq!(span_names(&run), EVERY_SPAN);
}

/// Resource attribute entries that are not `key=value` are reported once,
/// without being printed, and the resource is the default one.
#[test]
fn malformed_resource_attributes_are_reported_once_and_ignored() {
    let run = child::run(&[("OTEL_RESOURCE_ATTRIBUTES", "no-equals-sign,=no-key")]);
    assert_one_warning(
        &run,
        "OTEL export setting: OTEL_RESOURCE_ATTRIBUTES has 2 entries that are not key=value; \
         they are ignored",
    );
    for (signal, resource) in resources_of_all_signals(&run) {
        let keys: Vec<&str> = resource.keys().map(String::as_str).collect();
        assert_eq!(keys, ["runtime-id", "service.name"], "resource of {signal}");
    }
}

/// An empty value is the same as an unset variable: default behaviour and
/// nothing reported.
#[test]
fn empty_values_are_the_same_as_unset() {
    let run = child::run(&[
        ("OTEL_SERVICE_NAME", ""),
        ("OTEL_RESOURCE_ATTRIBUTES", " "),
        ("OTEL_TRACES_EXPORTER", ""),
        ("OTEL_METRICS_EXPORTER", ""),
        ("OTEL_LOGS_EXPORTER", ""),
        ("OTEL_TRACES_SAMPLER", ""),
        ("OTEL_TRACES_SAMPLER_ARG", ""),
    ]);
    assert_matches_baseline(&run, "the export with every setting empty");
}

/// The credential in the OTLP headers reaches the backend on every signal
/// and is never printed or exported, whatever else is reported at startup.
#[test]
fn credentials_are_sent_and_never_logged() {
    const SECRET: &str = "s3cr3t-credential";
    let run = child::run(&[
        (
            "OTEL_EXPORTER_OTLP_HEADERS",
            "authorization=Bearer s3cr3t-credential",
        ),
        ("OTEL_RESOURCE_ATTRIBUTES", "team=storage,s3cr3t-credential"),
        ("OTEL_SERVICE_NAME", "orders-eu"),
        ("OTEL_TRACES_EXPORTER", "console"),
        ("OTEL_TRACES_SAMPLER", "traceidratio"),
        ("OTEL_TRACES_SAMPLER_ARG", "half"),
    ]);
    for path in SIGNAL_PATHS {
        assert!(run.requests_to(path) > 0, "no export reached {path}");
    }
    for request in &run.received {
        assert_eq!(
            request.header("authorization"),
            "Bearer s3cr3t-credential",
            "authorization header on {}",
            request.path
        );
    }
    assert_eq!(warnings(&run).len(), 3, "{:?}", warnings(&run));
    assert!(!run.stdout.contains(SECRET), "credential on stdout");
    assert!(!run.stderr.contains(SECRET), "credential on stderr");
    assert!(
        !render::render(&run).contains(SECRET),
        "credential in the exported telemetry"
    );
}
