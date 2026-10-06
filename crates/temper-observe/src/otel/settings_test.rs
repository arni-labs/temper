use opentelemetry::Key;
use opentelemetry::trace::{
    SamplingDecision, SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId,
    TraceState,
};
use opentelemetry_sdk::trace::ShouldSample;

use super::*;

fn settings(variables: &[(&str, &str)]) -> ExportSettings {
    ExportSettings::from_lookup(|name| {
        variables
            .iter()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| value.to_string())
    })
}

fn computed() -> ComputedAttributes {
    ComputedAttributes {
        environment: None,
        version: None,
        runtime_id: "runtime-1".to_string(),
    }
}

fn attribute(resource: &Resource, key: &'static str) -> Option<String> {
    resource
        .get(&Key::from_static_str(key))
        .map(|value| value.to_string())
}

#[test]
fn exporter_variables_switch_signals_off_one_by_one() {
    assert_eq!(settings(&[]).signals(), Signals::default());
    assert_eq!(Signals::default().label(), "traces + metrics + logs");

    let settings = settings(&[
        (TRACES_EXPORTER_ENV, "otlp"),
        (METRICS_EXPORTER_ENV, "None"),
        (LOGS_EXPORTER_ENV, "none"),
    ]);
    let expected = Signals {
        traces: true,
        metrics: false,
        logs: false,
    };
    assert_eq!(settings.signals(), expected);
    assert_eq!(expected.label(), "traces");
    assert!(settings.warnings().is_empty());
}

#[test]
fn unsupported_exporter_is_reported_and_exported_as_otlp() {
    let settings = settings(&[(METRICS_EXPORTER_ENV, "prometheus")]);
    assert_eq!(settings.signals(), Signals::default());
    let [warning] = settings.warnings() else {
        panic!("expected one warning, got {:?}", settings.warnings());
    };
    assert!(warning.contains("OTEL_METRICS_EXPORTER=prometheus is not supported"));
}

#[test]
fn nothing_set_gives_the_built_in_resource_and_no_warnings() {
    let settings = settings(&[]);
    let resource = settings.resource("built-in", computed());
    assert_eq!(resource.len(), 2);
    assert_eq!(
        attribute(&resource, "service.name").as_deref(),
        Some("built-in")
    );
    assert_eq!(
        attribute(&resource, "runtime-id").as_deref(),
        Some("runtime-1")
    );
    assert!(settings.warnings().is_empty());
}

#[test]
fn resource_attributes_are_trimmed_and_the_last_duplicate_wins() {
    let settings = settings(&[(RESOURCE_ATTRIBUTES_ENV, " a = 1 ,b=2,a=3,empty=,")]);
    let resource = settings.resource("built-in", computed());
    assert_eq!(attribute(&resource, "a").as_deref(), Some("3"));
    assert_eq!(attribute(&resource, "b").as_deref(), Some("2"));
    assert_eq!(attribute(&resource, "empty").as_deref(), Some(""));
    assert!(settings.warnings().is_empty());
}

#[test]
fn entries_that_are_not_key_value_are_ignored_with_one_warning() {
    let settings = settings(&[(RESOURCE_ATTRIBUTES_ENV, "a=1,token-without-key,=x,b=2")]);
    let resource = settings.resource("built-in", computed());
    assert_eq!(attribute(&resource, "a").as_deref(), Some("1"));
    assert_eq!(attribute(&resource, "b").as_deref(), Some("2"));
    assert_eq!(resource.len(), 4);
    let [warning] = settings.warnings() else {
        panic!("expected one warning, got {:?}", settings.warnings());
    };
    assert!(warning.contains("OTEL_RESOURCE_ATTRIBUTES has 2 entries"));
    assert!(!warning.contains("token-without-key"));
}

#[test]
fn a_single_malformed_entry_is_reported_in_the_singular() {
    let settings = settings(&[(RESOURCE_ATTRIBUTES_ENV, "a=1,oops")]);
    assert_eq!(
        settings.warnings(),
        ["OTEL_RESOURCE_ATTRIBUTES has 1 entry that is not key=value; it is ignored"]
    );
}

#[test]
fn computed_attributes_win_over_resource_attributes() {
    let settings = settings(&[(
        RESOURCE_ATTRIBUTES_ENV,
        "service.name=x,deployment.environment.name=x,service.version=x,runtime-id=x",
    )]);
    let resource = settings.resource(
        "built-in",
        ComputedAttributes {
            environment: Some("prod".to_string()),
            version: Some("1.0".to_string()),
            runtime_id: "runtime-1".to_string(),
        },
    );
    assert_eq!(
        attribute(&resource, "service.name").as_deref(),
        Some("built-in")
    );
    assert_eq!(
        attribute(&resource, "deployment.environment.name").as_deref(),
        Some("prod")
    );
    assert_eq!(
        attribute(&resource, "service.version").as_deref(),
        Some("1.0")
    );
    assert_eq!(
        attribute(&resource, "runtime-id").as_deref(),
        Some("runtime-1")
    );
}

#[test]
fn service_name_variable_replaces_the_built_in_name() {
    let settings = settings(&[
        (SERVICE_NAME_ENV, "orders-eu"),
        (RESOURCE_ATTRIBUTES_ENV, "service.name=x"),
    ]);
    assert_eq!(settings.service_name("built-in"), "orders-eu");
    let resource = settings.resource("built-in", computed());
    assert_eq!(
        attribute(&resource, "service.name").as_deref(),
        Some("orders-eu")
    );
}

/// Whether `sampler` samples a root span, a span whose caller sampled, and a
/// span whose caller did not sample.
fn decisions(sampler: &Sampler) -> [bool; 3] {
    let trace_id = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").expect("trace id");
    let caller = |flags: TraceFlags| {
        let span_id = SpanId::from_hex("b7ad6b7169203331").expect("span id");
        let context = SpanContext::new(trace_id, span_id, flags, true, TraceState::default());
        opentelemetry::Context::new().with_remote_span_context(context)
    };
    let sampled = |parent: Option<&opentelemetry::Context>| {
        let result = sampler.should_sample(parent, trace_id, "span", &SpanKind::Server, &[], &[]);
        matches!(result.decision, SamplingDecision::RecordAndSample)
    };
    [
        sampled(None),
        sampled(Some(&caller(TraceFlags::SAMPLED))),
        sampled(Some(&caller(TraceFlags::default()))),
    ]
}

#[test]
fn sampler_defaults_to_following_the_caller() {
    let settings = settings(&[(TRACES_SAMPLER_ARG_ENV, "0")]);
    assert!(settings.chosen_sampler().is_none());
    assert_eq!(decisions(&settings.sampler()), [true, true, false]);
    assert!(settings.warnings().is_empty());
}

#[test]
fn every_supported_sampler_is_selected_by_name() {
    let cases = [
        ("always_on", "", [true, true, true]),
        ("ALWAYS_ON", "", [true, true, true]),
        ("always_off", "", [false, false, false]),
        ("parentbased_always_on", "", [true, true, false]),
        ("parentbased_always_off", "", [false, true, false]),
        ("traceidratio", "", [true, true, true]),
        ("traceidratio", "1", [true, true, true]),
        ("traceidratio", "0", [false, false, false]),
        ("parentbased_traceidratio", "1.0", [true, true, false]),
        ("parentbased_traceidratio", "0.0", [false, true, false]),
    ];
    for (name, arg, expected) in cases {
        let mut variables = vec![(TRACES_SAMPLER_ENV, name)];
        if !arg.is_empty() {
            variables.push((TRACES_SAMPLER_ARG_ENV, arg));
        }
        let settings = settings(&variables);
        assert!(settings.chosen_sampler().is_some(), "{name}");
        assert_eq!(decisions(&settings.sampler()), expected, "{name} {arg}");
        assert!(settings.warnings().is_empty(), "{name} {arg}");
    }
}

#[test]
fn unsupported_sampler_is_reported_and_the_default_is_used() {
    let settings = settings(&[(TRACES_SAMPLER_ENV, "jaeger_remote")]);
    assert!(settings.chosen_sampler().is_none());
    assert_eq!(decisions(&settings.sampler()), [true, true, false]);
    let [warning] = settings.warnings() else {
        panic!("expected one warning, got {:?}", settings.warnings());
    };
    assert!(warning.contains("OTEL_TRACES_SAMPLER=jaeger_remote is not supported"));
    assert!(warning.contains("using parentbased_always_on"));
}

#[test]
fn ratio_that_is_not_between_0_and_1_is_reported_and_1_is_used() {
    for arg in ["half", "1.5", "-0.1", "NaN"] {
        let settings = settings(&[
            (TRACES_SAMPLER_ENV, "traceidratio"),
            (TRACES_SAMPLER_ARG_ENV, arg),
        ]);
        assert_eq!(decisions(&settings.sampler()), [true, true, true], "{arg}");
        let [warning] = settings.warnings() else {
            panic!("expected one warning, got {:?}", settings.warnings());
        };
        assert!(
            warning.contains(&format!("OTEL_TRACES_SAMPLER_ARG={arg} is not a ratio")),
            "{warning}"
        );
    }
}
