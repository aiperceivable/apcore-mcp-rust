//! Integration tests for the MCP server module.

mod common;

use apcore::module::ModuleAnnotations;
use apcore::registry::ModuleDescriptor;
use apcore_mcp::server::factory::MCPServerFactory;
use apcore_mcp::server::server::{MCPServer, MCPServerConfig};
use serde_json::json;
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
