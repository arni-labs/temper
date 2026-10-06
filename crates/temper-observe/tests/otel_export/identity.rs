//! Service name and resource attributes from the standard variables.

use crate::*;

/// `OTEL_SERVICE_NAME` replaces the built-in service name on every signal.
#[test]
fn service_name_follows_otel_service_name() {
    let run = child::run(&[("OTEL_SERVICE_NAME", "orders-eu")]);
    for (signal, resource) in resources_of_all_signals(&run) {
        assert_eq!(
            attribute(&resource, "service.name"),
            Some("orders-eu"),
            "service.name on {signal}"
        );
    }
}

/// Without `OTEL_SERVICE_NAME` the service name is the built-in one.
#[test]
fn service_name_is_built_in_when_unset() {
    let run = child::run(&[]);
    for (signal, resource) in resources_of_all_signals(&run) {
        assert_eq!(
            attribute(&resource, "service.name"),
            Some(BUILT_IN_SERVICE_NAME),
            "service.name on {signal}"
        );
    }
}

/// `OTEL_RESOURCE_ATTRIBUTES` is merged into the resource of every signal,
/// next to what the server computes itself.
#[test]
fn resource_attributes_are_merged_on_every_signal() {
    let run = child::run(&[("OTEL_RESOURCE_ATTRIBUTES", "a=1,b=2")]);
    for (signal, resource) in resources_of_all_signals(&run) {
        assert_eq!(attribute(&resource, "a"), Some("1"), "a on {signal}");
        assert_eq!(attribute(&resource, "b"), Some("2"), "b on {signal}");
        assert_eq!(
            attribute(&resource, "service.name"),
            Some(BUILT_IN_SERVICE_NAME),
            "service.name on {signal}"
        );
        let runtime_id = attribute(&resource, "runtime-id").unwrap_or_default();
        assert!(!runtime_id.is_empty(), "runtime-id missing on {signal}");
    }
}

/// What the server computes itself wins over `OTEL_RESOURCE_ATTRIBUTES`:
/// the runtime id always, the environment and the version when their own
/// variables are set, and the service name unless `OTEL_SERVICE_NAME` is set.
#[test]
fn computed_resource_attributes_keep_precedence() {
    let run = child::run(&[
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "runtime-id=from-attributes,deployment.environment.name=from-attributes,\
                 service.version=from-attributes,service.name=from-attributes,team=storage",
        ),
        ("DD_ENV", "from-env-variable"),
        ("DD_VERSION", "from-version-variable"),
    ]);
    for (signal, resource) in resources_of_all_signals(&run) {
        let expect = |key: &str, value: &str| {
            assert_eq!(attribute(&resource, key), Some(value), "{key} on {signal}");
        };
        expect("deployment.environment.name", "from-env-variable");
        expect("service.version", "from-version-variable");
        expect("service.name", BUILT_IN_SERVICE_NAME);
        expect("team", "storage");
        assert_ne!(
            attribute(&resource, "runtime-id"),
            Some("from-attributes"),
            "runtime-id on {signal}"
        );
    }
}

/// Without their own variables, the environment and the version come from
/// `OTEL_RESOURCE_ATTRIBUTES`, and `OTEL_SERVICE_NAME` wins over a
/// `service.name` given there.
#[test]
fn resource_attributes_supply_what_has_no_variable_of_its_own() {
    let run = child::run(&[
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "deployment.environment.name=staging, service.version = 1.2.3 ,service.name=ignored",
        ),
        ("OTEL_SERVICE_NAME", "orders-eu"),
    ]);
    for (signal, resource) in resources_of_all_signals(&run) {
        let expect = |key: &str, value: &str| {
            assert_eq!(attribute(&resource, key), Some(value), "{key} on {signal}");
        };
        expect("deployment.environment.name", "staging");
        expect("service.version", "1.2.3");
        expect("service.name", "orders-eu");
    }
}
