//! Explorer mount — creates an Axum router for browsing and introspecting
//! the registered MCP tools.
//!
//! Delegates to `mcp-embedded-ui` for HTML rendering and HTTP handlers.
//! Bridges apcore's `Authenticator` and `AUTH_IDENTITY` task-local into the
//! shared UI library.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::auth::Authenticator;

// ---------------------------------------------------------------------------
// ToolInfo
// ---------------------------------------------------------------------------

/// Metadata about a single MCP tool, suitable for display in the explorer UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolInfo {
    /// The tool's unique name.
    pub name: String,
    /// A human-readable description of what the tool does.
    pub description: String,
    /// The JSON Schema describing the tool's input parameters.
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    /// MCP tool annotations (`destructiveHint`, `readOnlyHint`, ...).
    ///
    /// The explorer UI renders these as hint badges — including the
    /// destructive-tool warning shown on the one surface that also offers
    /// direct execution. Left `None`, that warning can never appear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
}

impl mcp_embedded_ui::Tool for ToolInfo {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn annotations(&self) -> Option<serde_json::Value> {
        self.annotations.clone()
    }
}

// ---------------------------------------------------------------------------
// HandleCallFn
// ---------------------------------------------------------------------------

/// The result of executing a tool call: a list of content blocks, an
/// `is_error` flag, and an optional error message.
pub type CallResult = (Vec<serde_json::Value>, bool, Option<String>);

/// Async callback used to execute a tool by name with the given arguments.
pub type HandleCallFn = Arc<
    dyn Fn(String, serde_json::Value) -> Pin<Box<dyn Future<Output = CallResult> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// ExplorerConfig
// ---------------------------------------------------------------------------

/// Configuration for the explorer UI and API routes.
pub struct ExplorerConfig {
    /// The tools to expose in the explorer.
    pub tools: Vec<ToolInfo>,
    /// Optional callback for executing tool calls from the UI.
    pub handle_call: Option<HandleCallFn>,
    /// Whether the explorer UI allows executing tools.
    pub allow_execute: bool,
    /// URL prefix where the explorer is mounted (e.g. `"/explorer"`).
    pub explorer_prefix: String,
    /// Optional authenticator for protecting explorer endpoints.
    pub authenticator: Option<Arc<dyn Authenticator>>,
    /// Page title shown in the browser tab and heading.
    pub title: String,
    /// Optional project name shown in the explorer footer.
    pub project_name: Option<String>,
    /// Optional project URL linked in the explorer footer.
    pub project_url: Option<String>,
}

impl Default for ExplorerConfig {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            handle_call: None,
            allow_execute: false,
            explorer_prefix: "/explorer".to_string(),
            authenticator: None,
            title: "MCP Tool Explorer".to_string(),
            project_name: None,
            project_url: None,
        }
    }
}

impl ExplorerConfig {
    /// Create a new `ExplorerConfig` with the given tools and defaults for
    /// everything else.
    pub fn new(tools: Vec<ToolInfo>) -> Self {
        Self {
            tools,
            ..Default::default()
        }
    }

    /// Set the `allow_execute` flag.
    pub fn allow_execute(mut self, allow: bool) -> Self {
        self.allow_execute = allow;
        self
    }

    /// Set the explorer URL prefix.
    pub fn explorer_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.explorer_prefix = prefix.into();
        self
    }

    /// Set the page title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set the project name shown in the footer.
    pub fn project_name(mut self, name: impl Into<String>) -> Self {
        self.project_name = Some(name.into());
        self
    }

    /// Set the project URL linked in the footer.
    pub fn project_url(mut self, url: impl Into<String>) -> Self {
        self.project_url = Some(url.into());
        self
    }

    /// Set the authenticator.
    pub fn authenticator(mut self, auth: Arc<dyn Authenticator>) -> Self {
        self.authenticator = Some(auth);
        self
    }

    /// Set the tool-execution callback.
    pub fn handle_call(mut self, f: HandleCallFn) -> Self {
        self.handle_call = Some(f);
        self
    }
}

// ---------------------------------------------------------------------------
// Private: bridges between apcore and mcp_embedded_ui types
// ---------------------------------------------------------------------------

/// Wraps apcore's `Authenticator` to implement `mcp_embedded_ui::Authenticator`.
///
/// Converts `apcore::Identity` field-by-field to `mcp_embedded_ui::Identity`.
struct AuthBridge {
    inner: Arc<dyn Authenticator>,
}

#[async_trait]
impl mcp_embedded_ui::Authenticator for AuthBridge {
    async fn authenticate(
        &self,
        headers: &HashMap<String, String>,
    ) -> Option<mcp_embedded_ui::Identity> {
        let id = self.inner.authenticate(headers).await?;
        Some(mcp_embedded_ui::Identity {
            id: id.id().to_string(),
            identity_type: id.identity_type().to_string(),
            roles: id.roles().to_vec(),
            attrs: id.attrs().clone(),
        })
    }
}

/// Wraps an apcore `HandleCallFn` into a `mcp_embedded_ui::ToolCallFn`.
///
/// The bridge:
/// 1. Reads the authenticated identity from `mcp_embedded_ui::AUTH_IDENTITY`
///    (populated by `mcp-embedded-ui` before invoking the handler).
/// 2. Converts it back to `apcore::Identity` and scopes it into
///    `crate::auth::middleware::AUTH_IDENTITY`, so tool handlers work the same
///    whether the call arrives via MCP or the explorer.
/// 3. Converts the `Vec<serde_json::Value>` content blocks to
///    `Vec<mcp_embedded_ui::Content>`.
fn wrap_call_fn(inner: HandleCallFn) -> mcp_embedded_ui::ToolCallFn {
    Arc::new(move |name: String, args: serde_json::Value| {
        let inner = inner.clone();
        Box::pin(async move {
            use crate::auth::middleware::AUTH_IDENTITY as APCORE_IDENTITY;
            use mcp_embedded_ui::AUTH_IDENTITY as UI_IDENTITY;

            // Read identity set by mcp-embedded-ui and convert to apcore::Identity.
            let apcore_identity =
                UI_IDENTITY
                    .try_with(|id| id.clone())
                    .ok()
                    .flatten()
                    .map(|ui_id| {
                        apcore::Identity::new(
                            ui_id.id,
                            ui_id.identity_type,
                            ui_id.roles,
                            ui_id.attrs,
                        )
                    });

            // Run the call handler within apcore's AUTH_IDENTITY scope.
            let (raw_content, is_error, error_code) = APCORE_IDENTITY
                .scope(apcore_identity, async move { inner(name, args).await })
                .await;

            // Convert Vec<serde_json::Value> → Vec<mcp_embedded_ui::Content>.
            let content = raw_content
                .into_iter()
                .map(|v| mcp_embedded_ui::Content {
                    content_type: v
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or_else(|| {
                            tracing::warn!(
                                "content block missing 'type' field, defaulting to 'text'"
                            );
                            "text"
                        })
                        .to_string(),
                    text: v
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string()),
                    mime_type: v
                        .get("mimeType")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string()),
                    data: v
                        .get("data")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string()),
                })
                .collect();

            Ok((content, is_error, error_code))
        })
    })
}

// ---------------------------------------------------------------------------
// Public factory
// ---------------------------------------------------------------------------

/// Create an Axum [`Router`] that serves the explorer UI and API.
///
/// The explorer provides:
/// - A self-contained HTML page for browsing and testing tools
/// - `GET /tools` — JSON list of all registered tools
/// - `GET /tools/{name}` — full tool detail including JSON Schema
/// - `POST /tools/{name}/call` — execute a tool interactively
pub fn create_explorer_mount(config: ExplorerConfig) -> Router {
    let prefix = config.explorer_prefix.clone();

    let tools: Vec<Arc<dyn mcp_embedded_ui::Tool>> = config
        .tools
        .into_iter()
        .map(|t| Arc::new(t) as Arc<dyn mcp_embedded_ui::Tool>)
        .collect();

    let handler = match config.handle_call {
        Some(f) => mcp_embedded_ui::ToolCallHandler::Basic(wrap_call_fn(f)),
        None => mcp_embedded_ui::ToolCallHandler::Basic(Arc::new(|name, _args| {
            Box::pin(async move {
                Err(mcp_embedded_ui::ToolCallError::Internal(format!(
                    "No call handler configured for tool '{}'",
                    name
                )))
            })
        })),
    };

    let mut ui_config = mcp_embedded_ui::UiConfig {
        allow_execute: config.allow_execute,
        title: config.title,
        project_name: config.project_name,
        project_url: config.project_url,
        auth_hook: Default::default(),
        authenticator: None,
    };

    if let Some(auth) = config.authenticator {
        ui_config.authenticator = Some(Arc::new(AuthBridge { inner: auth }));
    }

    mcp_embedded_ui::create_mount(Some(&prefix), Arc::new(tools), handler, ui_config)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    // -- ExplorerConfig defaults -------------------------------------------

    #[test]
    fn config_default_values() {
        let cfg = ExplorerConfig::default();
        assert_eq!(cfg.explorer_prefix, "/explorer");
        assert!(!cfg.allow_execute);
        assert_eq!(cfg.title, "MCP Tool Explorer");
        assert!(cfg.authenticator.is_none());
        assert!(cfg.handle_call.is_none());
        assert!(cfg.tools.is_empty());
        assert!(cfg.project_name.is_none());
        assert!(cfg.project_url.is_none());
    }

    #[test]
    fn config_builder_overrides() {
        let cfg = ExplorerConfig::new(vec![])
            .allow_execute(true)
            .explorer_prefix("/tools-ui")
            .title("My Tools")
            .project_name("demo")
            .project_url("https://example.com");

        assert!(cfg.allow_execute);
        assert_eq!(cfg.explorer_prefix, "/tools-ui");
        assert_eq!(cfg.title, "My Tools");
        assert_eq!(cfg.project_name.as_deref(), Some("demo"));
        assert_eq!(cfg.project_url.as_deref(), Some("https://example.com"));
    }

    // -- ToolInfo serialization --------------------------------------------

    #[test]
    fn tool_info_serializes_with_correct_field_names() {
        let tool = ToolInfo {
            name: "add".to_string(),
            description: "Add two numbers".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "a": { "type": "number" },
                    "b": { "type": "number" }
                }
            }),
            annotations: None,
        };

        let serialized = serde_json::to_value(&tool).unwrap();
        assert_eq!(serialized["name"], "add");
        assert_eq!(serialized["description"], "Add two numbers");
        assert!(serialized.get("inputSchema").is_some());
        assert!(serialized.get("input_schema").is_none());
    }

    #[test]
    fn tool_info_round_trip() {
        let tool = ToolInfo {
            name: "search".to_string(),
            description: "Search documents".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        };

        let json_str = serde_json::to_string(&tool).unwrap();
        let deserialized: ToolInfo = serde_json::from_str(&json_str).unwrap();
        assert_eq!(tool, deserialized);
    }

    // -- HandleCallFn type check ------------------------------------------

    #[test]
    fn handle_call_fn_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HandleCallFn>();
    }

    // -- Helpers for integration tests ------------------------------------

    fn sample_tools() -> Vec<ToolInfo> {
        vec![
            ToolInfo {
                name: "tool_one".to_string(),
                description: "First tool".to_string(),
                input_schema: json!({"type": "object"}),
                annotations: Some(json!({"destructiveHint": true})),
            },
            ToolInfo {
                name: "tool_two".to_string(),
                description: "Second tool".to_string(),
                input_schema: json!({"type": "object"}),
                annotations: None,
            },
        ]
    }

    fn mock_handle_call() -> HandleCallFn {
        Arc::new(|name: String, args: serde_json::Value| {
            Box::pin(async move {
                let content =
                    vec![json!({"type": "text", "text": format!("called {} with {}", name, args)})];
                (content, false, None)
            })
        })
    }

    // -- Integration: full lifecycle --------------------------------------

    #[tokio::test]
    async fn integration_html_page_served() {
        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(true)
            .handle_call(mock_handle_call())
            .title("Test Explorer");
        let app = create_explorer_mount(config);

        let req = Request::get("/explorer").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("<title>Test Explorer</title>"));
        assert!(html.contains("var executeEnabled = true"));
    }

    #[tokio::test]
    async fn integration_tools_endpoint() {
        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(true)
            .handle_call(mock_handle_call());
        let app = create_explorer_mount(config);

        let req = Request::get("/explorer/tools").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let tools: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[tokio::test]
    async fn integration_call_tool_success() {
        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(true)
            .handle_call(mock_handle_call());
        let app = create_explorer_mount(config);

        let req = Request::post("/explorer/tools/tool_one/call")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"arg": "value"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // mcp-embedded-ui serializes as "isError" (camelCase, MCP spec)
        assert!(!json["isError"].as_bool().unwrap());
        assert!(json["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tool_one"));
    }

    #[tokio::test]
    async fn integration_call_nonexistent_tool_returns_404() {
        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(true)
            .handle_call(mock_handle_call());
        let app = create_explorer_mount(config);

        let req = Request::post("/explorer/tools/nonexistent/call")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    // -- Integration: execution disabled ----------------------------------

    #[tokio::test]
    async fn integration_execute_disabled_returns_403() {
        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(false)
            .handle_call(mock_handle_call());
        let app = create_explorer_mount(config);

        let req = Request::post("/explorer/tools/tool_one/call")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn integration_execute_disabled_listing_still_works() {
        let config = ExplorerConfig::new(sample_tools()).allow_execute(false);
        let app = create_explorer_mount(config);

        let req = Request::get("/explorer/tools").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    // -- Integration: auth required ---------------------------------------

    #[tokio::test]
    async fn integration_auth_required_no_token_returns_401() {
        use crate::auth::protocol::Identity;

        struct RejectAuth;

        #[async_trait]
        impl Authenticator for RejectAuth {
            async fn authenticate(&self, _headers: &HashMap<String, String>) -> Option<Identity> {
                None
            }
        }

        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(true)
            .handle_call(mock_handle_call())
            .authenticator(Arc::new(RejectAuth));
        let app = create_explorer_mount(config);

        let req = Request::post("/explorer/tools/tool_one/call")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn integration_auth_with_valid_token_succeeds() {
        use crate::auth::middleware::AUTH_IDENTITY;
        use crate::auth::protocol::Identity;

        #[derive(Clone)]
        struct AcceptAuth;

        #[async_trait]
        impl Authenticator for AcceptAuth {
            async fn authenticate(&self, headers: &HashMap<String, String>) -> Option<Identity> {
                let auth = headers.get("authorization")?;
                if auth.starts_with("Bearer ") {
                    Some(Identity::new(
                        "authed-user".to_string(),
                        "human".to_string(),
                        vec!["user".to_string()],
                        Default::default(),
                    ))
                } else {
                    None
                }
            }
        }

        // Handler verifies that crate::auth::middleware::AUTH_IDENTITY is set.
        let handle_call: HandleCallFn = Arc::new(|_name, _args| {
            Box::pin(async move {
                let identity_id = AUTH_IDENTITY
                    .try_with(|id| {
                        id.as_ref()
                            .map(|i| i.id().to_string())
                            .unwrap_or_else(|| "none".to_string())
                    })
                    .unwrap_or_else(|_| "no-task-local".to_string());
                let content = vec![json!({"type": "text", "text": identity_id})];
                (content, false, None)
            })
        });

        let config = ExplorerConfig::new(sample_tools())
            .allow_execute(true)
            .handle_call(handle_call)
            .authenticator(Arc::new(AcceptAuth));
        let app = create_explorer_mount(config);

        let req = Request::post("/explorer/tools/tool_one/call")
            .header("content-type", "application/json")
            .header("authorization", "Bearer valid")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Verify the full auth bridge: apcore AUTH_IDENTITY was populated.
        assert_eq!(json["content"][0]["text"], "authed-user");
    }

    // -- HTML: allow_execute=false has no data attribute ------------------

    #[tokio::test]
    async fn integration_html_no_execute_attribute_when_disabled() {
        let config = ExplorerConfig::new(sample_tools()).allow_execute(false);
        let app = create_explorer_mount(config);

        let req = Request::get("/explorer").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("var executeEnabled = true"));
    }

    // -- wrap_call_fn content conversion tests ----------------------------

    #[tokio::test]
    async fn wrap_call_fn_converts_text_content() {
        let inner: HandleCallFn = Arc::new(|_name, _args| {
            Box::pin(async move {
                let content = vec![json!({"type": "text", "text": "hello world"})];
                (content, false, None)
            })
        });
        let wrapped = super::wrap_call_fn(inner);
        let result = wrapped("test".into(), json!({})).await.unwrap();
        let (content, is_error, error_code) = result;
        assert!(!is_error);
        assert!(error_code.is_none());
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].content_type, "text");
        assert_eq!(content[0].text.as_deref(), Some("hello world"));
        assert!(content[0].mime_type.is_none());
        assert!(content[0].data.is_none());
    }

    #[tokio::test]
    async fn wrap_call_fn_converts_image_content() {
        let inner: HandleCallFn = Arc::new(|_name, _args| {
            Box::pin(async move {
                let content =
                    vec![json!({"type": "image", "mimeType": "image/png", "data": "base64data"})];
                (content, false, None)
            })
        });
        let wrapped = super::wrap_call_fn(inner);
        let result = wrapped("test".into(), json!({})).await.unwrap();
        let (content, _, _) = result;
        assert_eq!(content[0].content_type, "image");
        assert_eq!(content[0].mime_type.as_deref(), Some("image/png"));
        assert_eq!(content[0].data.as_deref(), Some("base64data"));
        assert!(content[0].text.is_none());
    }

    #[tokio::test]
    async fn wrap_call_fn_missing_type_defaults_to_text() {
        let inner: HandleCallFn = Arc::new(|_name, _args| {
            Box::pin(async move {
                let content = vec![json!({"text": "no type field"})];
                (content, false, None)
            })
        });
        let wrapped = super::wrap_call_fn(inner);
        let result = wrapped("test".into(), json!({})).await.unwrap();
        let (content, _, _) = result;
        assert_eq!(content[0].content_type, "text");
        assert_eq!(content[0].text.as_deref(), Some("no type field"));
    }

    #[tokio::test]
    async fn wrap_call_fn_propagates_error_state() {
        let inner: HandleCallFn = Arc::new(|_name, _args| {
            Box::pin(async move {
                let content = vec![json!({"type": "text", "text": "error details"})];
                (content, true, Some("ERR_CODE".to_string()))
            })
        });
        let wrapped = super::wrap_call_fn(inner);
        let result = wrapped("test".into(), json!({})).await.unwrap();
        let (content, is_error, error_code) = result;
        assert!(is_error);
        assert_eq!(error_code.as_deref(), Some("ERR_CODE"));
        assert_eq!(content[0].text.as_deref(), Some("error details"));
    }
}
