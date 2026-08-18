//! Cross-language conformance: apcore Module -> OpenAI function definition.
//!
//! Drives the OpenAI converter from the shared fixture at
//! `apcore-mcp/conformance/fixtures/openai_tool_mapping.json`. The Python and
//! TypeScript bridges run the same fixture through their own converters; all
//! three must agree on the function name and on the schema rewrites the strict
//! conversion performs.
//!
//! Unlike those two, the Rust converter has no `convert_descriptor` — only
//! `convert_registry*` — so each case registers its module in a fresh registry
//! and converts that. The fixture's `known_gaps#rust_driver_missing` recorded
//! this shape difference as the reason the driver was absent; it is now
//! present, and the gap entry can go.

mod common;

use apcore::registry::{ModuleDescriptor, Registry};
use apcore::ModuleAnnotations;
use apcore_mcp::converters::openai::{ConvertOptions, OpenAIConverter};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    test_cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    #[serde(default)]
    options: Options,
    input_module: FixtureModule,
    expected_function_name: String,
    #[serde(default)]
    expected_property_description: Option<PropertyDescription>,
    #[serde(default)]
    expected_absent_property_keys: Option<Vec<String>>,
    #[serde(default)]
    expected_property_of: Option<String>,
    #[serde(default)]
    expected_description_contains: Option<Vec<String>>,
    #[serde(default)]
    expected_description_not_contains: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct Options {
    #[serde(default)]
    embed_annotations: bool,
    #[serde(default = "default_strict")]
    strict: bool,
}

/// Matches the converter default, so a fixture case that says nothing about
/// `strict` gets the shipped behaviour rather than `false`.
fn default_strict() -> bool {
    true
}

#[derive(Deserialize)]
struct PropertyDescription {
    property: String,
    value: String,
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

/// Minimal module body: conversion reads the descriptor, never executes.
struct StubModule;

#[async_trait::async_trait]
impl apcore::Module for StubModule {
    fn description(&self) -> &str {
        "conformance stub"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }

    fn output_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }

    async fn execute(
        &self,
        _inputs: Value,
        _ctx: &apcore::Context<Value>,
    ) -> Result<Value, apcore::errors::ModuleError> {
        panic!("conversion must not execute the module")
    }
}

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
fn conformance_module_to_openai_function() {
    let Some(fixture) = common::load_fixture::<Fixture>("openai_tool_mapping.json") else {
        return;
    };

    for case in &fixture.test_cases {
        let registry = Registry::default();
        registry
            .register(
                &case.input_module.module_id,
                Box::new(StubModule),
                descriptor(&case.input_module),
            )
            .unwrap_or_else(|e| panic!("case {}: register failed: {e}", case.id));

        let options = ConvertOptions {
            embed_annotations: case.options.embed_annotations,
            strict: case.options.strict,
            rich_description: false,
        };
        let tools = OpenAIConverter::new()
            .convert_registry_with_options(&registry, options, None, None)
            .unwrap_or_else(|e| panic!("case {}: convert failed: {e}", case.id));

        assert_eq!(tools.len(), 1, "case {}: expected one tool", case.id);
        let function = &tools[0]["function"];

        assert_eq!(
            function["name"], case.expected_function_name,
            "case {}: function name",
            case.id
        );

        let properties = &function["parameters"]["properties"];

        if let Some(ref spec) = case.expected_property_description {
            assert_eq!(
                properties[&spec.property]["description"], spec.value,
                "case {}: {}.description",
                case.id, spec.property
            );
        }

        if let Some(ref forbidden_keys) = case.expected_absent_property_keys {
            let scoped: Vec<(&String, &Value)> = match case.expected_property_of {
                Some(ref only) => vec![(only, &properties[only])],
                None => properties
                    .as_object()
                    .map(|map| map.iter().collect())
                    .unwrap_or_default(),
            };
            for (name, schema) in scoped {
                for forbidden in forbidden_keys {
                    assert!(
                        schema.get(forbidden).is_none(),
                        "case {}: property {name} still carries {forbidden}",
                        case.id
                    );
                }
            }
        }

        let description = function["description"].as_str().unwrap_or_default();
        for needle in case.expected_description_contains.iter().flatten() {
            assert!(
                description.contains(needle.as_str()),
                "case {}: description missing {needle:?} — got {description:?}",
                case.id
            );
        }
        for needle in case.expected_description_not_contains.iter().flatten() {
            assert!(
                !description.contains(needle.as_str()),
                "case {}: description leaked {needle:?} — got {description:?}",
                case.id
            );
        }
    }
}
