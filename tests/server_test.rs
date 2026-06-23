//! Integration tests for the MCP server module.

mod common;

use apcore::module::ModuleAnnotations;
use apcore::registry::ModuleDescriptor;
use apcore_mcp::server::factory::MCPServerFactory;
use apcore_mcp::server::server::{MCPServer, MCPServerConfig};
use serde_json::{json, Value};
use std::collections::HashMap;

// ---- MCPServer construction -------------------------------------------------

#[test]
fn mcp_server_constructs_with_default_config() {
    let server = MCPServer::new(MCPServerConfig {
        name: "test-server".to_string(),
        ..Default::default()
    });
    assert!(!server.has_tool_handlers());
    assert!(!server.has_resource_handlers());
}

// ---- MCPServerFactory::build_tool ------------------------------------------

fn make_descriptor(module_id: &str) -> ModuleDescriptor {
    ModuleDescriptor {
        module_id: module_id.to_string(),
        name: None,
        description: "test module".to_string(),
        documentation: None,
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            }
        }),
        output_schema: json!({"type": "object"}),
        version: "1.0.0".to_string(),
        tags: vec![],
        annotations: Some(ModuleAnnotations::default()),
        examples: vec![],
        metadata: HashMap::new(),
        display: None,
        sunset_date: None,
        dependencies: vec![],
        enabled: true,
    }
}

#[test]
fn factory_builds_tool_from_descriptor() {
    let factory = MCPServerFactory::new();
    let desc = make_descriptor("math.add");
    let tool = factory.build_tool(&desc, "Add two numbers", None).unwrap();
    assert_eq!(tool.name, "math.add");
    assert_eq!(tool.description, "Add two numbers");
    assert_eq!(tool.input_schema["type"], "object");
}

#[test]
fn factory_rejects_reserved_module_id() {
    let factory = MCPServerFactory::new();
    let desc = make_descriptor("__apcore_task_submit");
    let result = factory.build_tool(&desc, "reserved", None);
    assert!(
        result.is_err(),
        "__apcore_* module IDs must be rejected by factory"
    );
}

#[test]
fn factory_build_tool_with_name_override() {
    let factory = MCPServerFactory::new();
    let desc = make_descriptor("internal.id");
    let tool = factory
        .build_tool(&desc, "description", Some("public_name"))
        .unwrap();
    assert_eq!(tool.name, "public_name");
}

#[test]
fn factory_build_tool_with_registry_uses_schema_converter() {
    // [D11-011] build_tool_with_registry uses local SchemaConverter.
    let factory = MCPServerFactory::new();
    let desc = make_descriptor("test.module");
    let tool = factory
        .build_tool_with_registry(&desc, "description", None, None)
        .unwrap();
    assert_eq!(tool.input_schema["type"], "object");
    assert!(tool.input_schema.get("properties").is_some());
}

// ---- MCPServerFactory::create_server name validation [D10-002] -------------

#[test]
fn factory_create_server_rejects_empty_name() {
    // [D10-002] Spec mandates `name` must be a non-empty string of at
    // most 255 chars. An empty name must return InvalidName(0).
    use apcore_mcp::server::server::FactoryError;
    let factory = MCPServerFactory::new();
    let result = factory.create_server("", "1.0.0");
    match result {
        Ok(_) => panic!("empty name must be rejected"),
        Err(FactoryError::InvalidName(0)) => {}
        Err(other) => panic!("expected InvalidName(0), got {other:?}"),
    }
}

#[test]
fn factory_create_server_rejects_name_over_255_chars() {
    // [D10-002] Spec mandates `name` must be a non-empty string of at
    // most 255 chars. A 256-char name must return InvalidName(256).
    use apcore_mcp::server::server::FactoryError;
    let factory = MCPServerFactory::new();
    let long = "x".repeat(256);
    let result = factory.create_server(&long, "1.0.0");
    match result {
        Ok(_) => panic!("256-char name must be rejected"),
        Err(FactoryError::InvalidName(256)) => {}
        Err(other) => panic!("expected InvalidName(256), got {other:?}"),
    }
}

#[test]
fn factory_create_server_accepts_valid_name_at_boundary() {
    // 255 chars is the upper bound and must be accepted.
    let factory = MCPServerFactory::new();
    let name = "y".repeat(255);
    assert!(
        factory.create_server(&name, "1.0.0").is_ok(),
        "255-char name should be accepted"
    );
}

// ---- H-3: async_serve mounts /mcp ------------------------------------------

use apcore::config::Config;
use apcore::context::Context;
use apcore::executor::Executor;
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_mcp::{async_serve, AsyncServeConfig};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

#[derive(Debug)]
struct AddModule;

#[async_trait]
impl Module for AddModule {
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}})
    }
    fn output_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn description(&self) -> &str {
        "Add two numbers"
    }
    async fn execute(
        &self,
        inputs: serde_json::Value,
        _ctx: &Context<serde_json::Value>,
    ) -> Result<serde_json::Value, apcore::errors::ModuleError> {
        let a = inputs.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let b = inputs.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
        Ok(json!({"sum": a + b}))
    }
}

fn build_executor_with_add() -> Arc<Executor> {
    let registry = Registry::new();
    let descriptor = ModuleDescriptor {
        module_id: "math.add".to_string(),
        name: None,
        description: "Add two numbers".to_string(),
        documentation: None,
        input_schema: json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}),
        output_schema: json!({"type": "object"}),
        version: "1.0.0".to_string(),
        tags: vec![],
        annotations: Some(ModuleAnnotations::default()),
        examples: vec![],
        metadata: HashMap::new(),
        display: None,
        sunset_date: None,
        dependencies: vec![],
        enabled: true,
    };
    registry
        .register("math.add", Box::new(AddModule), descriptor)
        .expect("register math.add");
    Arc::new(Executor::new(registry, Config::default()))
}

#[tokio::test]
async fn async_serve_router_serves_mcp_tools_list() {
    // [H-3] The Router returned by async_serve must mount `/mcp` and serve
    // real MCP traffic (tools/list), not just /health, /metrics, /usage.
    let executor = build_executor_with_add();
    let app = async_serve(
        executor,
        AsyncServeConfig {
            name: "h3-test".to_string(),
            require_auth: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect("async_serve should build a router");

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/mcp POST must return 200"
    );

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let resp: Value = serde_json::from_slice(&bytes).unwrap();
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"math.add"),
        "math.add must be listed; got {names:?}"
    );
}

#[tokio::test]
async fn async_serve_router_still_serves_health() {
    // [H-3] Regression: the /health endpoint must remain available after
    // switching from health_metrics_router() to build_streamable_http_app().
    let executor = build_executor_with_add();
    let app = async_serve(
        executor,
        AsyncServeConfig {
            name: "h3-health".to_string(),
            require_auth: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect("async_serve should build a router");

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "/health must return 200");
}
