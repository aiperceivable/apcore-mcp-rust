//! Integration tests for the converters module.

mod common;

use apcore_mcp::{OpenAIConverter, SchemaConverter};
use serde_json::json;

// ---- SchemaConverter roundtrip tests ---------------------------------------

#[test]
fn schema_converter_empty_schema_becomes_object() {
    let schema = json!({});
    let result = SchemaConverter::convert_input_schema_strict(&schema, false).unwrap();
    assert_eq!(result, json!({"type": "object", "properties": {}}));
}

#[test]
fn schema_converter_null_schema_becomes_empty_object() {
    let result =
        SchemaConverter::convert_input_schema_strict(&serde_json::Value::Null, false).unwrap();
    assert_eq!(result, json!({"type": "object", "properties": {}}));
}

#[test]
fn schema_converter_strict_injects_additional_properties_false() {
    let schema = json!({
        "type": "object",
        "properties": {
            "x": {"type": "string"}
        }
    });
    let result = SchemaConverter::convert_input_schema(&schema).unwrap();
    assert_eq!(result["additionalProperties"], json!(false));
}

#[test]
fn schema_converter_non_strict_does_not_inject_additional_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "x": {"type": "string"}
        }
    });
    let result = SchemaConverter::convert_input_schema_strict(&schema, false).unwrap();
    assert!(
        result.get("additionalProperties").is_none(),
        "non-strict mode must not inject additionalProperties"
    );
}

// ---- OpenAIConverter::convert_descriptor tests -----------------------------

#[test]
fn openai_converter_normalizes_dot_to_dash() {
    let converter = OpenAIConverter::new();
    let descriptor = json!({"input_schema": {}});
    let result = converter
        .convert_descriptor("math.add", &descriptor, "Add numbers", false, false)
        .unwrap();
    assert_eq!(result["function"]["name"], "math-add");
}

#[test]
fn openai_converter_strict_mode_adds_strict_flag() {
    let converter = OpenAIConverter::new();
    let descriptor = json!({
        "input_schema": {
            "type": "object",
            "properties": {
                "x": {"type": "number"}
            }
        }
    });
    let result = converter
        .convert_descriptor("math.add", &descriptor, "desc", false, true)
        .unwrap();
    assert_eq!(result["function"]["strict"], true);
    assert_eq!(
        result["function"]["parameters"]["additionalProperties"],
        false
    );
}

#[test]
fn openai_converter_convert_registry_empty_returns_empty() {
    let converter = OpenAIConverter::new();
    let registry = json!({});
    let result = converter
        .convert_registry_json(&registry, false, false, None, None)
        .unwrap();
    assert!(result.is_empty());
}

#[test]
fn openai_converter_convert_registry_single_module() {
    let converter = OpenAIConverter::new();
    let registry = json!({
        "text.upper": {
            "description": "Uppercase text",
            "input_schema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"]
            }
        }
    });
    let result = converter
        .convert_registry_json(&registry, false, false, None, None)
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["function"]["name"], "text-upper");
    assert_eq!(result[0]["function"]["description"], "Uppercase text");
    assert_eq!(result[0]["function"]["parameters"]["type"], "object");
}

// ---- OC-5 regression: convert_registry must accept a live Registry ----------
// (CHANGELOG 0.14.0 — canonical Rust entrypoint takes &Registry, not &Value)

#[test]
fn openai_converter_convert_registry_takes_live_registry() {
    use apcore::registry::registry::Registry;
    use std::sync::Arc;

    // convert_registry must accept &Registry (canonical after OC-5 rename).
    // If this fails to compile, convert_registry_apcore was not renamed.
    let registry = Arc::new(Registry::default());
    let converter = OpenAIConverter::new();
    let result = converter
        .convert_registry(&registry, false, false, None, None)
        .unwrap();
    assert!(result.is_empty());
}

#[test]
fn openai_converter_convert_registry_json_takes_value() {
    // convert_registry_json must accept &Value (preserved legacy path).
    let registry = json!({});
    let converter = OpenAIConverter::new();
    let result = converter
        .convert_registry_json(&registry, false, false, None, None)
        .unwrap();
    assert!(result.is_empty());
}
