//! MCP helper functions — progress reporting and elicitation.
//!
//! These helpers wrap MCP protocol primitives for common patterns.
//! Callback type aliases allow the execution router to inject MCP-specific
//! behaviour while keeping module code transport-agnostic.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The action taken by the user in response to an elicitation prompt.
///
/// The `Unknown(String)` variant is a catch-all for any action string not
/// explicitly listed. Python+TS treat any non-"accept" string as rejected;
/// Rust captures the raw string so the reason can be surfaced at the caller.
/// [D11-020]
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub enum ElicitAction {
    /// The user accepted the prompt.
    Accept,
    /// The user declined the prompt.
    Decline,
    /// The user cancelled the prompt.
    Cancel,
    /// An unknown/unrecognised action string received from the client.
    Unknown(String),
}

impl serde::Serialize for ElicitAction {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ElicitAction::Accept => s.serialize_str("accept"),
            ElicitAction::Decline => s.serialize_str("decline"),
            ElicitAction::Cancel => s.serialize_str("cancel"),
            ElicitAction::Unknown(raw) => s.serialize_str(raw),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ElicitAction {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "accept" => ElicitAction::Accept,
            "decline" => ElicitAction::Decline,
            "cancel" => ElicitAction::Cancel,
            other => ElicitAction::Unknown(other.to_string()),
        })
    }
}

/// Result of an elicitation request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ElicitResult {
    /// The action the user chose.
    pub action: ElicitAction,
    /// Optional content returned with the action (e.g. form data).
    pub content: Option<Value>,
}

// ---------------------------------------------------------------------------
// Callback type aliases
// ---------------------------------------------------------------------------

/// Async callback for reporting progress to the MCP client.
///
/// # Parameters
/// - `progress` (`f64`) — current progress value.
/// - `total` (`Option<f64>`) — total expected value, if known.
/// - `message` (`Option<String>`) — human-readable progress message.
pub type ProgressCallback = Box<
    dyn Fn(f64, Option<f64>, Option<String>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Async callback for eliciting information from the user via the MCP client.
///
/// # Parameters
/// - `message` (`String`) — the prompt message to display.
/// - `requested_schema` (`Option<Value>`) — JSON Schema describing expected input.
///
/// # Returns
/// `Some(ElicitResult)` if the client responded, `None` if elicitation is
/// unsupported or unavailable.
pub type ElicitCallback = Box<
    dyn Fn(String, Option<Value>) -> Pin<Box<dyn Future<Output = Option<ElicitResult>> + Send>>
        + Send
        + Sync,
>;

/// Key for the progress-reporting callback in context data.
pub const MCP_PROGRESS_KEY: &str = "_mcp_progress";

/// Key for the elicitation callback in context data.
pub const MCP_ELICIT_KEY: &str = "_mcp_elicit";

/// Key under which the router writes the id of the live [`ElicitCallback`]
/// into `apcore::Context.data`, so an `ApprovalHandler` can reach it.
///
/// A closure is not a `serde_json::Value`, so [`MCP_ELICIT_KEY`] can only ever
/// carry the marker string `"available"` — enough to advertise that
/// elicitation exists, not enough to perform one. The id written here is
/// exchanged for the callback itself through the crate-local registry that
/// owns it for the duration of the tool call.
///
/// A separate key rather than a widening of [`MCP_ELICIT_KEY`]: modules
/// already test that one against `"available"`, and changing its type would
/// break them for no gain. Both are `_`-prefixed, which apcore's
/// `Context::serialize` filters out of the wire shape, so neither reaches a
/// client.
pub const MCP_ELICIT_CALL_ID_KEY: &str = "_mcp_elicit_call_id";

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Report progress to the MCP client.
///
/// When a [`ProgressCallback`] is provided, it is invoked with the given
/// progress values. If `callback` is `None` the call is a silent no-op,
/// which allows modules to call this unconditionally regardless of whether
/// the execution context supports progress reporting.
///
/// The `context` parameter is reserved for future use when a concrete
/// `apcore::Context` type is available; it is currently unused.
///
/// # Arguments
/// * `context` - The MCP request context (opaque for now).
/// * `callback` - Optional progress callback injected by the execution router.
/// * `progress` - Current progress value.
/// * `total` - Total expected value (if known).
/// * `message` - Optional human-readable progress message.
pub async fn report_progress(
    _context: &Value,
    callback: Option<&ProgressCallback>,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) {
    let Some(cb) = callback else {
        tracing::debug!("no progress callback provided, skipping");
        return;
    };
    cb(progress, total, message.map(|s| s.to_owned())).await;
}

/// Elicit information from the user via the MCP client.
///
/// When an [`ElicitCallback`] is provided, it is invoked with the given
/// message and optional JSON Schema. Returns `Some(ElicitResult)` from the
/// callback, or `None` if the callback is absent (meaning elicitation is
/// unavailable in the current execution context).
///
/// The `context` parameter is reserved for future use when a concrete
/// `apcore::Context` type is available; it is currently unused.
///
/// # Arguments
/// * `context` - The MCP request context (opaque for now).
/// * `callback` - Optional elicit callback injected by the execution router.
/// * `message` - Prompt message for the user.
/// * `requested_schema` - JSON Schema describing the expected response shape.
///
/// # Returns
/// `Some(ElicitResult)` if the client responded, `None` if elicitation is unsupported.
pub async fn elicit(
    _context: &Value,
    callback: Option<&ElicitCallback>,
    message: &str,
    requested_schema: Option<&Value>,
) -> Option<ElicitResult> {
    let Some(cb) = callback else {
        tracing::debug!("no elicit callback provided, skipping");
        return None;
    };
    cb(message.to_owned(), requested_schema.cloned()).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- ElicitAction serde round-trip tests --------------------------------

    #[test]
    fn elicit_action_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ElicitAction::Accept).unwrap(),
            "\"accept\""
        );
        assert_eq!(
            serde_json::to_string(&ElicitAction::Decline).unwrap(),
            "\"decline\""
        );
        assert_eq!(
            serde_json::to_string(&ElicitAction::Cancel).unwrap(),
            "\"cancel\""
        );
    }

    #[test]
    fn elicit_action_deserializes_from_snake_case() {
        let accept: ElicitAction = serde_json::from_str("\"accept\"").unwrap();
        assert_eq!(accept, ElicitAction::Accept);

        let decline: ElicitAction = serde_json::from_str("\"decline\"").unwrap();
        assert_eq!(decline, ElicitAction::Decline);

        let cancel: ElicitAction = serde_json::from_str("\"cancel\"").unwrap();
        assert_eq!(cancel, ElicitAction::Cancel);
    }

    #[test]
    fn elicit_action_unknown_variant_maps_to_unknown() {
        // [D11-020] Unknown action strings must NOT error; they map to Unknown(String).
        // Python+TS treat any non-"accept" string as rejected; Rust captures the
        // raw value so the reason can be surfaced at the caller.
        let result = serde_json::from_str::<ElicitAction>("\"unknown\"");
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            ElicitAction::Unknown("unknown".to_string())
        );
    }

    #[test]
    fn elicit_action_unknown_serializes_raw() {
        let action = ElicitAction::Unknown("foobar".to_string());
        assert_eq!(serde_json::to_string(&action).unwrap(), "\"foobar\"");
    }

    // -- ElicitResult serde round-trip tests --------------------------------

    #[test]
    fn elicit_result_round_trip_with_content() {
        let result = ElicitResult {
            action: ElicitAction::Accept,
            content: Some(json!({"name": "Alice", "age": 30})),
        };
        let serialized = serde_json::to_string(&result).unwrap();
        let deserialized: ElicitResult = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.action, ElicitAction::Accept);
        assert_eq!(deserialized.content.unwrap()["name"], "Alice");
    }

    #[test]
    fn elicit_result_round_trip_without_content() {
        let result = ElicitResult {
            action: ElicitAction::Decline,
            content: None,
        };
        let serialized = serde_json::to_string(&result).unwrap();
        let deserialized: ElicitResult = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.action, ElicitAction::Decline);
        assert!(deserialized.content.is_none());
    }

    #[test]
    fn elicit_result_deserializes_from_json_object() {
        let json_str = r#"{"action": "cancel", "content": null}"#;
        let result: ElicitResult = serde_json::from_str(json_str).unwrap();
        assert_eq!(result.action, ElicitAction::Cancel);
        assert!(result.content.is_none());
    }

    // -- JsonSchema generation tests ----------------------------------------

    #[test]
    fn elicit_action_generates_json_schema() {
        let schema = schemars::schema_for!(ElicitAction);
        let schema_json = serde_json::to_value(&schema).unwrap();
        let schema_str = schema_json.to_string();
        assert!(schema_str.contains("accept"));
        assert!(schema_str.contains("decline"));
        assert!(schema_str.contains("cancel"));
    }

    #[test]
    fn elicit_result_generates_json_schema() {
        let schema = schemars::schema_for!(ElicitResult);
        let schema_json = serde_json::to_value(&schema).unwrap();
        let schema_str = schema_json.to_string();
        assert!(schema_str.contains("action"));
        assert!(schema_str.contains("content"));
    }

    // -- Callback type alias tests ------------------------------------------

    #[test]
    fn progress_callback_is_constructible() {
        let _cb: ProgressCallback = Box::new(|_progress, _total, _message| Box::pin(async {}));
    }

    #[test]
    fn elicit_callback_is_constructible() {
        let _cb: ElicitCallback = Box::new(|_message, _schema| {
            Box::pin(async {
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });
    }

    #[tokio::test]
    async fn progress_callback_can_be_invoked() {
        let cb: ProgressCallback = Box::new(|progress, total, message| {
            Box::pin(async move {
                assert!((progress - 0.5).abs() < f64::EPSILON);
                assert_eq!(total, Some(1.0));
                assert_eq!(message.as_deref(), Some("halfway"));
            })
        });
        cb(0.5, Some(1.0), Some("halfway".to_string())).await;
    }

    #[tokio::test]
    async fn elicit_callback_can_be_invoked() {
        let cb: ElicitCallback = Box::new(|message, _schema| {
            Box::pin(async move {
                assert_eq!(message, "confirm?");
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: Some(json!({"confirmed": true})),
                })
            })
        });
        let result = cb("confirm?".to_string(), None).await.unwrap();
        assert_eq!(result.action, ElicitAction::Accept);
        assert_eq!(result.content.unwrap()["confirmed"], true);
    }

    // -- Context key constant tests -----------------------------------------

    #[test]
    fn mcp_progress_key_matches_python() {
        assert_eq!(MCP_PROGRESS_KEY, "_mcp_progress");
    }

    #[test]
    fn mcp_elicit_key_matches_python() {
        assert_eq!(MCP_ELICIT_KEY, "_mcp_elicit");
    }

    // -- report_progress function tests -------------------------------------

    #[tokio::test]
    async fn report_progress_invokes_callback_with_correct_args() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: ProgressCallback = Box::new(move |progress, total, message| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some((progress, total, message));
            })
        });

        let ctx = json!({});
        report_progress(&ctx, Some(&cb), 0.5, Some(1.0), Some("halfway")).await;

        let (p, t, m) = captured.lock().unwrap().take().unwrap();
        assert!((p - 0.5).abs() < f64::EPSILON);
        assert_eq!(t, Some(1.0));
        assert_eq!(m.as_deref(), Some("halfway"));
    }

    #[tokio::test]
    async fn report_progress_no_op_when_callback_is_none() {
        let ctx = json!({});
        // Should not panic
        report_progress(&ctx, None, 0.5, Some(1.0), Some("halfway")).await;
    }

    #[tokio::test]
    async fn report_progress_passes_none_message() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: ProgressCallback = Box::new(move |progress, total, message| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some((progress, total, message));
            })
        });

        let ctx = json!({});
        report_progress(&ctx, Some(&cb), 3.0, None, None).await;

        let (p, t, m) = captured.lock().unwrap().take().unwrap();
        assert!((p - 3.0).abs() < f64::EPSILON);
        assert_eq!(t, None);
        assert_eq!(m, None);
    }

    // -- elicit function tests -----------------------------------------------

    #[tokio::test]
    async fn elicit_invokes_callback_and_returns_accept() {
        let cb: ElicitCallback = Box::new(|_message, _schema| {
            Box::pin(async {
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });

        let ctx = json!({});
        let result = elicit(&ctx, Some(&cb), "confirm?", None).await;
        let result = result.unwrap();
        assert_eq!(result.action, ElicitAction::Accept);
        assert!(result.content.is_none());
    }

    #[tokio::test]
    async fn elicit_invokes_callback_and_returns_decline_with_content() {
        let cb: ElicitCallback = Box::new(|_message, _schema| {
            Box::pin(async {
                Some(ElicitResult {
                    action: ElicitAction::Decline,
                    content: Some(json!({"reason": "not now"})),
                })
            })
        });

        let ctx = json!({});
        let result = elicit(&ctx, Some(&cb), "proceed?", None).await.unwrap();
        assert_eq!(result.action, ElicitAction::Decline);
        assert_eq!(result.content.unwrap()["reason"], "not now");
    }

    #[tokio::test]
    async fn elicit_returns_none_when_callback_is_none() {
        let ctx = json!({});
        let result = elicit(&ctx, None, "hello?", None).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn elicit_passes_schema_to_callback() {
        use std::sync::{Arc, Mutex};

        let captured_schema = Arc::new(Mutex::new(None));
        let captured_clone = captured_schema.clone();
        let cb: ElicitCallback = Box::new(move |_message, schema| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some(schema);
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });

        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        let ctx = json!({});
        elicit(&ctx, Some(&cb), "fill form", Some(&schema)).await;

        let captured = captured_schema.lock().unwrap().take().unwrap();
        assert_eq!(captured.unwrap()["type"], "object");
    }

    #[tokio::test]
    async fn elicit_callback_returns_none_propagates() {
        let cb: ElicitCallback = Box::new(|_message, _schema| Box::pin(async { None }));

        let ctx = json!({});
        let result = elicit(&ctx, Some(&cb), "hello?", None).await;
        assert!(result.is_none());
    }

    // -- elicit cancel variant test ------------------------------------------

    #[tokio::test]
    async fn elicit_invokes_callback_and_returns_cancel() {
        let cb: ElicitCallback = Box::new(|_message, _schema| {
            Box::pin(async {
                Some(ElicitResult {
                    action: ElicitAction::Cancel,
                    content: None,
                })
            })
        });

        let ctx = json!({});
        let result = elicit(&ctx, Some(&cb), "are you sure?", None)
            .await
            .unwrap();
        assert_eq!(result.action, ElicitAction::Cancel);
        assert!(result.content.is_none());
    }

    // -- Edge case tests -----------------------------------------------------

    #[tokio::test]
    async fn report_progress_with_zero_values() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: ProgressCallback = Box::new(move |progress, total, message| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some((progress, total, message));
            })
        });

        let ctx = json!({});
        report_progress(&ctx, Some(&cb), 0.0, Some(0.0), Some("")).await;

        let (p, t, m) = captured.lock().unwrap().take().unwrap();
        assert!((p - 0.0).abs() < f64::EPSILON);
        assert_eq!(t, Some(0.0));
        assert_eq!(m.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn report_progress_with_large_values() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: ProgressCallback = Box::new(move |progress, total, message| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some((progress, total, message));
            })
        });

        let ctx = json!({});
        report_progress(&ctx, Some(&cb), f64::MAX, Some(f64::MAX), None).await;

        let (p, t, m) = captured.lock().unwrap().take().unwrap();
        assert_eq!(p, f64::MAX);
        assert_eq!(t, Some(f64::MAX));
        assert!(m.is_none());
    }

    #[tokio::test]
    async fn report_progress_with_nan() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: ProgressCallback = Box::new(move |progress, total, message| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some((progress, total, message));
            })
        });

        let ctx = json!({});
        report_progress(&ctx, Some(&cb), f64::NAN, Some(f64::NAN), None).await;

        let (p, t, _m) = captured.lock().unwrap().take().unwrap();
        assert!(p.is_nan());
        assert!(t.unwrap().is_nan());
    }

    #[tokio::test]
    async fn elicit_with_empty_message() {
        use std::sync::{Arc, Mutex};

        let captured_msg = Arc::new(Mutex::new(None));
        let captured_clone = captured_msg.clone();
        let cb: ElicitCallback = Box::new(move |message, _schema| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some(message);
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });

        let ctx = json!({});
        let result = elicit(&ctx, Some(&cb), "", None).await.unwrap();
        assert_eq!(result.action, ElicitAction::Accept);
        assert_eq!(captured_msg.lock().unwrap().as_deref(), Some(""));
    }

    // -- Integration smoke tests ---------------------------------------------

    #[tokio::test]
    async fn smoke_test_progress_and_elicit_full_lifecycle() {
        use std::sync::{Arc, Mutex};

        // Set up shared capture for progress calls
        let progress_calls = Arc::new(Mutex::new(Vec::<(f64, Option<f64>, Option<String>)>::new()));
        let progress_clone = progress_calls.clone();
        let progress_cb: ProgressCallback = Box::new(move |progress, total, message| {
            let calls = progress_clone.clone();
            Box::pin(async move {
                calls.lock().unwrap().push((progress, total, message));
            })
        });

        // Set up elicit callback that returns Accept with content
        let elicit_cb: ElicitCallback = Box::new(|_message, _schema| {
            Box::pin(async {
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: Some(json!({"name": "test"})),
                })
            })
        });

        let ctx = json!({});

        // Call report_progress multiple times
        report_progress(&ctx, Some(&progress_cb), 0.0, Some(100.0), Some("starting")).await;
        report_progress(&ctx, Some(&progress_cb), 50.0, Some(100.0), Some("halfway")).await;
        report_progress(&ctx, Some(&progress_cb), 100.0, Some(100.0), Some("done")).await;

        // Verify all progress calls were captured
        {
            let calls = progress_calls.lock().unwrap();
            assert_eq!(calls.len(), 3);
            assert!((calls[0].0 - 0.0).abs() < f64::EPSILON);
            assert_eq!(calls[0].2.as_deref(), Some("starting"));
            assert!((calls[1].0 - 50.0).abs() < f64::EPSILON);
            assert_eq!(calls[1].2.as_deref(), Some("halfway"));
            assert!((calls[2].0 - 100.0).abs() < f64::EPSILON);
            assert_eq!(calls[2].2.as_deref(), Some("done"));
        }

        // Call elicit with a schema
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        let result = elicit(&ctx, Some(&elicit_cb), "Enter name:", Some(&schema)).await;
        let result = result.unwrap();
        assert_eq!(result.action, ElicitAction::Accept);
        assert_eq!(result.content.unwrap()["name"], "test");
    }

    #[tokio::test]
    async fn smoke_test_no_callbacks_graceful_noop() {
        let ctx = json!({});

        // Both should be no-ops without panic
        report_progress(&ctx, None, 50.0, Some(100.0), Some("halfway")).await;
        let result = elicit(&ctx, None, "hello?", None).await;
        assert!(result.is_none());
    }
}
