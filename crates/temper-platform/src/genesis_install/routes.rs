//! The routes an app declares, made part of its install.
//!
//! An app declares its inbound HTTP routes in `routes.json` at its root:
//! `{"routes": [{"Id": "he-...", "PathPrefix": "/...", "Methods": "POST",
//! "IntegrationModule": "...", ...}]}`, each entry an `HttpEndpoint` row
//! (ADR-0069) by its fields. `why` on an entry and `//` at the top are notes
//! and are not stored. An install makes the tenant's rows what the installed
//! version declares: a declared route is created or set to its fields, and a
//! route the version installed before declared and this one does not is
//! deleted. A route declared by no installed version (one made by hand) is
//! never touched.

use super::*;
use crate::os_apps::AppFileSource;

/// The file an app declares its routes in.
const ROUTES_FILE: &str = "routes.json";

/// The `HttpEndpoint` fields a route may set; `Id` names the row.
const ROUTE_FIELDS: &[&str] = &[
    "PathPrefix",
    "Methods",
    "IntegrationModule",
    "RequiresAuth",
    "TimeoutSecs",
    "MaxFuel",
    "MaxMemory",
    "MaxResponseBytes",
    "ActionBridgeEntityType",
    "ActionBridgeEntityId",
    "ActionBridgeAction",
    "ActionBridgeResponse",
    "AdmissionActions",
];

/// Keys that are notes for the reader, not fields.
const NOTE_KEYS: &[&str] = &["why", "//"];

/// One declared route: its row id and the fields it sets.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DeclaredRoute {
    pub(super) id: String,
    pub(super) fields: serde_json::Map<String, Value>,
}

/// The routes of one app before and after its install.
pub(super) struct RouteChange {
    pub(super) app: String,
    /// What the installed version declares, before the install.
    pub(super) before: Vec<DeclaredRoute>,
    /// What the version being installed declares.
    pub(super) now: Vec<DeclaredRoute>,
}

/// The routes `files` declares: none without a `routes.json`.
///
/// # Errors
///
/// A `routes.json` that does not read as routes refuses the install, naming
/// what is wrong, before anything changes.
pub(super) fn declared_routes(
    app: &str,
    files: &AppFileSource,
) -> Result<Vec<DeclaredRoute>, String> {
    if !files.has(ROUTES_FILE) {
        return Ok(Vec::new());
    }
    let text = files.read_text(ROUTES_FILE)?;
    parse_routes(&text).map_err(|why| format!("{app}/{ROUTES_FILE}: {why}"))
}

fn parse_routes(text: &str) -> Result<Vec<DeclaredRoute>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|error| format!("not JSON: {error}"))?;
    let Some(top) = doc.as_object() else {
        return Err("must be an object with \"routes\"".to_string());
    };
    if let Some(key) = top.keys().find(|key| *key != "routes" && *key != "//") {
        return Err(format!("unknown key \"{key}\""));
    }
    let entries = match top.get("routes") {
        Some(Value::Array(entries)) => entries,
        None => return Ok(Vec::new()),
        Some(_) => return Err("\"routes\" must be a list".to_string()),
    };
    let mut routes: Vec<DeclaredRoute> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            return Err("each route must be an object".to_string());
        };
        let id = entry.get("Id").and_then(Value::as_str).unwrap_or_default();
        let id_ok = !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !id_ok {
            return Err(format!(
                "route Id {:?} must be 1-64 of a-z, 0-9 and -",
                entry.get("Id")
            ));
        }
        if routes.iter().any(|route| route.id == id) {
            return Err(format!("route {id} is declared twice"));
        }
        let mut fields = serde_json::Map::new();
        for (key, value) in entry {
            if key == "Id" || NOTE_KEYS.contains(&key.as_str()) {
                continue;
            }
            if !ROUTE_FIELDS.contains(&key.as_str()) {
                return Err(format!("route {id}: unknown field \"{key}\""));
            }
            fields.insert(key.clone(), value.clone());
        }
        let path = fields.get("PathPrefix").and_then(Value::as_str);
        if !path.is_some_and(|path| path.starts_with('/')) {
            return Err(format!("route {id}: PathPrefix must start with /"));
        }
        for required in ["Methods", "IntegrationModule"] {
            let given = fields.get(required).and_then(Value::as_str);
            if given.is_none_or(|value| value.trim().is_empty()) {
                return Err(format!("route {id}: {required} is required"));
            }
        }
        routes.push(DeclaredRoute {
            id: id.to_string(),
            fields,
        });
    }
    Ok(routes)
}

/// Make the tenant's routes what each app of `changes` now declares.
///
/// # Errors
///
/// Says which route could not be made or deleted, and why. The caller puts
/// the routes back ([`put_back`]) with the rest of the install.
pub(super) async fn apply(
    platform: &PlatformState,
    tenant: &str,
    changes: &[RouteChange],
) -> Result<(), String> {
    let tenant_id = TenantId::new(tenant);
    let mut result = Ok(());
    for change in changes {
        if let Err(error) = settle(platform, &tenant_id, &change.before, &change.now).await {
            result = Err(format!("route of {}: {error}", change.app));
            break;
        }
    }
    temper_server::http_endpoint::rebuild_tenant_table(&platform.server, &tenant_id).await;
    result
}

/// Put the routes back as the versions installed before declared them, after
/// an install that did not go through. Best effort: a route that cannot be
/// put back is named in the answer.
pub(super) async fn put_back(
    platform: &PlatformState,
    tenant: &str,
    changes: &[RouteChange],
) -> Result<(), String> {
    let tenant_id = TenantId::new(tenant);
    let mut errors = Vec::new();
    for change in changes {
        if let Err(error) = settle(platform, &tenant_id, &change.now, &change.before).await {
            errors.push(format!("route of {}: {error}", change.app));
        }
    }
    temper_server::http_endpoint::rebuild_tenant_table(&platform.server, &tenant_id).await;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Move the rows from what `before` declared to what `now` declares.
async fn settle(
    platform: &PlatformState,
    tenant: &TenantId,
    before: &[DeclaredRoute],
    now: &[DeclaredRoute],
) -> Result<(), String> {
    for route in now {
        set_route(platform, tenant, route).await?;
    }
    for route in before {
        if !now.iter().any(|kept| kept.id == route.id) {
            delete_route(platform, tenant, &route.id).await?;
        }
    }
    Ok(())
}

async fn set_route(
    platform: &PlatformState,
    tenant: &TenantId,
    route: &DeclaredRoute,
) -> Result<(), String> {
    let server = &platform.server;
    if !server
        .ensure_entity_loaded(tenant, "HttpEndpoint", &route.id)
        .await
    {
        let mut fields = route.fields.clone();
        fields.insert("Id".to_string(), Value::String(route.id.clone()));
        server
            .get_or_create_tenant_entity(tenant, "HttpEndpoint", &route.id, Value::Object(fields))
            .await
            .map_err(|error| format!("{} could not be created: {error:?}", route.id))?;
        return Ok(());
    }
    let current = server
        .get_tenant_entity_state(tenant, "HttpEndpoint", &route.id)
        .await
        .map_err(|error| format!("{} could not be read: {error}", route.id))?;
    if current.state.status != "Active" {
        return Err(format!(
            "{} is {}; resume or remove it, then install again",
            route.id, current.state.status
        ));
    }
    let same = route
        .fields
        .iter()
        .all(|(key, value)| current.state.fields.get(key) == Some(value));
    if same {
        return Ok(());
    }
    dispatch_route(platform, tenant, &route.id, "Create", route.fields.clone()).await
}

async fn delete_route(platform: &PlatformState, tenant: &TenantId, id: &str) -> Result<(), String> {
    let server = &platform.server;
    if !server
        .ensure_entity_loaded(tenant, "HttpEndpoint", id)
        .await
    {
        return Ok(());
    }
    let current = server
        .get_tenant_entity_state(tenant, "HttpEndpoint", id)
        .await
        .map_err(|error| format!("{id} could not be read: {error}"))?;
    if current.state.status == "Deleted" {
        return Ok(());
    }
    dispatch_route(platform, tenant, id, "Delete", serde_json::Map::new()).await
}

async fn dispatch_route(
    platform: &PlatformState,
    tenant: &TenantId,
    id: &str,
    action: &str,
    params: serde_json::Map<String, Value>,
) -> Result<(), String> {
    let agent_ctx = temper_server::request_context::AgentContext::for_service("genesis-install");
    let response = platform
        .server
        .dispatch(DispatchCommand {
            tenant,
            entity_type: "HttpEndpoint",
            entity_id: id,
            action,
            params: Value::Object(params),
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
        .map_err(|error| format!("{id}.{action}: {error}"))?;
    if response.success {
        Ok(())
    } else {
        Err(format!(
            "{id}.{action} refused: {}",
            response.error.unwrap_or_default()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_routes;

    #[test]
    fn a_route_file_reads_as_its_routes_without_notes() {
        let routes = parse_routes(
            r#"{"//": "note", "routes": [{"why": "w", "Id": "he-a", "PathPrefix": "/a",
                "Methods": "POST", "IntegrationModule": "steps", "TimeoutSecs": 25}]}"#,
        )
        .expect("routes");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].id, "he-a");
        assert!(!routes[0].fields.contains_key("why"));
        assert_eq!(routes[0].fields["TimeoutSecs"], 25);
    }

    #[test]
    fn a_route_file_with_an_unknown_field_or_a_twice_declared_id_is_refused() {
        let unknown = r#"{"routes": [{"Id": "he-a", "PathPrefix": "/a", "Methods": "GET",
            "IntegrationModule": "m", "ForwardsCredential": true}]}"#;
        assert!(
            parse_routes(unknown)
                .unwrap_err()
                .contains("ForwardsCredential")
        );
        let twice = r#"{"routes": [
            {"Id": "he-a", "PathPrefix": "/a", "Methods": "GET", "IntegrationModule": "m"},
            {"Id": "he-a", "PathPrefix": "/b", "Methods": "GET", "IntegrationModule": "m"}]}"#;
        assert!(parse_routes(twice).unwrap_err().contains("twice"));
        let relative = r#"{"routes": [{"Id": "he-a", "PathPrefix": "a", "Methods": "GET",
            "IntegrationModule": "m"}]}"#;
        assert!(parse_routes(relative).unwrap_err().contains("PathPrefix"));
    }
}
