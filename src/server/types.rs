//! MCP protocol types used by the server factory.
//!
//! These are local Rust structs that mirror the MCP protocol specification.
//! They can later be replaced by types from an official MCP SDK crate.

mod json_precision;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An MCP Tool definition exposed via `list_tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Optional annotation hints for a Tool (MCP spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

impl Default for ToolAnnotations {
    fn default() -> Self {
        Self {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }
}

/// A text content item returned in tool call results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

impl TextContent {
    /// Create a new text content item.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            content_type: "text".to_string(),
            text: text.into(),
        }
    }
}

fn matches_output_schema(value: &Value, schema: &Value) -> bool {
    if !super::schema_references::resolve_all_refs(schema) {
        return false;
    }
    jsonschema::options()
        .with_retriever(super::schema_references::LocalSchemaRetriever)
        .should_validate_formats(false)
        .build(schema)
        .is_ok_and(|validator| validator.is_valid(value))
}

/// Result of a `call_tool` invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<TextContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    pub is_error: bool,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl CallToolResult {
    /// Add structured data from the already-redacted JSON text result.
    /// A mismatch fails closed without exposing result values in diagnostics.
    pub fn with_structured_content(mut self, schema: Option<&Value>) -> Self {
        self.structured_content = None;
        if self.is_error {
            return self;
        }
        let value = self
            .content
            .first()
            .filter(|item| item.content_type == "text")
            .and_then(|item| {
                let value = serde_json::from_str::<Value>(&item.text).ok()?;
                json_precision::preserves_numeric_values(&item.text).then_some(value)
            })
            .filter(Value::is_object);
        if let Some(schema) = schema {
            let valid = value
                .as_ref()
                .is_some_and(|value| matches_output_schema(value, schema));
            if !valid {
                self.meta = None;
                self.is_error = true;
                self.content = vec![TextContent::new(
                    "Output does not match the advertised output schema after redaction",
                )];
                return self;
            }
        }
        self.structured_content = value;
        self
    }
    /// Create a minimal result from content + error flag (no `_meta`).
    pub fn new(content: Vec<TextContent>, is_error: bool) -> Self {
        Self {
            content,
            structured_content: None,
            is_error,
            meta: None,
        }
    }
}

/// An MCP Resource definition exposed via `list_resources`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub mime_type: String,
}

/// Contents returned by `read_resource`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResourceContents {
    pub content: String,
    pub mime_type: String,
}

/// An MCP Resource Template definition exposed via `resources/templates/
/// list`. Describes a parameterized resource URI using RFC 6570 syntax
/// (e.g. `apcore://system.health.module/{module_id}`) for a resource whose
/// read requires an argument that a static [`Resource`] cannot carry
/// (aiperceivable/apcore-mcp#15(a), aiperceivable/apcore-mcp-rust#6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub mime_type: String,
}

/// Initialization options passed to the MCP server on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializationOptions {
    pub server_name: String,
    pub server_version: String,
    pub capabilities: ServerCapabilities,
}

/// Server capabilities advertised during MCP initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    /// Vendor extension capabilities, keyed by extension id (e.g.
    /// `"com.aiperceivable/management"`, aiperceivable/apcore-mcp#16).
    /// Carried as a raw JSON object rather than a strongly-typed field per
    /// extension, since this struct has no MCP SDK to negotiate the shape
    /// with — any extension's payload can be represented. `None` omits the
    /// `extensions` key entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

/// Tools capability — advertises tool-related features.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Whether the server supports `notifications/tools/listChanged`.
    pub list_changed: bool,
}

/// Resources capability — advertises resource-related features.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    /// Whether the server supports `notifications/resources/listChanged`.
    pub list_changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_structured_content_preserves_text_and_checks_the_declared_schema() {
        let text = r#"{"count":1}"#;
        let result = CallToolResult::new(vec![TextContent::new(text)], false)
            .with_structured_content(Some(&json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]})));
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(json!({"count":1})));
        assert_eq!(result.content[0].text, text);
        let result = CallToolResult::new(vec![TextContent::new(text)], false)
            .with_structured_content(Some(&json!({"type":"object","required":["missing"]})));
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        assert!(!result.content[0].text.contains("count"));
    }

    #[test]
    fn test_advertised_schema_requires_a_primary_json_object_result() {
        let schema = json!({"type":"object"});
        for content in [
            vec![],
            vec![TextContent::new("private formatted result")],
            vec![TextContent::new("[1,2]")],
        ] {
            let result = CallToolResult::new(content, false).with_structured_content(Some(&schema));
            assert!(result.is_error);
            assert!(result.structured_content.is_none());
            assert_eq!(result.content.len(), 1);
            assert_eq!(
                result.content[0].text,
                "Output does not match the advertised output schema after redaction"
            );
        }
    }

    #[test]
    fn test_structured_content_preserves_additional_trace_text() {
        let content = vec![
            TextContent::new(r#"{"count":1}"#),
            TextContent::new("trace diagnostics"),
        ];
        let result = CallToolResult::new(content.clone(), false)
            .with_structured_content(Some(&json!({"type":"object"})));
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(json!({"count":1})));
        assert_eq!(result.content.len(), content.len());
        for (actual, expected) in result.content.iter().zip(&content) {
            assert_eq!(actual.text, expected.text);
            assert_eq!(actual.content_type, expected.content_type);
        }
    }

    #[test]
    fn test_output_schema_validates_raw_draft_and_local_references() {
        let draft_seven = json!({"$schema":"http://json-schema.org/draft-07/schema#",
            "type":"object", "properties":{"values":{"type":"array", "items":[{"type":"integer"}], "additionalItems":false}}});
        assert!(matches_output_schema(&json!({"values":[1]}), &draft_seven));
        assert!(!matches_output_schema(
            &json!({"values":[1,2]}),
            &draft_seven
        ));
        let definitions = json!({"type":"object", "$defs":{"count":{"type":"integer","minimum":2}},
            "properties":{"count":{"$ref":"#/$defs/count"}}, "required":["count"]});
        assert!(matches_output_schema(&json!({"count":2}), &definitions));
        assert!(!matches_output_schema(&json!({"count":1}), &definitions));
        let format =
            json!({"type":"object","properties":{"contact":{"type":"string","format":"email"}}});
        assert!(matches_output_schema(
            &json!({"contact":"annotation only"}),
            &format
        ));
    }

    #[test]
    fn test_output_schema_never_fetches_external_http_references() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requested = Arc::new(AtomicBool::new(false));
        let observed = requested.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..40 {
                if let Ok((mut stream, _)) = listener.accept() {
                    observed.store(true, Ordering::SeqCst);
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                        .unwrap();
                    let mut buffer = [0; 1024];
                    let _ = stream.read(&mut buffer);
                    let body = r#"{"type":"integer"}"#;
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes());
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let schema = json!({"type":"object","properties":{"count":{"$ref":format!("http://{address}/schema.json")}}});
        let result = CallToolResult::new(vec![TextContent::new(r#"{"count":1}"#)], false)
            .with_structured_content(Some(&schema));
        server.join().unwrap();
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        assert!(!requested.load(Ordering::SeqCst));
    }

    #[test]
    fn test_output_schema_never_reads_external_file_references() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"type":"integer"}"#).unwrap();
        let schema = json!({"type":"object","properties":{"count":{"$ref":format!("file://{}",file.path().display())}}});
        assert!(!matches_output_schema(&json!({"count":1}), &schema));
    }

    #[test]
    fn test_output_schema_mismatch_discards_only_success_result_metadata() {
        let schema = json!({"type":"object","required":["missing"]});
        let mut result = CallToolResult::new(vec![TextContent::new("{}")], false);
        result.meta = Some(json!({"trace":"diagnostics"}));
        let result = result.with_structured_content(Some(&schema));
        assert!(result.is_error);
        assert!(result.meta.is_none());
        let mut error = CallToolResult::new(vec![TextContent::new("execution error")], true);
        error.meta = Some(json!({"trace":"diagnostics"}));
        assert_eq!(
            error.with_structured_content(Some(&schema)).meta,
            Some(json!({"trace":"diagnostics"}))
        );
    }

    #[test]
    fn test_output_schema_supports_only_common_dialects_and_canonical_aliases() {
        for dialect in [
            "http://json-schema.org/draft-03/schema#",
            "http://json-schema.org/draft-04/schema#",
            "http://json-schema.org/draft-06/schema#",
        ] {
            assert!(!matches_output_schema(
                &json!({}),
                &json!({"$schema":dialect,"type":"object"})
            ));
        }
        for dialect in [
            "http://json-schema.org/draft-07/schema",
            "http://json-schema.org/draft-07/schema#",
            "https://json-schema.org/draft/2019-09/schema",
            "https://json-schema.org/draft/2019-09/schema#",
            "https://json-schema.org/draft/2020-12/schema",
            "https://json-schema.org/draft/2020-12/schema#",
        ] {
            let nested = dialect.trim_end_matches('#');
            let schema = json!({"$schema":dialect,"type":"object","properties":{"child":{"$schema":nested,"type":"object"}}});
            assert!(
                matches_output_schema(&json!({"child":{}}), &schema),
                "{dialect}"
            );
        }
    }

    #[test]
    fn test_output_schema_rejects_mixed_dialects_only_in_schema_positions() {
        for dialect in [
            "http://json-schema.org/draft-07/schema#",
            "https://json-schema.org/draft/2019-09/schema",
        ] {
            let schema =
                json!({"type":"object","$defs":{"unused":{"$id":"child", "$schema":dialect}}});
            let result =
                CallToolResult::new(vec![TextContent::new(r#"{"value":"canary"}"#)], false)
                    .with_structured_content(Some(&schema));
            assert!(result.is_error);
            assert!(result.structured_content.is_none());
            assert!(!serde_json::to_string(&result).unwrap().contains("canary"));
        }
        let schema = json!({"type":"object","$defs":{"unused":{"$schema":"https://json-schema.org/draft/2020-12/schema"}},
            "default":{"$schema":"http://json-schema.org/draft-07/schema#"}});
        assert!(matches_output_schema(&json!({"value":"visible"}), &schema));
    }

    #[test]
    fn test_output_schema_rejects_unresolved_unused_local_references() {
        for reference in ["#/$defs/Missing", "#missing"] {
            let schema = json!({"type":"object","$defs":{"Unused":{"$ref":reference}}});
            let result = CallToolResult::new(vec![TextContent::new("{}")], false)
                .with_structured_content(Some(&schema));
            assert!(result.is_error);
            assert!(result.structured_content.is_none());
        }
    }

    #[test]
    fn test_output_schema_accepts_valid_unused_refs_and_literal_reference_data() {
        let schema = json!({"type":"object","$defs":{
            "Unused":{"$ref":"#/$defs/Existing"},"Existing":{"type":"string"}},
            "properties":{"$ref":{"type":"string"},"public":{"type":"string"}},
            "const":{"$ref":"literal","public":"visible"},
            "default":{"$ref":"https://unavailable.invalid/data"},
            "examples":[{"$ref":"#missing"}]});
        let result = CallToolResult::new(
            vec![TextContent::new(r#"{"$ref":"literal","public":"visible"}"#)],
            false,
        )
        .with_structured_content(Some(&schema));
        assert!(!result.is_error);
        assert_eq!(
            result.structured_content,
            Some(json!({"$ref":"literal","public":"visible"}))
        );
    }

    #[test]
    fn test_structured_content_revalidation_clears_stale_payloads() {
        let result = CallToolResult::new(vec![TextContent::new(r#"{"secret":"redacted"}"#)], false)
            .with_structured_content(None)
            .with_structured_content(Some(&json!({"type":"object","required":["missing"]})));
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        assert!(!result.content[0].text.contains("secret"));
    }

    #[test]
    fn test_structured_content_omits_errors_and_unstructured_values() {
        for text in ["[1,2]", "42", "not JSON"] {
            let result = CallToolResult::new(vec![TextContent::new(text)], false)
                .with_structured_content(None);
            assert!(result.structured_content.is_none());
            assert_eq!(result.content[0].text, text);
        }
        let result = CallToolResult::new(vec![TextContent::new("{\"code\":\"DENIED\"}")], true)
            .with_structured_content(None);
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn test_structured_content_rejects_numeric_value_loss_without_leaking_values() {
        for text in [
            r#"{"count":18446744073709551617}"#,
            r#"{"count":0.10000000000000001}"#,
            r#"{"count":1e-400}"#,
        ] {
            let original = CallToolResult::new(vec![TextContent::new(text)], false);
            let text_only = original.clone().with_structured_content(None);
            assert!(!text_only.is_error);
            assert!(text_only.structured_content.is_none());
            assert_eq!(text_only.content[0].text, text);
            let invalid = original.with_structured_content(Some(&json!({"type":"object"})));
            assert!(invalid.is_error);
            assert!(invalid.structured_content.is_none());
            assert!(!invalid.content[0].text.contains("count"));
        }
    }

    #[test]
    fn test_structured_content_preserves_exact_native_integer_and_decimal_forms() {
        for text in [
            r#"{"count":18446744073709551615}"#,
            r#"{"count":0.1000000,"equivalent":10e-1}"#,
        ] {
            let result = CallToolResult::new(vec![TextContent::new(text)], false)
                .with_structured_content(Some(&json!({"type":"object"})));
            assert!(!result.is_error);
            assert!(result.structured_content.is_some());
            assert_eq!(result.content[0].text, text);
        }
    }

    #[test]
    fn test_structured_content_preserves_native_floating_point_results() {
        for number in [
            -4.0765893351549374e-163,
            1.6176163832269587e274,
            -1.7482232971272084e-9,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
        ] {
            let original = json!({"value": number});
            let text = serde_json::to_string(&original).unwrap();
            let result = CallToolResult::new(vec![TextContent::new(&text)], false)
                .with_structured_content(Some(&json!({"type":"object"})));
            assert!(!result.is_error, "{text}");
            assert_eq!(result.structured_content, Some(original));
            assert_eq!(result.content[0].text, text);
        }
    }

    #[test]
    fn test_tool_serializes_to_mcp_json() {
        let tool = Tool {
            output_schema: None,
            name: "my_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            annotations: Some(ToolAnnotations::default()),
            meta: Some(json!({"requiresApproval": true})),
        };

        let serialized = serde_json::to_value(&tool).unwrap();

        assert_eq!(serialized["name"], "my_tool");
        assert_eq!(serialized["description"], "A test tool");
        assert_eq!(serialized["inputSchema"]["type"], "object");
        assert!(serialized["annotations"].is_object());
        assert_eq!(serialized["_meta"]["requiresApproval"], true);
    }

    #[test]
    fn test_tool_omits_none_fields() {
        let tool = Tool {
            output_schema: None,
            name: "bare".to_string(),
            description: "Minimal".to_string(),
            input_schema: json!({}),
            annotations: None,
            meta: None,
        };

        let serialized = serde_json::to_value(&tool).unwrap();

        assert!(serialized.get("annotations").is_none());
        assert!(serialized.get("_meta").is_none());
    }

    #[test]
    fn test_tool_annotations_defaults() {
        let ann = ToolAnnotations::default();
        let serialized = serde_json::to_value(&ann).unwrap();

        assert_eq!(serialized["readOnlyHint"], false);
        assert_eq!(serialized["destructiveHint"], false);
        assert_eq!(serialized["idempotentHint"], false);
        assert_eq!(serialized["openWorldHint"], true);
        assert!(serialized.get("title").is_none());
    }

    #[test]
    fn test_text_content_type_field() {
        let content = TextContent::new("hello world");
        let serialized = serde_json::to_value(&content).unwrap();

        assert_eq!(serialized["type"], "text");
        assert_eq!(serialized["text"], "hello world");
    }

    #[test]
    fn test_resource_uri_format() {
        let resource = Resource {
            uri: "docs://my_module".to_string(),
            name: "my_module documentation".to_string(),
            mime_type: "text/plain".to_string(),
        };

        let serialized = serde_json::to_value(&resource).unwrap();

        assert_eq!(serialized["uri"], "docs://my_module");
        assert_eq!(serialized["name"], "my_module documentation");
        assert_eq!(serialized["mimeType"], "text/plain");
    }

    #[test]
    fn test_init_options_serialization() {
        let opts = InitializationOptions {
            server_name: "apcore-mcp".to_string(),
            server_version: "0.1.0".to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: true }),
                resources: None,
                extensions: None,
            },
        };

        let serialized = serde_json::to_value(&opts).unwrap();

        assert_eq!(serialized["server_name"], "apcore-mcp");
        assert_eq!(serialized["server_version"], "0.1.0");
        assert_eq!(serialized["capabilities"]["tools"]["listChanged"], true);
    }

    #[test]
    fn test_call_tool_result_serialization() {
        let result = CallToolResult {
            structured_content: None,
            content: vec![TextContent::new("result text")],
            is_error: false,
            meta: None,
        };

        let serialized = serde_json::to_value(&result).unwrap();

        assert_eq!(serialized["isError"], false);
        assert_eq!(serialized["content"][0]["type"], "text");
        assert_eq!(serialized["content"][0]["text"], "result text");
    }

    #[test]
    fn test_read_resource_contents_serialization() {
        let contents = ReadResourceContents {
            content: "# Documentation".to_string(),
            mime_type: "text/plain".to_string(),
        };

        let serialized = serde_json::to_value(&contents).unwrap();

        assert_eq!(serialized["content"], "# Documentation");
        assert_eq!(serialized["mimeType"], "text/plain");
    }

    #[test]
    fn test_capabilities_extensions_omitted_when_none() {
        let caps = ServerCapabilities {
            tools: None,
            resources: None,
            extensions: None,
        };
        let serialized = serde_json::to_value(&caps).unwrap();
        assert!(serialized.get("extensions").is_none());
    }

    #[test]
    fn test_capabilities_extensions_serialized_when_present() {
        let caps = ServerCapabilities {
            tools: None,
            resources: None,
            extensions: Some(json!({
                "com.aiperceivable/management": {
                    "surfaces": ["health", "control"],
                    "protocolVersion": "1.30.0"
                }
            })),
        };
        let serialized = serde_json::to_value(&caps).unwrap();
        assert_eq!(
            serialized["extensions"]["com.aiperceivable/management"]["surfaces"],
            json!(["health", "control"])
        );
    }

    #[test]
    fn test_resource_template_serializes_to_mcp_json() {
        let template = ResourceTemplate {
            uri_template: "apcore://system.health.module/{module_id}".to_string(),
            name: "system.health.module".to_string(),
            mime_type: "application/json".to_string(),
        };
        let serialized = serde_json::to_value(&template).unwrap();
        assert_eq!(
            serialized["uriTemplate"],
            "apcore://system.health.module/{module_id}"
        );
        assert_eq!(serialized["mimeType"], "application/json");
    }

    #[test]
    fn test_tool_roundtrip_deserialization() {
        let json_str = r#"{
            "name": "test",
            "description": "desc",
            "inputSchema": {},
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false
            }
        }"#;

        let tool: Tool = serde_json::from_str(json_str).unwrap();
        assert_eq!(tool.name, "test");
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(true));
    }
}
