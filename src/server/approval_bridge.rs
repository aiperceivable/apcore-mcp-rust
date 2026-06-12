//! ApprovalBridge: exposes __apcore_approval_check as an MCP meta-tool.
//!
//! Symmetric with AsyncTaskBridge.

use apcore::approval::{ApprovalHandler, ApprovalResult};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::server::types::Tool;

pub const APPROVAL_META_TOOL_NAMES: &[&str] = &["__apcore_approval_check"];

pub struct ApprovalBridge {
    handler: Arc<dyn ApprovalHandler>,
}

impl ApprovalBridge {
    pub fn new(handler: Arc<dyn ApprovalHandler>) -> Self {
        Self { handler }
    }

    pub fn is_meta_tool(name: &str) -> bool {
        APPROVAL_META_TOOL_NAMES.contains(&name)
    }

    pub fn build_meta_tools() -> Vec<Tool> {
        vec![Tool {
            name: "__apcore_approval_check".to_string(),
            description: "Poll the status of a pending approval request. Returns \
                {approval_id, status, reason}. status is 'pending', 'approved', or 'rejected'. \
                When 'approved', retry the original tool call with _meta.approvalId set to \
                this approval_id."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "approval_id": {
                        "type": "string",
                        "description": "The approval_id from the APPROVAL_PENDING response"
                    }
                },
                "required": ["approval_id"],
                "additionalProperties": false
            }),
            annotations: None,
            meta: Some(json!({ "reserved": true })),
        }]
    }

    pub async fn handle_meta_tool(&self, name: &str, arguments: &Value) -> (Value, bool) {
        if name == "__apcore_approval_check" {
            self.handle_check(arguments).await
        } else {
            (
                json!({"error": format!("Unknown approval meta-tool: {name}")}),
                true,
            )
        }
    }

    async fn handle_check(&self, args: &Value) -> (Value, bool) {
        let approval_id = match args.get("approval_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                return (
                    json!({"error": "approval_id is required", "is_error": true}),
                    true,
                )
            }
        };

        let result: ApprovalResult = match self.handler.check_approval(&approval_id).await {
            Ok(r) => r,
            Err(e) => {
                return (
                    json!({"error": format!("store error: {e}"), "is_error": true}),
                    true,
                )
            }
        };

        let mut payload = json!({
            "approval_id": approval_id,
            "status": result.status,
        });
        if let Some(r) = result.reason {
            payload["reason"] = json!(r);
        }
        (payload, false)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::approval::{ApprovalHandler, ApprovalRequest, ApprovalResult};
    use apcore::errors::ModuleError;
    use async_trait::async_trait;
    use serde_json::json;

    // ── Stub handler for testing ─────────────────────────────────────────────

    #[derive(Debug)]
    struct StubApprovalHandler {
        status: String,
        reason: Option<String>,
    }

    impl StubApprovalHandler {
        fn new(status: &str, reason: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                status: status.to_string(),
                reason: reason.map(|s| s.to_string()),
            })
        }
    }

    #[async_trait]
    impl ApprovalHandler for StubApprovalHandler {
        async fn request_approval(
            &self,
            _request: &ApprovalRequest,
        ) -> Result<ApprovalResult, ModuleError> {
            let mut result = ApprovalResult::default();
            result.status = self.status.clone();
            result.reason = self.reason.clone();
            Ok(result)
        }

        async fn check_approval(&self, _approval_id: &str) -> Result<ApprovalResult, ModuleError> {
            let mut result = ApprovalResult::default();
            result.status = self.status.clone();
            result.reason = self.reason.clone();
            Ok(result)
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn is_meta_tool_returns_true_for_approval_check() {
        assert!(ApprovalBridge::is_meta_tool("__apcore_approval_check"));
    }

    #[test]
    fn is_meta_tool_returns_false_for_other_names() {
        assert!(!ApprovalBridge::is_meta_tool("__apcore_task_submit"));
        assert!(!ApprovalBridge::is_meta_tool("math.add"));
        assert!(!ApprovalBridge::is_meta_tool(""));
    }

    #[test]
    fn build_meta_tools_returns_correct_schema() {
        let tools = ApprovalBridge::build_meta_tools();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name, "__apcore_approval_check");
        assert_eq!(tool.input_schema["type"], "object");

        // Verify required fields
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("approval_id")));
        // additionalProperties: false
        assert_eq!(tool.input_schema["additionalProperties"], false);
        // meta has reserved: true
        assert_eq!(tool.meta.as_ref().unwrap()["reserved"], true);
    }

    #[tokio::test]
    async fn handle_meta_tool_check_pending() {
        let bridge = ApprovalBridge::new(StubApprovalHandler::new("pending", None));
        let (result, is_error) = bridge
            .handle_meta_tool(
                "__apcore_approval_check",
                &json!({"approval_id": "abc-123"}),
            )
            .await;
        assert!(!is_error);
        assert_eq!(result["approval_id"].as_str(), Some("abc-123"));
        assert_eq!(result["status"].as_str(), Some("pending"));
        assert!(result.get("reason").is_none());
    }

    #[tokio::test]
    async fn handle_meta_tool_check_approved() {
        let bridge = ApprovalBridge::new(StubApprovalHandler::new("approved", None));
        let (result, is_error) = bridge
            .handle_meta_tool(
                "__apcore_approval_check",
                &json!({"approval_id": "xyz-456"}),
            )
            .await;
        assert!(!is_error);
        assert_eq!(result["approval_id"].as_str(), Some("xyz-456"));
        assert_eq!(result["status"].as_str(), Some("approved"));
    }

    #[tokio::test]
    async fn handle_meta_tool_check_rejected_with_reason() {
        let bridge = ApprovalBridge::new(StubApprovalHandler::new("rejected", Some("not allowed")));
        let (result, is_error) = bridge
            .handle_meta_tool(
                "__apcore_approval_check",
                &json!({"approval_id": "xyz-789"}),
            )
            .await;
        assert!(!is_error);
        assert_eq!(result["status"].as_str(), Some("rejected"));
        assert_eq!(result["reason"].as_str(), Some("not allowed"));
    }

    #[tokio::test]
    async fn handle_meta_tool_missing_approval_id_returns_is_error() {
        let bridge = ApprovalBridge::new(StubApprovalHandler::new("pending", None));
        let (result, is_error) = bridge
            .handle_meta_tool("__apcore_approval_check", &json!({}))
            .await;
        assert!(is_error, "missing approval_id must set is_error");
        assert!(result.get("error").is_some());
    }

    #[tokio::test]
    async fn handle_meta_tool_empty_approval_id_returns_is_error() {
        let bridge = ApprovalBridge::new(StubApprovalHandler::new("pending", None));
        let (result, is_error) = bridge
            .handle_meta_tool("__apcore_approval_check", &json!({"approval_id": ""}))
            .await;
        assert!(is_error, "empty approval_id must set is_error");
        assert!(result.get("error").is_some());
    }

    #[tokio::test]
    async fn handle_meta_tool_unknown_tool_returns_is_error() {
        let bridge = ApprovalBridge::new(StubApprovalHandler::new("pending", None));
        let (result, is_error) = bridge
            .handle_meta_tool("__apcore_something_else", &json!({"approval_id": "x"}))
            .await;
        assert!(is_error);
        assert!(result.get("error").is_some());
    }
}
