//! A reload parses only the statements it has not seen (ADR-0183).

use super::*;
use crate::context::{PrincipalKind, SecurityContext};

fn statements(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            format!(
                "// rule {i}; with a semicolon in a comment\n@source(\"app/p.cedar#{i}\")\npermit(principal, action == Action::\"read\", resource is Doc) when {{ resource.id == \"d{i};x\" }};\n"
            )
        })
        .collect()
}

fn tenant_set(engine: &AuthzEngine, tenant: &str) -> Vec<(String, serde_json::Value)> {
    let tenants = engine.tenant_policies.read().unwrap();
    let mut out: Vec<(String, serde_json::Value)> = tenants[tenant]
        .policies
        .policy_set
        .policies()
        .map(|p| (p.id().to_string(), p.to_json().unwrap()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn reads(engine: &AuthzEngine, doc: &str) -> bool {
    let who =
        SecurityContext::from_verified_jwt("u", PrincipalKind::Customer, None, None, None, None);
    let attrs = HashMap::from([("id".to_string(), serde_json::json!(doc))]);
    engine
        .authorize_for_tenant("t", &who, "read", "Doc", &attrs)
        .is_allowed()
}

#[test]
fn changing_one_statement_parses_that_statement_and_nothing_else() {
    let engine = AuthzEngine::empty();
    let mut rules = statements(50);
    engine.reload_tenant_policies("t", &rules.concat()).unwrap();
    assert_eq!(engine.statements_parsed(), 50);
    assert!(reads(&engine, "d7;x"));

    rules[7] = rules[7].replace("d7;x", "d7;changed");
    engine.reload_tenant_policies("t", &rules.concat()).unwrap();
    assert_eq!(
        engine.statements_parsed(),
        51,
        "only the changed statement is parsed"
    );
    assert!(!reads(&engine, "d7;x"));
    assert!(reads(&engine, "d7;changed"));
    assert!(reads(&engine, "d8;x"));

    engine.reload_tenant_policies("t", &rules.concat()).unwrap();
    assert_eq!(
        engine.statements_parsed(),
        51,
        "an unchanged reload parses nothing"
    );
    assert_eq!(engine.cached_statement_count("t"), 50);
}

#[test]
fn a_set_built_from_cached_statements_equals_a_whole_parse() {
    let text = statements(20).concat();
    let engine = AuthzEngine::empty();
    engine.reload_tenant_policies("t", &text).unwrap();
    // Built a second time almost entirely from the cache.
    let mut changed = statements(20);
    changed[3] = changed[3].replace("d3;x", "d3;y");
    engine
        .reload_tenant_policies("t", &changed.concat())
        .unwrap();
    engine.reload_tenant_policies("t", &text).unwrap();

    let reference = AuthzEngine::empty();
    let mut whole = text.parse::<PolicySet>().unwrap();
    merge_system_platform_policy(&mut whole);
    reference.tenant_policies.write().unwrap().insert(
        "t".into(),
        TenantPolicies {
            policies: CompiledPolicies::new(whole),
            source_text: text.clone(),
            statements: Arc::default(),
        },
    );
    assert_eq!(tenant_set(&engine, "t"), tenant_set(&reference, "t"));
    assert_eq!(
        engine.get_tenant_policy_text("t").as_deref(),
        Some(text.as_str())
    );
}

#[test]
fn named_rows_reuse_parsed_statements_and_keep_their_ids() {
    let engine = AuthzEngine::empty();
    let rows: Vec<(String, String)> = statements(3)
        .into_iter()
        .enumerate()
        .map(|(i, text)| (format!("app/p.cedar#{i}"), text))
        .collect();
    engine.reload_tenant_policies_named("t", &rows).unwrap();
    engine.reload_tenant_policies_named("t", &rows).unwrap();
    assert_eq!(engine.statements_parsed(), 3);
    let ids: Vec<String> = tenant_set(&engine, "t")
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| !id.starts_with(SYSTEM_PLATFORM_POLICY_ID_PREFIX))
        .collect();
    assert_eq!(
        ids,
        ["t:app/p.cedar#0", "t:app/p.cedar#1", "t:app/p.cedar#2"]
    );
}

#[test]
fn text_that_does_not_cut_into_plain_statements_is_parsed_whole_as_before() {
    let engine = AuthzEngine::empty();
    let template = "permit(principal == ?principal, action, resource);\npermit(principal, action == Action::\"read\", resource is Doc);";
    engine.reload_tenant_policies("t", template).unwrap();
    assert!(reads(&engine, "any"));
    assert_eq!(
        engine.tenant_policies.read().unwrap()["t"]
            .policies
            .policy_set
            .templates()
            .count(),
        1
    );
    let error = engine
        .reload_tenant_policies("t", "permit(principal, action, resource")
        .unwrap_err();
    assert!(matches!(error, AuthzError::PolicyParse(_)));
    assert!(
        reads(&engine, "any"),
        "a failed reload keeps the previous set"
    );
}
