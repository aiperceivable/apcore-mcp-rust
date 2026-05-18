//! ElicitationApprovalHandler — uses MCP elicitation to request user approval
//! for destructive or sensitive tool executions.

use std::fmt;

use apcore::approval::{ApprovalHandler, ApprovalRequest, ApprovalResult};
use apcore::errors::ModuleError;
use async_trait::async_trait;

use crate::helpers::{ElicitAction, ElicitCallback};

/// Handles user approval requests via MCP elicitation.
///
/// Implements the apcore [`ApprovalHandler`] contract by sending an elicitation
/// prompt to the MCP client and interpreting the response.
///
/// # Lifecycle
///
/// - `request_approval` — formats a human-readable message from the
///   [`ApprovalRequest`], invokes the injected [`ElicitCallback`], and maps the
///   elicit response to an [`ApprovalResult`].
/// - `check_approval` — always returns "rejected" because Phase B (async
///   polling of pending approvals) is not supported via stateless MCP
///   elicitation.
pub struct ElicitationApprovalHandler {
    elicit: Option<ElicitCallback>,
}

impl ElicitationApprovalHandler {
    /// Create a new approval handler with an optional elicit callback.
    ///
    /// When `elicit` is `None`, all approval requests will be rejected with a
    /// descriptive reason indicating no callback is available.
    pub fn new(elicit: Option<ElicitCallback>) -> Self {
        Self { elicit }
    }
}

impl fmt::Debug for ElicitationApprovalHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ElicitationApprovalHandler")
            .field("has_elicit", &self.elicit.is_some())
            .finish()
    }
}

/// Build a rejected [`ApprovalResult`] with the given reason.
fn rejected(reason: &str) -> ApprovalResult {
    let mut result = ApprovalResult::default();
    result.status = "rejected".to_string();
    result.reason = Some(reason.to_string());
    result
}

/// Build an approved [`ApprovalResult`].
fn approved() -> ApprovalResult {
    let mut result = ApprovalResult::default();
    result.status = "approved".to_string();
    result
}

#[async_trait]
impl ApprovalHandler for ElicitationApprovalHandler {
    async fn request_approval(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalResult, ModuleError> {
        // [D10-001] Python+TS extract the elicit callback per-call from
        // `request.context.data[MCP_ELICIT_KEY]`. In Rust, `ElicitCallback`
        // is a `Box<dyn Fn...>` which cannot be serialized into the
        // `Context<Value>` data map (typed as `HashMap<String, serde_json::Value>`).
        // Therefore we follow the closest possible approximation: if the request
        // has a context, surface a "No context available for elicitation" rejection
        // when no constructor-injected callback is provided, mirroring the Python
        // branch. The constructor-injected `self.elicit` acts as the pre-resolved
        // callback that Python would extract at request time. [D10-001]
        //
        // [D10-002] Return type kept as `Result<ApprovalResult, ModuleError>` for
        // idiomatic Rust. All error paths return `Ok(rejected(...))` so callers
        // always receive an `ApprovalResult` for the elicitation surface; no `Err`
        // is returned from the elicitation logic itself. [D10-002]

        // [D10-005] "No context available for elicitation" rejection branch —
        // matches the Python/TS path where `request.context` is absent.
        // Rust previously only fired this branch when BOTH context and
        // elicit were missing, which made the callback fire even on
        // context-less requests if a constructor-injected callback was
        // present. The handler must NEVER invoke elicitation without a
        // context attached to the request because the elicitation
        // surface is security-gated and the calling identity is carried
        // by `request.context`. Reject unconditionally whenever context
        // is None.
        if request.context.is_none() {
            tracing::debug!("no context attached to approval request, rejecting");
            return Ok(rejected("No context available for elicitation"));
        }

        let elicit = match &self.elicit {
            Some(cb) => cb,
            None => {
                tracing::debug!("no elicitation callback available, rejecting approval");
                return Ok(rejected("No elicitation callback available"));
            }
        };

        let message = format!(
            "Approval required for tool: {}\n\n{}\n\nArguments: {}",
            request.module_id,
            request.description.as_deref().unwrap_or(""),
            request.arguments,
        );

        // [AH-3] Catch panics from the elicit callback so a buggy
        // implementation can't bring down the approval task. Mirrors
        // Python (try/except) and TypeScript (try/catch) which both
        // degrade to `rejected("Elicitation request failed")` on error.
        // futures::FutureExt::catch_unwind handles async panics across
        // .await points; std::panic::catch_unwind cannot.
        use futures::FutureExt;
        let result_outcome = std::panic::AssertUnwindSafe(elicit(message, None))
            .catch_unwind()
            .await;
        let result = match result_outcome {
            Ok(Some(r)) => r,
            Ok(None) => {
                tracing::debug!("elicitation returned no response");
                return Ok(rejected("Elicitation returned no response"));
            }
            Err(_panic) => {
                tracing::debug!("elicit callback panicked");
                return Ok(rejected("Elicitation request failed"));
            }
        };

        match result.action {
            ElicitAction::Accept => Ok(approved()),
            ElicitAction::Decline => Ok(rejected("User action: decline")),
            ElicitAction::Cancel => Ok(rejected("User action: cancel")),
            // [D11-020] Python+TS treat any non-"accept" string as rejected.
            // Rust captures the raw string in Unknown(String) so the reason
            // can be surfaced, matching cross-language semantics. [D11-020]
            ElicitAction::Unknown(raw) => Ok(rejected(&format!("User action: {raw}"))),
        }
    }

    async fn check_approval(&self, _approval_id: &str) -> Result<ApprovalResult, ModuleError> {
        Ok(rejected("Phase B not supported via MCP elicitation"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::ElicitResult;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Create a mock elicit callback that returns the given action.
    fn mock_elicit(action: ElicitAction) -> ElicitCallback {
        Box::new(move |_msg, _schema| {
            let action = action.clone();
            Box::pin(async move {
                Some(ElicitResult {
                    action,
                    content: None,
                })
            })
        })
    }

    /// Create a mock elicit callback that returns `None`.
    fn mock_elicit_none() -> ElicitCallback {
        Box::new(|_msg, _schema| Box::pin(async { None }))
    }

    /// Create a mock [`ApprovalRequest`] for testing.
    ///
    /// Attaches a default `Context` so the request reaches the
    /// elicitation path. [D10-005] requires the handler to reject
    /// unconditionally when `request.context.is_none()`.
    fn test_request() -> ApprovalRequest {
        use apcore::{Context, Identity};
        let mut req = ApprovalRequest::default();
        req.module_id = "test.dangerous_tool".to_string();
        req.arguments = json!({"path": "/etc/passwd"});
        req.description = Some("Delete a system file".to_string());
        req.tags = vec!["destructive".to_string()];
        req.context = Some(Context::new(Identity::new(
            "u1".into(),
            "user".into(),
            vec![],
            Default::default(),
        )));
        req
    }

    // -- request_approval tests -----------------------------------------------

    #[tokio::test]
    async fn test_request_approval_accepted() {
        let handler = ElicitationApprovalHandler::new(Some(mock_elicit(ElicitAction::Accept)));
        let result = handler.request_approval(&test_request()).await.unwrap();
        assert_eq!(result.status, "approved");
        assert!(result.reason.is_none());
    }

    #[tokio::test]
    async fn test_request_approval_declined() {
        let handler = ElicitationApprovalHandler::new(Some(mock_elicit(ElicitAction::Decline)));
        let result = handler.request_approval(&test_request()).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(result.reason.as_deref(), Some("User action: decline"));
    }

    #[tokio::test]
    async fn test_request_approval_cancelled() {
        let handler = ElicitationApprovalHandler::new(Some(mock_elicit(ElicitAction::Cancel)));
        let result = handler.request_approval(&test_request()).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(result.reason.as_deref(), Some("User action: cancel"));
    }

    #[tokio::test]
    async fn test_request_approval_no_callback() {
        // [D10-005] `test_request()` attaches a context, so the
        // "No context available" branch is skipped and we hit the
        // missing-callback branch instead.
        let handler = ElicitationApprovalHandler::new(None);
        let result = handler.request_approval(&test_request()).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("No elicitation callback available")
        );
    }

    #[tokio::test]
    async fn test_request_approval_has_context_no_callback() {
        // When context is present but no callback, falls through to the callback check.
        use apcore::Context;
        let handler = ElicitationApprovalHandler::new(None);
        let mut request = test_request();
        request.context = Some(Context::new(apcore::Identity::new(
            "u1".into(),
            "user".into(),
            vec![],
            Default::default(),
        )));
        let result = handler.request_approval(&request).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("No elicitation callback available")
        );
    }

    #[tokio::test]
    async fn test_request_approval_callback_none() {
        let handler = ElicitationApprovalHandler::new(Some(mock_elicit_none()));
        let result = handler.request_approval(&test_request()).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("Elicitation returned no response")
        );
    }

    // -- elicit callback panic-catching test (Ru-W2) ---------------------------

    #[tokio::test]
    async fn test_elicit_callback_panic_is_caught() {
        // [Ru-W2] The production code already uses `futures::FutureExt::catch_unwind`
        // (lines 118-131). A panicking callback must be caught and degraded to a
        // rejected ApprovalResult, NOT propagate as a panic.
        let panic_cb: ElicitCallback = Box::new(|_msg, _schema| {
            Box::pin(async move {
                panic!("elicit callback panicked intentionally");
            })
        });
        use apcore::{Context, Identity};
        let handler = ElicitationApprovalHandler::new(Some(panic_cb));
        let req = {
            let mut r = ApprovalRequest::default();
            r.module_id = "test_module".to_string();
            r.arguments = serde_json::json!({});
            // [D10-005] Context required so we reach the elicit path.
            r.context = Some(Context::new(Identity::new(
                "u1".into(),
                "user".into(),
                vec![],
                Default::default(),
            )));
            r
        };
        let result = handler.request_approval(&req).await;
        assert!(
            result.is_ok(),
            "panicking callback must not propagate as Err"
        );
        let approval = result.unwrap();
        assert_eq!(
            approval.status, "rejected",
            "panicking callback must yield rejected status"
        );
        assert_eq!(
            approval.reason.as_deref(),
            Some("Elicitation request failed"),
            "panic rejection reason must be 'Elicitation request failed'"
        );
    }

    // -- check_approval tests -------------------------------------------------

    #[tokio::test]
    async fn test_check_approval_always_rejected() {
        let handler = ElicitationApprovalHandler::new(None);
        let result = handler.check_approval("any-id-123").await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("Phase B not supported via MCP elicitation")
        );
    }

    // -- message format test --------------------------------------------------

    #[tokio::test]
    async fn test_approval_message_format() {
        let captured_msg = Arc::new(Mutex::new(String::new()));
        let captured_clone = captured_msg.clone();
        let cb: ElicitCallback = Box::new(move |msg, _schema| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = msg;
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });

        let handler = ElicitationApprovalHandler::new(Some(cb));
        let request = test_request();
        handler.request_approval(&request).await.unwrap();

        let msg = captured_msg.lock().unwrap().clone();
        assert!(
            msg.contains("test.dangerous_tool"),
            "message should contain module_id"
        );
        assert!(
            msg.contains("Delete a system file"),
            "message should contain description"
        );
        assert!(
            msg.contains("/etc/passwd"),
            "message should contain arguments"
        );
    }

    // -- Issue D10-001/D10-002: context-not-present rejection branch ----------

    #[tokio::test]
    async fn test_request_approval_no_context_no_callback_returns_rejected() {
        // D10-002: When no context AND no callback, result must be Ok(rejected),
        // not Err. Reason must indicate "No context available for elicitation".
        let handler = ElicitationApprovalHandler::new(None);
        let request = {
            let mut r = ApprovalRequest::default();
            r.module_id = "test.tool".to_string();
            r.arguments = json!({});
            r
        };
        let result = handler.request_approval(&request).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("No context available for elicitation"),
            "should return 'No context available for elicitation' when context is None and no callback"
        );
    }

    // -- Issue D10-005: unconditional rejection on missing context ------------

    #[tokio::test]
    async fn test_request_approval_no_context_with_callback_returns_rejected() {
        // [D10-005] Python and TypeScript unconditionally reject when
        // `request.context` is None — the elicit callback is NEVER
        // invoked because the security-gated approval handler should not
        // act without an identity/context attached to the request. Rust
        // previously special-cased the branch and only rejected when
        // both context and elicit were absent, so it would call the
        // callback for `context == None` if a constructor-injected
        // `elicit` was present. This is a cross-language behavioral
        // divergence on a security-sensitive surface.
        use std::sync::atomic::{AtomicBool, Ordering};
        let callback_called = Arc::new(AtomicBool::new(false));
        let flag = callback_called.clone();
        let cb: ElicitCallback = Box::new(move |_msg, _schema| {
            let flag = flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });
        let handler = ElicitationApprovalHandler::new(Some(cb));
        let request = {
            let mut r = ApprovalRequest::default();
            r.module_id = "test.tool".to_string();
            r.arguments = json!({});
            // context intentionally None
            r
        };
        let result = handler.request_approval(&request).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("No context available for elicitation"),
            "missing context must reject regardless of callback availability"
        );
        assert!(
            !callback_called.load(Ordering::SeqCst),
            "elicit callback must NOT be invoked when context is None"
        );
    }

    // -- Issue D11-020: Unknown action variant maps to rejected ---------------

    #[tokio::test]
    async fn test_unknown_action_maps_to_rejected_with_reason() {
        // D11-020: Unknown action strings must map to rejected with the
        // raw action string in the reason.
        let action_str = "unknown-action";
        let action = ElicitAction::Unknown(action_str.to_string());
        let cb: ElicitCallback = Box::new(move |_msg, _schema| {
            let a = action.clone();
            Box::pin(async move {
                Some(ElicitResult {
                    action: a,
                    content: None,
                })
            })
        });
        let handler = ElicitationApprovalHandler::new(Some(cb));
        let result = handler.request_approval(&test_request()).await.unwrap();
        assert_eq!(result.status, "rejected");
        assert_eq!(
            result.reason.as_deref(),
            Some("User action: unknown-action"),
            "unknown action must be rejected with 'User action: <raw_string>'"
        );
    }

    #[test]
    fn test_unknown_action_deserializes_from_unknown_string() {
        // D11-020: Unknown action strings must not cause deserialization error.
        let raw = r#"{"action": "unknown-action"}"#;
        let result: ElicitResult = serde_json::from_str(raw).unwrap();
        assert_eq!(
            result.action,
            ElicitAction::Unknown("unknown-action".to_string())
        );
    }

    // -- Issue D11-019 partial: arguments formatted as JSON (not debug repr) -

    #[tokio::test]
    async fn test_approval_message_arguments_formatted_as_json() {
        // D11-019: Arguments must be formatted as JSON (e.g. {"key":"val"}),
        // not as Rust debug repr. serde_json's Display uses JSON format — correct.
        let captured_msg = Arc::new(Mutex::new(String::new()));
        let captured_clone = captured_msg.clone();
        let cb: ElicitCallback = Box::new(move |msg, _schema| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = msg;
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                })
            })
        });

        use apcore::{Context, Identity};
        let handler = ElicitationApprovalHandler::new(Some(cb));
        let request = {
            let mut r = ApprovalRequest::default();
            r.module_id = "test.tool".to_string();
            r.arguments = json!({"key": "val"});
            // [D10-005] Context required so we reach the elicit path.
            r.context = Some(Context::new(Identity::new(
                "u1".into(),
                "user".into(),
                vec![],
                Default::default(),
            )));
            r
        };
        handler.request_approval(&request).await.unwrap();

        let msg = captured_msg.lock().unwrap().clone();
        // JSON format: {"key":"val"} — must contain the key in JSON, not Rust debug
        assert!(
            msg.contains("\"key\""),
            "arguments must be JSON-formatted in message: {msg}"
        );
        assert!(
            msg.contains("\"val\""),
            "arguments must be JSON-formatted in message: {msg}"
        );
    }

    // -- Debug impl test ------------------------------------------------------

    #[test]
    fn test_debug_with_callback() {
        let handler = ElicitationApprovalHandler::new(Some(mock_elicit(ElicitAction::Accept)));
        let debug_str = format!("{:?}", handler);
        assert!(debug_str.contains("has_elicit: true"));
    }

    #[test]
    fn test_debug_without_callback() {
        let handler = ElicitationApprovalHandler::new(None);
        let debug_str = format!("{:?}", handler);
        assert!(debug_str.contains("has_elicit: false"));
    }
}
