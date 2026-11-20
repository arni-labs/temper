//! Governance with real credentials: an agent Cedar denies, a pending
//! decision, and a human who approves it with their own key.

use super::*;

/// Key of an agent with no permits: Cedar denies every action it attempts.
const TEST_AGENT_KEY: &str = "temper-mcp-test-agent-key";
const TEST_AGENT_ID: &str = "checkout-bot";

/// Build a RuntimeContext signed in as the test agent, which holds no permits.
fn agent_ctx_for_port(port: u16) -> RuntimeContext {
    RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        temper_url: None,
        agent_id: None,
        agent_type: None,
        session_id: Some("temper-mcp-test-agent-session".to_string()),
        api_key: Some(TEST_AGENT_KEY.to_string()),
    })
    .expect("ctx")
}

/// Issue the test agent's credential: an Active agent type and a credential
/// bound to [`TEST_AGENT_KEY`], the same records an operator issues for any
/// agent. No policy names the agent, so Cedar denies what it attempts.
pub(super) async fn issue_test_agent_credential(state: &PlatformState) {
    use temper_server::identity::hash_token;
    use temper_server::request_context::AgentContext;

    let tenant = TenantId::new(TEST_TENANT);
    let issuer = AgentContext::for_service("temper-mcp-test");
    let defined = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "AgentType",
            "test-agent-type",
            "Define",
            json!({
                "name": "claude-code",
                "system_prompt": "MCP test agent",
                "tool_set": "local",
                "model": "none",
                "max_turns": "0",
                "adapter_config": "{}",
                "default_budget_cents": "0"
            }),
            &issuer,
        )
        .await
        .expect("define test agent type");
    assert!(
        defined.success,
        "define test agent type: {:?}",
        defined.error
    );

    let key_hash = hash_token(TEST_AGENT_KEY);
    let issued = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "AgentCredential",
            &key_hash,
            "Issue",
            json!({
                "agent_type_id": "test-agent-type",
                "agent_instance_id": TEST_AGENT_ID,
                "key_hash": key_hash,
                "key_prefix": TEST_AGENT_KEY.chars().take(8).collect::<String>(),
                "description": "MCP test agent credential",
                "created_by": "temper-mcp-test",
                "expires_at": ""
            }),
            &issuer,
        )
        .await
        .expect("issue test agent credential");
    assert!(
        issued.success,
        "issue test agent credential: {:?}",
        issued.error
    );
}

/// End-to-end governance flow: agent denied -> get decision -> human approves
/// directly (not via agent sandbox) -> agent retries -> success.
#[tokio::test]
async fn e2e_agent_denial_human_approve_retry() {
    let (port, shutdown) = start_test_temper_server().await;

    // Step 0: The operator creates the order; the agent may not.
    let mut operator = authenticated_ctx_for_port(port);
    let response = rpc(
        &mut operator,
        call_tool_request(
            20,
            "execute",
            "return await temper.create('demo', 'Orders', {'id': 'agent-e2e-1', 'customer': 'Alice'})",
        ),
    )
    .await;
    let (_, is_error) = tool_text(&response);
    assert!(!is_error, "operator create should succeed: {response:#}");

    // The agent signs in with its own key; no policy permits it anything.
    let mut ctx = agent_ctx_for_port(port);

    // Step 1: Agent tries a bound action — should be denied (403).
    let response = rpc(
        &mut ctx,
        call_tool_request(
            21,
            "execute",
            "return await temper.action('demo', 'Orders', 'agent-e2e-1', 'CancelOrder', {'Reason': 'test'})",
        ),
    )
    .await;
    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "agent action should return structured denial (not error): {response:#}"
    );
    assert!(
        text.contains("authorization_denied"),
        "should get structured authorization_denied status, got: {text}"
    );
    assert!(
        text.contains("poll_decision"),
        "denial response should mention poll_decision, got: {text}"
    );

    // Step 2: Agent lists ALL decisions (no status filter) to debug.
    let response = rpc(
        &mut ctx,
        call_tool_request(22, "execute", "return await temper.get_decisions('demo')"),
    )
    .await;
    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "get_decisions should succeed: {response:#}");
    let decisions: Value = serde_json::from_str(text).expect("json");
    let decisions_arr = decisions
        .get("decisions")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("decisions should have 'decisions' array, got: {text}"));
    assert!(
        !decisions_arr.is_empty(),
        "should have at least one pending decision"
    );
    let decision_id = decisions_arr[0]["id"]
        .as_str()
        .expect("decision should have id");
    assert!(
        decision_id.starts_with("PD-"),
        "decision ID should start with PD-: {decision_id}"
    );

    // Step 2b: Verify agent cannot self-approve.
    let approve_code = format!(
        "return await temper.approve_decision('demo', '{}', 'broad')",
        decision_id
    );
    let response = rpc(&mut ctx, call_tool_request(22, "execute", &approve_code)).await;
    let (text, is_error) = tool_text(&response);
    assert!(is_error, "approve_decision should be blocked: {response:#}");
    assert!(
        text.contains("not available to agents"),
        "should get governance write blocked message, got: {text}"
    );

    // Step 3: A human approves directly via HTTP (as Observe UI or `temper
    // decide` would), with the operator's key, which may manage policies.
    let http = reqwest::Client::new();
    let approve_url =
        format!("http://127.0.0.1:{port}/api/tenants/demo/decisions/{decision_id}/approve");
    let approve_resp = http
        .post(&approve_url)
        .bearer_auth(TEST_OPERATOR_KEY)
        .header("X-Tenant-Id", TEST_TENANT)
        .json(&json!({
            "scope": {
                "principal": "any_agent",
                "action": "all_actions",
                "resource": "any_resource",
                "duration": "always"
            },
            "decided_by": "human-test"
        }))
        .send()
        .await
        .expect("approve request");
    assert!(
        approve_resp.status().is_success(),
        "human approval should succeed: {:?}",
        approve_resp.status()
    );

    // Step 4: Retry the action — should now succeed (Cedar policy was hot-loaded).
    let response = rpc(
        &mut ctx,
        call_tool_request(
            24,
            "execute",
            r#"
result = await temper.action('demo', 'Orders', 'agent-e2e-1', 'CancelOrder', {'Reason': 'approved'})
return result['status']
"#,
        ),
    )
    .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "retry action after human approval should succeed: {response:#}"
    );
    let parsed: Value = serde_json::from_str(text).expect("json");
    assert_eq!(parsed, Value::String("Cancelled".to_string()));
}

#[tokio::test]
async fn get_decision_status_returns_decision() {
    let (port, shutdown) = start_test_temper_server().await;

    // The operator creates the order; the agent may not.
    let mut operator = authenticated_ctx_for_port(port);
    let response = rpc(
        &mut operator,
        call_tool_request(
            60,
            "execute",
            "return await temper.create('demo', 'Orders', {'id': 'status-test-1', 'customer': 'Bob'})",
        ),
    )
    .await;
    let (_, is_error) = tool_text(&response);
    assert!(!is_error, "create should succeed: {response:#}");

    // The agent tries an action with its own key -- Cedar denies it.
    let mut ctx = agent_ctx_for_port(port);
    let response = rpc(
        &mut ctx,
        call_tool_request(
            61,
            "execute",
            "return await temper.action('demo', 'Orders', 'status-test-1', 'CancelOrder', {'Reason': 'test'})",
        ),
    )
    .await;
    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "should get structured denial: {response:#}");
    let denial: Value = serde_json::from_str(text).expect("json");
    let decision_id = denial["pending_decision"]
        .as_str()
        .or_else(|| denial["decision_id"].as_str())
        .expect("should have decision ID");

    // Use get_decision_status to check.
    let status_code = format!(
        "return await temper.get_decision_status('demo', '{}')",
        decision_id
    );
    let response = rpc(&mut ctx, call_tool_request(62, "execute", &status_code)).await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "get_decision_status should succeed: {response:#}"
    );
    let status_result: Value = serde_json::from_str(text).expect("json");
    assert_eq!(
        status_result["decision_id"].as_str().unwrap(), // ci-ok: test assertion
        decision_id,
        "should return the correct decision ID"
    );
    assert_eq!(
        status_result["status"].as_str().unwrap(), // ci-ok: test assertion
        "pending",
        "status should be pending"
    );
}
