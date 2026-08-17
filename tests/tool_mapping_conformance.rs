//! Cross-language conformance: apcore Module -> MCP Tool.
//!
//! Drives `MCPServerFactory::build_tool` from the shared fixture at
//! `apcore-mcp/conformance/fixtures/tool_mapping.json`. The Python and
//! TypeScript bridges run the same fixture through their own factories; all
//! three must agree on the tool name, input schema and annotation hints.
//!
//! The fixture pins SRS section 7.1: the MCP tool name keeps the module id's
//! dot notation. Hyphenation, `x-llm-description` promotion and `x-` stripping
//! are the OpenAI converter's job, not this one's.

mod common;

use apcore::registry::ModuleDescriptor;
use apcore::ModuleAnnotations;
use apcore_mcp::server::factory::MCPServerFactory;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    test_cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    input_module: FixtureModule,
    expected_mcp_tool: ExpectedTool,
}

#[derive(Deserialize)]
struct FixtureModule {
    module_id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    input_schema: Value,
    #[serde(default)]
    annotations: Option<Value>,
}

#[derive(Deserialize)]
struct ExpectedTool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
    annotations: Value,
}

/// Build a descriptor from the fixture's `input_module`.
///
/// Annotations come in as the apcore wire shape, so they round-trip through
/// `ModuleAnnotations`' own deserializer rather than being mapped by hand —
/// a hand-map would silently drift from apcore's defaults.
fn descriptor(module: &FixtureModule) -> ModuleDescriptor {
    let annotations: Option<ModuleAnnotations> = module
        .annotations
        .as_ref()
        .map(|value| serde_json::from_value(value.clone()).expect("fixture annotations"));

    ModuleDescriptor {
        module_id: module.module_id.clone(),
        name: None,
        description: module.description.clone(),
        documentation: None,
        input_schema: module.input_schema.clone(),
        output_schema: serde_json::json!({}),
        version: "1.0.0".to_string(),
        tags: vec![],
        annotations,
        examples: vec![],
        metadata: std::collections::HashMap::new(),
        display: None,
        sunset_date: None,
        dependencies: vec![],
        enabled: true,
    }
}

#[test]
fn conformance_module_to_mcp_tool() {
    let Some(fixture) = common::load_fixture::<Fixture>("tool_mapping.json") else {
        return;
    };

    let factory = MCPServerFactory::new();
    for case in &fixture.test_cases {
        let d = descriptor(&case.input_module);
        let tool = factory
            .build_tool(&d, &case.input_module.description, None)
            .unwrap_or_else(|e| panic!("case {}: build_tool failed: {e}", case.id));

        assert_eq!(
            tool.name, case.expected_mcp_tool.name,
            "case {}: tool name",
            case.id
        );
        assert_eq!(
            tool.description, case.expected_mcp_tool.description,
            "case {}: description",
            case.id
        );
        assert_eq!(
            tool.input_schema, case.expected_mcp_tool.input_schema,
            "case {}: inputSchema",
            case.id
        );

        let got = serde_json::to_value(&tool.annotations).expect("annotations serialize");
        let expected = case
            .expected_mcp_tool
            .annotations
            .as_object()
            .expect("fixture annotations object");
        for (key, value) in expected {
            assert_eq!(
                got.get(key),
                Some(value),
                "case {}: annotation {key}",
                case.id
            );
        }
    }
}
