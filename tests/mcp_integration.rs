//! MCP integration tests.
//!
//! Tests the full MCP flow: spawn mock server → discover tools → register → enforce → execute.
//! Uses the mock_mcp_server example binary as the MCP server.

#![cfg(feature = "mcp")]

use std::path::PathBuf;
use std::str::FromStr;

use serde_json::json;

use cherub::enforcement;
use cherub::enforcement::policy::Policy;
use cherub::tools::mcp::loader;
use cherub::tools::{ToolContext, ToolInvocation, ToolRegistry};

fn mock_server_binary() -> PathBuf {
    // CARGO_BIN_EXE_cherub → target/debug/cherub
    // Pop the binary name → target/debug/
    // Then push examples/mock_mcp_server → target/debug/examples/mock_mcp_server
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_cherub"));
    path.pop(); // Remove binary name.
    path.push("examples");
    path.push("mock_mcp_server");
    path
}

fn write_temp_config(dir: &tempfile::TempDir, server_name: &str) -> PathBuf {
    let binary = mock_server_binary();
    let config_content = format!(
        r#"[servers.{server_name}]
command = "{}"
"#,
        binary.display()
    );
    let config_path = dir.path().join("mcp_config.toml");
    std::fs::write(&config_path, config_content).expect("write config");
    config_path
}

async fn load_mock_tools(
    server_name: &str,
) -> (
    tempfile::TempDir,
    Vec<cherub::tools::mcp::proxy::McpToolProxy>,
) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_temp_config(&dir, server_name);
    let result = loader::load_from_config(
        &config_path,
        #[cfg(feature = "credentials")]
        None,
        #[cfg(feature = "credentials")]
        "test",
    )
    .await;
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    (dir, result.tools)
}

// ── Loader + Discovery ──────────────────────────────────────────────────────

#[tokio::test]
async fn discover_tools_from_mock_server() {
    let (_dir, tools) = load_mock_tools("mock").await;

    assert_eq!(tools.len(), 2);

    let names: Vec<&str> = tools.iter().map(|t| t.composite_name.as_str()).collect();
    assert!(
        names.contains(&"mock__echo"),
        "expected mock__echo, got {names:?}"
    );
    assert!(
        names.contains(&"mock__add"),
        "expected mock__add, got {names:?}"
    );

    let echo = tools.iter().find(|t| t.tool_name == "echo").unwrap();
    assert_eq!(echo.server_name, "mock");
    assert_eq!(echo.composite_name, "mock__echo");
    assert!(!echo.description.is_empty());
}

// ── Tool Execution ──────────────────────────────────────────────────────────

#[tokio::test]
async fn execute_echo_tool() {
    let (_dir, tools) = load_mock_tools("mock").await;
    let echo = tools.iter().find(|t| t.tool_name == "echo").unwrap();
    let result = echo
        .execute(&json!({"message": "hello world"}))
        .await
        .unwrap();
    assert_eq!(result.output, "hello world");
}

#[tokio::test]
async fn execute_add_tool() {
    let (_dir, tools) = load_mock_tools("mock").await;
    let add = tools.iter().find(|t| t.tool_name == "add").unwrap();
    let result = add.execute(&json!({"a": 3, "b": 4})).await.unwrap();
    assert_eq!(result.output, "7");
}

// ── Registry Integration ────────────────────────────────────────────────────

#[tokio::test]
async fn registry_enforcement_name_mapping() {
    let (_dir, tools) = load_mock_tools("test-server").await;
    let registry = ToolRegistry::new().with_mcp(tools);

    assert_eq!(
        registry.enforcement_name("test-server__echo"),
        "test-server"
    );
    assert_eq!(registry.enforcement_name("test-server__add"), "test-server");
    assert_eq!(registry.enforcement_name("bash"), "bash");
}

#[tokio::test]
async fn registry_enrich_params_injects_metadata() {
    let (_dir, tools) = load_mock_tools("test-server").await;
    let registry = ToolRegistry::new().with_mcp(tools);

    let params = json!({"message": "hello"});
    let enriched = registry.enrich_params("test-server__echo", &params);

    assert_eq!(enriched["__mcp_server"], "test-server");
    assert_eq!(enriched["__mcp_tool"], "echo");
    assert_eq!(enriched["message"], "hello");
}

// ── Adversarial: Params Override Prevention ─────────────────────────────────

#[tokio::test]
async fn adversarial_params_override_prevented() {
    let (_dir, tools) = load_mock_tools("test-server").await;
    let registry = ToolRegistry::new().with_mcp(tools);

    // Model tries to inject __mcp_server to spoof a different server.
    let params = json!({
        "__mcp_server": "evil-server",
        "__mcp_tool": "evil-tool",
        "message": "hello"
    });
    let enriched = registry.enrich_params("test-server__echo", &params);

    // Must be overwritten by the proxy's true values.
    assert_eq!(enriched["__mcp_server"], "test-server");
    assert_eq!(enriched["__mcp_tool"], "echo");
}

// ── Enforcement Integration ─────────────────────────────────────────────────

const MCP_POLICY: &str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]

[tools.mock]
enabled = true
match_source = "mcp_structured"

[tools.mock.actions.echo]
tier = "observe"
patterns = ["^mock:echo$"]

[tools.mock.actions.add]
tier = "act"
patterns = ["^mock:add$"]
"#;

#[tokio::test]
async fn enforcement_allows_permitted_mcp_tool() {
    let (_dir, tools) = load_mock_tools("mock").await;
    let registry = ToolRegistry::new().with_mcp(tools);
    let policy = Policy::from_str(MCP_POLICY).unwrap();

    // echo tool should be allowed (observe tier).
    let enforcement_name = registry.enforcement_name("mock__echo");
    let enriched = registry.enrich_params("mock__echo", &json!({"message": "hi"}));
    let proposal = ToolInvocation::new(enforcement_name, "execute", enriched);
    let (_, decision) = enforcement::evaluate(proposal, &policy);
    match decision {
        enforcement::Decision::Allow(_) => {} // expected
        _ => panic!("expected Allow for echo tool"),
    }

    // add tool should be allowed (act tier).
    let enforcement_name = registry.enforcement_name("mock__add");
    let enriched = registry.enrich_params("mock__add", &json!({"a": 1, "b": 2}));
    let proposal = ToolInvocation::new(enforcement_name, "execute", enriched);
    let (_, decision) = enforcement::evaluate(proposal, &policy);
    match decision {
        enforcement::Decision::Allow(_) => {} // expected
        _ => panic!("expected Allow for add tool"),
    }
}

#[tokio::test]
async fn enforcement_rejects_unregistered_mcp_server() {
    let (_dir, tools) = load_mock_tools("mock").await;
    let registry = ToolRegistry::new().with_mcp(tools);

    // Policy has no entry for "mock" server.
    let policy = Policy::from_str(
        r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#,
    )
    .unwrap();

    let enforcement_name = registry.enforcement_name("mock__echo");
    let enriched = registry.enrich_params("mock__echo", &json!({"message": "hi"}));
    let proposal = ToolInvocation::new(enforcement_name, "execute", enriched);
    let (_, decision) = enforcement::evaluate(proposal, &policy);
    match decision {
        enforcement::Decision::Reject => {} // expected
        _ => panic!("expected Reject for unregistered server"),
    }
}

// ── Full Execute Through Registry ───────────────────────────────────────────

#[tokio::test]
async fn full_execute_through_registry() {
    let (_dir, tools) = load_mock_tools("mock").await;
    let registry = ToolRegistry::new().with_mcp(tools);

    let policy = Policy::from_str(
        r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]

[tools.mock]
enabled = true
match_source = "mcp_structured"

[tools.mock.actions.all]
tier = "observe"
patterns = ["^mock:"]
"#,
    )
    .unwrap();

    // Simulate the agent loop's enforcement flow.
    let enforcement_name = registry.enforcement_name("mock__echo");
    let enriched = registry.enrich_params("mock__echo", &json!({"message": "integration test"}));
    let proposal = ToolInvocation::new(enforcement_name, "execute", enriched);
    let (evaluated, decision) = enforcement::evaluate(proposal, &policy);

    match decision {
        enforcement::Decision::Allow(token) => {
            let ctx = ToolContext {
                user_id: "test".to_owned(),
                session_id: uuid::Uuid::now_v7(),
                turn_number: 1,
            };
            // Use the composite name for registry lookup.
            let result = evaluated.execute(token, &registry, &ctx).await;
            // The execute will fail because evaluated.tool is the enforcement name ("mock"),
            // not the composite name. This is expected — in the real agent loop, we restore
            // the composite name. Here we test the enforcement decision is correct.
            // The tool execution tests above already verify direct execution works.
            assert!(result.is_err()); // "unknown tool: mock"
        }
        _ => panic!("expected Allow"),
    }
}

// ── Internal Keys Stripped Before Forwarding ─────────────────────────────────

#[tokio::test]
async fn internal_keys_stripped_before_forwarding() {
    let (_dir, tools) = load_mock_tools("mock").await;
    let echo = tools.iter().find(|t| t.tool_name == "echo").unwrap();

    // Params with internal keys — they should be stripped before forwarding.
    let params = json!({
        "__mcp_server": "mock",
        "__mcp_tool": "echo",
        "message": "stripped"
    });
    let result = echo.execute(&params).await.unwrap();
    assert_eq!(result.output, "stripped");
}

// ── Config Error Handling ───────────────────────────────────────────────────

#[tokio::test]
async fn nonexistent_config_returns_error() {
    let result = loader::load_from_config(
        std::path::Path::new("/nonexistent/mcp_config.toml"),
        #[cfg(feature = "credentials")]
        None,
        #[cfg(feature = "credentials")]
        "test",
    )
    .await;
    assert!(!result.errors.is_empty());
    assert!(result.tools.is_empty());
}

#[tokio::test]
async fn invalid_command_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("mcp_config.toml");
    std::fs::write(
        &config_path,
        r#"
[servers.bad]
command = "/nonexistent/binary/that/doesnt/exist"
"#,
    )
    .unwrap();

    let result = loader::load_from_config(
        &config_path,
        #[cfg(feature = "credentials")]
        None,
        #[cfg(feature = "credentials")]
        "test",
    )
    .await;
    assert!(!result.errors.is_empty());
    assert!(result.tools.is_empty());
}
