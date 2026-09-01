//! MCP protocol types used by the server factory.
//!
//! These are local Rust structs that mirror the MCP protocol specification.
//! They can later be replaced by types from an official MCP SDK crate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An MCP Tool definition exposed via `list_tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
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

/// Result of a `call_tool` invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<TextContent>,
    pub is_error: bool,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl CallToolResult {
    /// Create a minimal result from content + error flag (no `_meta`).
    pub fn new(content: Vec<TextContent>, is_error: bool) -> Self {
        Self {
            content,
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
    fn test_tool_serializes_to_mcp_json() {
        let tool = Tool {
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
