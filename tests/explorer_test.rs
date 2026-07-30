//! Integration tests for the `apcore_mcp::explorer` public API.
//!
//! These tests exercise the explorer mount through the *crate's* public
//! surface (not internal helpers), so they catch breakage in:
//!
//! - The set of types re-exported under `apcore_mcp::explorer::*`.
//! - The basic GET routes for the HTML page and the JSON tools listing.
//! - The auth integration point (`Authenticator` + `AUTH_IDENTITY`).
//!
//! The unit-level tests inside `src/explorer/mount.rs` cover the full
//! lifecycle (handle_call, auth bridging, content conversion). This
//! file is the cross-module smoke test that mirrors the Python
//! integration suite shape under `tests/explorer/...`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use apcore_mcp::explorer::{create_explorer_mount, ExplorerConfig, HandleCallFn, ToolInfo};
use apcore_mcp::{Authenticator, Identity};

// -- Fixtures ----------------------------------------------------------------

fn sample_tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "math.add".to_string(),
            description: "Add two numbers".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "a": { "type": "number" },
                    "b": { "type": "number" }
                }
            }),
            annotations: Some(json!({"destructiveHint": true})),
        },
        ToolInfo {
            name: "text.upper".to_string(),
            description: "Uppercase a string".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "s": { "type": "string" } }
            }),
            annotations: None,
        },
    ]
}

fn echo_handler() -> HandleCallFn {
    Arc::new(|name: String, args: Value| {
        Box::pin(async move {
            let content =
                vec![json!({"type": "text", "text": format!("{} called with {}", name, args)})];
            (content, false, None)
        })
    })
}

// -- Mount + GET on the explorer UI route ------------------------------------

#[tokio::test]
async fn explorer_mount_serves_html_at_configured_prefix() {
    let config = ExplorerConfig::new(sample_tools())
        .allow_execute(true)
        .handle_call(echo_handler())
        .explorer_prefix("/explorer")
        .title("Integration Test Explorer");
    let app = create_explorer_mount(config);

    let req = Request::get("/explorer").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/html"),
        "expected text/html content-type, got {content_type}"
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("Integration Test Explorer"),
        "html should contain configured title"
    );
}

#[tokio::test]
async fn explorer_mount_lists_tools_as_json() {
    let config = ExplorerConfig::new(sample_tools())
        .allow_execute(true)
        .handle_call(echo_handler());
    let app = create_explorer_mount(config);

    let req = Request::get("/explorer/tools").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let tools: Vec<Value> = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert_eq!(names, vec!["math.add", "text.upper"]);
}

#[tokio::test]
async fn explorer_tool_detail_serves_annotations() {
    // The explorer UI has a render branch for a tool's annotations — the
    // destructive-tool warning among them — on the one surface that also
    // offers direct execution. It can only fire if the detail endpoint
    // actually carries them.
    let config = ExplorerConfig::new(sample_tools())
        .allow_execute(true)
        .handle_call(echo_handler());
    let app = create_explorer_mount(config);

    let req = Request::get("/explorer/tools/math.add")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let detail: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        detail
            .get("annotations")
            .and_then(|a| a.get("destructiveHint"))
            .and_then(Value::as_bool),
        Some(true),
        "tool detail must carry annotations; got {detail}"
    );
}

#[tokio::test]
async fn explorer_mount_respects_custom_prefix() {
    let config = ExplorerConfig::new(sample_tools())
        .allow_execute(true)
        .handle_call(echo_handler())
        .explorer_prefix("/tools-ui");
    let app = create_explorer_mount(config);

    // Default `/explorer` should now 404.
    let req = Request::get("/explorer").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Custom `/tools-ui` should serve.
    let req = Request::get("/tools-ui").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// -- Auth hook integration ----------------------------------------------------

struct AlwaysAuthenticator {
    identity: Identity,
}

#[async_trait]
impl Authenticator for AlwaysAuthenticator {
    async fn authenticate(&self, _headers: &HashMap<String, String>) -> Option<Identity> {
        Some(self.identity.clone())
    }
}

struct NeverAuthenticator;

#[async_trait]
impl Authenticator for NeverAuthenticator {
    async fn authenticate(&self, _headers: &HashMap<String, String>) -> Option<Identity> {
        None
    }
}

#[tokio::test]
async fn explorer_mount_rejects_when_authenticator_returns_none() {
    let config = ExplorerConfig::new(sample_tools())
        .allow_execute(true)
        .handle_call(echo_handler())
        .authenticator(Arc::new(NeverAuthenticator));
    let app = create_explorer_mount(config);

    let req = Request::post("/explorer/tools/math.add/call")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn explorer_mount_passes_through_when_authenticator_accepts() {
    let config = ExplorerConfig::new(sample_tools())
        .allow_execute(true)
        .handle_call(echo_handler())
        .authenticator(Arc::new(AlwaysAuthenticator {
            identity: Identity::new(
                "test-user".into(),
                "human".into(),
                vec!["user".into()],
                Default::default(),
            ),
        }));
    let app = create_explorer_mount(config);

    let req = Request::post("/explorer/tools/math.add/call")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"a": 1, "b": 2}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
