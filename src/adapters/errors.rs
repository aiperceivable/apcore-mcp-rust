//! ErrorMapper — translates apcore errors to MCP error responses.
//!
//! Converts [`apcore::errors::ModuleError`] into [`McpErrorResponse`], sanitizing
//! internal and ACL error codes, formatting validation errors, handling
//! approval-related codes, and attaching AI guidance fields in camelCase.

use apcore::errors::{ErrorCode as ApcoreErrorCode, ModuleError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error codes that represent internal failures — sanitized to a generic
/// message before returning to the MCP client.
const INTERNAL_ERROR_CODES: &[ApcoreErrorCode] = &[
    ApcoreErrorCode::CallDepthExceeded,
    ApcoreErrorCode::CircularCall,
    ApcoreErrorCode::CallFrequencyExceeded,
];

/// Error codes that require detail sanitization (hide sensitive info).
const SANITIZED_ERROR_CODES: &[ApcoreErrorCode] = &[ApcoreErrorCode::ACLDenied];

/// Structured MCP error response.
///
/// Wire format uses camelCase keys to match MCP/TypeScript convention.
/// Optional fields are omitted when `None`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpErrorResponse {
    pub is_error: bool,
    pub error_type: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_fixable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Maps apcore execution errors to MCP-compatible error content.
pub struct ErrorMapper;

impl ErrorMapper {
    /// Convert any [`std::error::Error`] into an [`McpErrorResponse`].
    ///
    /// Attempts to downcast to [`ModuleError`] and delegates to the typed
    /// [`Self::to_mcp_error`] fast path. Unknown error types are mapped to an
    /// `GENERAL_INTERNAL_ERROR` envelope, matching Python+TS fallback behavior (EM-6).
    ///
    /// Note: downcast requires `'static` — the bound is placed on this signature.
    pub fn to_mcp_error_any(err: &(dyn std::error::Error + 'static)) -> McpErrorResponse {
        // Attempt fast-path downcast to ModuleError
        if let Some(module_err) = err.downcast_ref::<ModuleError>() {
            return Self::to_mcp_error(module_err);
        }
        // Unknown error type → canonical GENERAL_INTERNAL_ERROR (EM-6).
        // [D10-002] Wire string is the literal "GENERAL_INTERNAL_ERROR" so MCP
        // clients can branch on errorType === "GENERAL_INTERNAL_ERROR" portably.
        internal_error_response()
    }

    /// Convert an apcore [`ModuleError`] into an [`McpErrorResponse`].
    ///
    /// Internal errors are sanitized to avoid leaking implementation details.
    /// ACL errors hide caller information. Validation errors are reformatted.
    /// Approval-related errors receive special handling.
    pub fn to_mcp_error(error: &ModuleError) -> McpErrorResponse {
        let code = error.code;
        let error_type = error_code_to_string(&code);

        // Internal codes → generic message, no details
        if INTERNAL_ERROR_CODES.contains(&code) {
            return McpErrorResponse {
                is_error: true,
                error_type,
                message: "Internal error occurred".to_string(),
                details: None,
                retryable: None,
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
        }

        // ACL codes → "Access denied", no details
        if SANITIZED_ERROR_CODES.contains(&code) {
            return McpErrorResponse {
                is_error: true,
                error_type,
                message: "Access denied".to_string(),
                details: None,
                retryable: None,
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
        }

        // Schema validation → format field-level errors
        if code == ApcoreErrorCode::SchemaValidationError {
            let formatted = format_validation_errors(&error.details);
            let details_value = if error.details.is_empty() {
                None
            } else {
                Some(serde_json::to_value(&error.details).unwrap_or(Value::Null))
            };
            let mut resp = McpErrorResponse {
                is_error: true,
                error_type,
                message: formatted,
                details: details_value,
                retryable: None,
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
            attach_ai_guidance(error, &mut resp);
            return resp;
        }

        // Approval pending → narrow details to only approvalId
        if code == ApcoreErrorCode::ApprovalPending {
            let narrowed = error
                .details
                .get("approval_id")
                .map(|v| serde_json::json!({ "approvalId": v }));
            let mut resp = McpErrorResponse {
                is_error: true,
                error_type,
                message: error.message.clone(),
                details: narrowed,
                retryable: None,
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
            attach_ai_guidance(error, &mut resp);
            return resp;
        }

        // Approval timeout → pass through with retryable=true
        if code == ApcoreErrorCode::ApprovalTimeout {
            let details_value = if error.details.is_empty() {
                None
            } else {
                Some(serde_json::to_value(&error.details).unwrap_or(Value::Null))
            };
            let mut resp = McpErrorResponse {
                is_error: true,
                error_type,
                message: error.message.clone(),
                details: details_value,
                retryable: Some(true),
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
            attach_ai_guidance(error, &mut resp);
            return resp;
        }

        // Approval denied → extract reason
        if code == ApcoreErrorCode::ApprovalDenied {
            let reason = error.details.get("reason");
            let details_value = match reason {
                Some(r) => Some(serde_json::json!({ "reason": r })),
                None => {
                    if error.details.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_value(&error.details).unwrap_or(Value::Null))
                    }
                }
            };
            let mut resp = McpErrorResponse {
                is_error: true,
                error_type,
                message: error.message.clone(),
                details: details_value,
                retryable: None,
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
            attach_ai_guidance(error, &mut resp);
            return resp;
        }

        // Execution cancelled → specific message with retryable=true
        if code == ApcoreErrorCode::ExecutionCancelled {
            let mut resp = McpErrorResponse {
                is_error: true,
                error_type,
                message: "Execution was cancelled".to_string(),
                details: None,
                retryable: Some(true),
                ai_guidance: None,
                user_fixable: None,
                suggestion: None,
            };
            attach_ai_guidance(error, &mut resp);
            return resp;
        }

        // CONFIG_ENV_MAP_CONFLICT → include env_var from details
        if error_type == "CONFIG_ENV_MAP_CONFLICT" {
            let detail_val = error
                .details
                .get("env_var")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let message = format!("Config env map conflict: {}", detail_val);
            return build_detail_response(error, error_type, message);
        }

        // PIPELINE_ABORT → include step name from details
        if error_type == "PIPELINE_ABORT" {
            let detail_val = error
                .details
                .get("step")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let message = format!("Pipeline aborted at step: {}", detail_val);
            return build_detail_response(error, error_type, message);
        }

        // STEP_NOT_FOUND, VERSION_INCOMPATIBLE → handled by default passthrough below.

        // Async-task capacity: map `TaskLimitExceeded` to a retryable
        // envelope with an explicit agent-facing message, mirroring the
        // MCP async-task-bridge spec (`ASYNC_CAPACITY_EXCEEDED`).
        if code == ApcoreErrorCode::TaskLimitExceeded {
            let mut resp = build_detail_response(error, error_type, error.message.clone());
            if resp.retryable.is_none() {
                resp.retryable = Some(true);
            }
            // [D11-106] This SDK-side `ai_guidance` fallback is intentional:
            // it gives the agent an actionable hint when the underlying
            // `ModuleError` did not populate one. Python and TypeScript
            // currently rely on the upstream apcore error to carry the
            // guidance and have no equivalent fallback. The cross-language
            // alignment is tracked as a follow-up PR; do not remove this
            // block without coordinating across all three SDKs.
            if resp.ai_guidance.is_none() {
                resp.ai_guidance = Some(
                    "AsyncTaskManager has reached its max_tasks cap. \
                     Wait for in-flight tasks to complete, or cancel \
                     tasks via __apcore_task_cancel before retrying."
                        .to_string(),
                );
            }
            return resp;
        }

        // apcore 0.20.0 (sync alignment A-001): surface circuit-breaker
        // rejections with retryable=true plus a recovery hint so the AI
        // orchestrator backs off until the recovery window elapses. The
        // error already carries a per-module `ai_guidance` populated by
        // `apcore::errors::ErrorBuilder::circuit_breaker_open` — we mirror
        // it onto the MCP envelope without overwriting.
        if code == ApcoreErrorCode::CircuitBreakerOpen {
            let mut resp = build_detail_response(error, error_type, error.message.clone());
            if resp.retryable.is_none() {
                resp.retryable = Some(true);
            }
            if resp.ai_guidance.is_none() {
                resp.ai_guidance = Some(
                    "Module's circuit breaker is OPEN — repeated failures have \
                     tripped the breaker. Wait until the recovery window elapses, \
                     then retry; the breaker will move to HALF_OPEN and accept a \
                     trial call."
                        .to_string(),
                );
            }
            return resp;
        }

        // Default: pass through message and details
        let details_value = if error.details.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&error.details).unwrap_or(Value::Null))
        };
        let mut resp = McpErrorResponse {
            is_error: true,
            error_type,
            message: error.message.clone(),
            details: details_value,
            retryable: None,
            ai_guidance: None,
            user_fixable: None,
            suggestion: None,
        };
        attach_ai_guidance(error, &mut resp);
        resp
    }
}

/// Attach AI guidance fields from the error to the response.
///
/// Reads snake_case fields from the apcore error and writes camelCase
/// keys to the MCP result. Skips `None` values and does not overwrite
/// existing (already-set) fields.
fn attach_ai_guidance(error: &ModuleError, resp: &mut McpErrorResponse) {
    if resp.retryable.is_none() {
        resp.retryable = error.retryable;
    }
    if resp.ai_guidance.is_none() {
        resp.ai_guidance = error.ai_guidance.clone();
    }
    if resp.user_fixable.is_none() {
        resp.user_fixable = error.user_fixable;
    }
    if resp.suggestion.is_none() {
        resp.suggestion = error.suggestion.clone();
    }
}

/// Format schema validation field-level errors into a readable message.
///
/// Extracts the `"errors"` array from details and formats each entry as
/// `"field: message"`. Returns a fallback if no errors are present.
fn format_validation_errors(details: &std::collections::HashMap<String, Value>) -> String {
    let errors = match details.get("errors") {
        Some(Value::Array(arr)) => arr,
        _ => return "Schema validation failed".to_string(),
    };

    if errors.is_empty() {
        return "Schema validation failed".to_string();
    }

    let lines: Vec<String> = errors
        .iter()
        .map(|e| {
            let field = e.get("field").and_then(Value::as_str).unwrap_or("unknown");
            let msg = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("invalid");
            format!("  {field}: {msg}")
        })
        .collect();

    format!("Schema validation failed:\n{}", lines.join("\n"))
}

/// Build an MCP error response with details passthrough and AI guidance.
///
/// Used by error-code-specific branches that extract a detail field into
/// a custom message but otherwise follow the same structure.
fn build_detail_response(
    error: &ModuleError,
    error_type: String,
    message: String,
) -> McpErrorResponse {
    let details_value = if error.details.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&error.details).unwrap_or(Value::Null))
    };
    let mut resp = McpErrorResponse {
        is_error: true,
        error_type,
        message,
        details: details_value,
        retryable: None,
        ai_guidance: None,
        user_fixable: None,
        suggestion: None,
    };
    attach_ai_guidance(error, &mut resp);
    resp
}

/// Convert an apcore [`ApcoreErrorCode`] to its SCREAMING_SNAKE_CASE string.
fn error_code_to_string(code: &ApcoreErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{code:?}"))
}

// ---- MCP Error Formatter for apcore ErrorFormatterRegistry (§8.8) ----------

use apcore::error_formatter::{ErrorFormatter as ApcoreErrorFormatter, ErrorFormatterRegistry};

/// MCP-specific error formatter for the apcore ErrorFormatterRegistry.
///
/// Wraps [`ErrorMapper::to_mcp_error`] for use with the shared registry.
pub struct McpErrorFormatter;

impl ApcoreErrorFormatter for McpErrorFormatter {
    fn format(&self, error: &ModuleError, _context: Option<&dyn std::any::Any>) -> Value {
        serde_json::to_value(ErrorMapper::to_mcp_error(error)).unwrap_or_else(|_| error.to_dict())
    }
}

/// Register the MCP error formatter with apcore's ErrorFormatterRegistry.
///
/// Safe to call multiple times — ignores duplicate registration.
pub fn register_mcp_formatter() {
    if !ErrorFormatterRegistry::is_registered("mcp") {
        let _ = ErrorFormatterRegistry::register("mcp", Box::new(McpErrorFormatter));
    }
}

/// Canonical `GENERAL_INTERNAL_ERROR` envelope for non-`ModuleError` fallback (EM-6).
///
/// All three SDKs (Python, TypeScript, Rust) emit byte-identical envelopes so MCP
/// clients can branch on `errorType === "GENERAL_INTERNAL_ERROR"` portably. See
/// `apcore-mcp/docs/features/error-mapper.md` (EM-6).
pub fn internal_error_response() -> McpErrorResponse {
    McpErrorResponse {
        is_error: true,
        error_type: "GENERAL_INTERNAL_ERROR".to_string(),
        message: "Internal error occurred".to_string(),
        details: None,
        retryable: None,
        ai_guidance: None,
        user_fixable: None,
        suggestion: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Helper: create a basic ModuleError with the given code and message.
    fn make_error(code: ApcoreErrorCode, message: &str) -> ModuleError {
        ModuleError::new(code, message)
    }

    /// Helper: create a ModuleError with details.
    fn make_error_with_details(
        code: ApcoreErrorCode,
        message: &str,
        details: HashMap<String, Value>,
    ) -> ModuleError {
        ModuleError::new(code, message).with_details(details)
    }

    // ---- Internal error sanitization ----

    #[test]
    fn test_internal_error_sanitized() {
        let err = make_error(
            ApcoreErrorCode::CallDepthExceeded,
            "depth 42 exceeded limit",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "CALL_DEPTH_EXCEEDED");
        assert_eq!(resp.message, "Internal error occurred");
        assert!(resp.details.is_none());
    }

    #[test]
    fn test_circular_call_sanitized() {
        let err = make_error(ApcoreErrorCode::CircularCall, "a -> b -> a");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "CIRCULAR_CALL");
        assert_eq!(resp.message, "Internal error occurred");
        assert!(resp.details.is_none());
    }

    #[test]
    fn test_call_frequency_sanitized() {
        let err = make_error(
            ApcoreErrorCode::CallFrequencyExceeded,
            "module called 100 times",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "CALL_FREQUENCY_EXCEEDED");
        assert_eq!(resp.message, "Internal error occurred");
        assert!(resp.details.is_none());
    }

    // ---- ACL denied sanitization ----

    #[test]
    fn test_acl_denied_sanitized() {
        let mut details = HashMap::new();
        details.insert(
            "caller_id".to_string(),
            Value::String("secret-caller".to_string()),
        );
        let err = make_error_with_details(
            ApcoreErrorCode::ACLDenied,
            "caller X denied access to Y",
            details,
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "ACL_DENIED");
        assert_eq!(resp.message, "Access denied");
        assert!(resp.details.is_none(), "ACL details should be stripped");
    }

    // ---- Schema validation formatting ----

    #[test]
    fn test_schema_validation_formatted() {
        let errors_arr = serde_json::json!([
            {"field": "name", "message": "required"},
            {"field": "age", "message": "must be positive"}
        ]);
        let mut details = HashMap::new();
        details.insert("errors".to_string(), errors_arr);
        let err = make_error_with_details(
            ApcoreErrorCode::SchemaValidationError,
            "Validation failed",
            details,
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "SCHEMA_VALIDATION_ERROR");
        assert!(resp.message.contains("Schema validation failed:"));
        assert!(resp.message.contains("name: required"));
        assert!(resp.message.contains("age: must be positive"));
        assert!(resp.details.is_some());
    }

    #[test]
    fn test_schema_validation_empty_errors() {
        let mut details = HashMap::new();
        details.insert("errors".to_string(), serde_json::json!([]));
        let err = make_error_with_details(
            ApcoreErrorCode::SchemaValidationError,
            "Validation failed",
            details,
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.message, "Schema validation failed");
    }

    // ---- Approval pending ----

    #[test]
    fn test_approval_pending_narrowed() {
        let mut details = HashMap::new();
        details.insert(
            "approval_id".to_string(),
            Value::String("abc-123".to_string()),
        );
        details.insert(
            "module_id".to_string(),
            Value::String("secret.module".to_string()),
        );
        let err = make_error_with_details(
            ApcoreErrorCode::ApprovalPending,
            "Awaiting approval",
            details,
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "APPROVAL_PENDING");
        assert_eq!(resp.message, "Awaiting approval");
        let d = resp.details.unwrap();
        assert_eq!(d.get("approvalId").unwrap().as_str().unwrap(), "abc-123");
        // module_id should NOT be present
        assert!(d.get("module_id").is_none());
    }

    // ---- Approval timeout ----

    #[test]
    fn test_approval_timeout_retryable() {
        let err = make_error(ApcoreErrorCode::ApprovalTimeout, "Timed out waiting");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "APPROVAL_TIMEOUT");
        assert_eq!(resp.retryable, Some(true));
    }

    // ---- Approval denied ----

    #[test]
    fn test_approval_denied_reason() {
        let mut details = HashMap::new();
        details.insert(
            "reason".to_string(),
            Value::String("Policy violation".to_string()),
        );
        details.insert("module_id".to_string(), Value::String("mod.x".to_string()));
        let err =
            make_error_with_details(ApcoreErrorCode::ApprovalDenied, "Approval denied", details);
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "APPROVAL_DENIED");
        let d = resp.details.unwrap();
        assert_eq!(
            d.get("reason").unwrap().as_str().unwrap(),
            "Policy violation"
        );
    }

    // ---- AI guidance fields ----

    #[test]
    fn test_ai_guidance_fields() {
        let err = ModuleError::new(ApcoreErrorCode::ModuleExecuteError, "something broke")
            .with_retryable(true)
            .with_ai_guidance("Try reducing batch size")
            .with_suggestion("Use smaller input");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.retryable, Some(true));
        assert_eq!(resp.ai_guidance.as_deref(), Some("Try reducing batch size"));
        assert_eq!(resp.suggestion.as_deref(), Some("Use smaller input"));
    }

    #[test]
    fn test_ai_guidance_none_omitted() {
        // MODULE_EXECUTE_ERROR has no policy entry — all guidance fields stay None.
        let err = make_error(ApcoreErrorCode::ModuleExecuteError, "execute failed");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.retryable.is_none());
        assert!(resp.ai_guidance.is_none());
        assert!(resp.user_fixable.is_none());
        assert!(resp.suggestion.is_none());

        // Verify they are omitted from JSON
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("retryable").is_none());
        assert!(json.get("aiGuidance").is_none());
        assert!(json.get("userFixable").is_none());
        assert!(json.get("suggestion").is_none());
    }

    // ---- Execution cancelled ----

    #[test]
    fn test_execution_cancelled() {
        let err = make_error(ApcoreErrorCode::ExecutionCancelled, "user cancelled");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "EXECUTION_CANCELLED");
        assert_eq!(resp.message, "Execution was cancelled");
        assert!(resp.details.is_none());
        assert_eq!(resp.retryable, Some(true));
    }

    // ---- New error code handling (string-based matching) ----
    //
    // CONFIG_ENV_MAP_CONFLICT, PIPELINE_ABORT, STEP_NOT_FOUND, and
    // VERSION_INCOMPATIBLE are handled via string matching on the
    // serialized error code. Tests for CONFIG_ENV_MAP_CONFLICT and
    // PIPELINE_ABORT require the apcore crate to expose those enum
    // variants. STEP_NOT_FOUND and VERSION_INCOMPATIBLE fall through
    // to the default passthrough and are implicitly covered by
    // test_unknown_error_passthrough.

    // ---- Unknown / passthrough ----

    #[test]
    fn test_unknown_error_passthrough() {
        let mut details = HashMap::new();
        details.insert(
            "module_id".to_string(),
            Value::String("core.math".to_string()),
        );
        let err = make_error_with_details(
            ApcoreErrorCode::ModuleExecuteError,
            "division by zero",
            details,
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "MODULE_EXECUTE_ERROR");
        assert_eq!(resp.message, "division by zero");
        assert!(resp.details.is_some());
    }

    // ---- MCP Error Formatter ----

    #[test]
    fn test_mcp_error_formatter_format() {
        let error = ModuleError::new(ApcoreErrorCode::GeneralInternalError, "test");
        let formatter = McpErrorFormatter;
        let result = ApcoreErrorFormatter::format(&formatter, &error, None);
        assert!(result.is_object());
        assert_eq!(result.get("isError").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn test_register_mcp_formatter_idempotent() {
        register_mcp_formatter();
        register_mcp_formatter(); // Should not panic
    }

    #[test]
    fn test_register_mcp_formatter_naming_parity() {
        // Regression for [A-001]: function must be named `register_mcp_formatter`
        // (no `_error_` infix), matching the Python and TypeScript SDKs.
        // This test references the canonical name; if the function is renamed
        // back to `register_mcp_error_formatter`, this won't compile.
        register_mcp_formatter();
    }

    // ---- camelCase output keys ----

    #[test]
    fn test_output_keys_camel_case() {
        let err = ModuleError::new(ApcoreErrorCode::ModuleTimeout, "timed out")
            .with_retryable(true)
            .with_ai_guidance("increase timeout");
        let resp = ErrorMapper::to_mcp_error(&err);
        let json = serde_json::to_value(&resp).unwrap();

        // Check camelCase keys
        assert!(json.get("isError").is_some());
        assert!(json.get("errorType").is_some());
        assert!(json.get("message").is_some());
        assert!(json.get("aiGuidance").is_some());

        // Ensure snake_case keys are NOT present
        assert!(json.get("is_error").is_none());
        assert!(json.get("error_type").is_none());
        assert!(json.get("ai_guidance").is_none());
        assert!(json.get("user_fixable").is_none());
    }

    // ---- to_mcp_error_any ----

    #[test]
    fn test_to_mcp_error_any_with_io_error_returns_general_internal_error() {
        // [D10-002 / EM-6] Arbitrary errors (not ModuleError) must fall back to
        // GENERAL_INTERNAL_ERROR (not INTERNAL_ERROR) so MCP clients can branch on
        // errorType === "GENERAL_INTERNAL_ERROR" portably across all three SDKs.
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let resp = ErrorMapper::to_mcp_error_any(&io_err);
        assert!(resp.is_error);
        assert_eq!(resp.error_type, "GENERAL_INTERNAL_ERROR");
        assert_eq!(resp.message, "Internal error occurred");
        assert!(resp.details.is_none());
    }

    #[test]
    fn test_internal_error_response_helper_envelope() {
        // [D10-001 / EM-6] internal_error_response() returns the canonical envelope.
        let envelope = internal_error_response();
        assert!(envelope.is_error);
        assert_eq!(envelope.error_type, "GENERAL_INTERNAL_ERROR");
        assert_eq!(envelope.message, "Internal error occurred");
        assert!(envelope.details.is_none());
        assert!(envelope.retryable.is_none());
        assert!(envelope.ai_guidance.is_none());
        assert!(envelope.user_fixable.is_none());
        assert!(envelope.suggestion.is_none());
    }

    #[test]
    fn test_to_mcp_error_any_ignores_input_contents() {
        // [D10-002 / EM-6] The helper deliberately ignores the error's class and message
        // (security: avoid leaking server-side state to MCP clients).
        let secret_err = std::io::Error::other("secret-detail-XYZ");
        let resp = ErrorMapper::to_mcp_error_any(&secret_err);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("secret-detail-XYZ"),
            "to_mcp_error_any leaked input message: {json}"
        );
        assert_eq!(resp.error_type, "GENERAL_INTERNAL_ERROR");
    }

    #[test]
    fn test_to_mcp_error_any_with_module_error_delegates() {
        // [D10-009] When the error IS a ModuleError, to_mcp_error_any delegates to to_mcp_error.
        let module_err = ModuleError::new(ApcoreErrorCode::ModuleNotFound, "no such module");
        let resp_typed = ErrorMapper::to_mcp_error(&module_err);
        let resp_any = ErrorMapper::to_mcp_error_any(&module_err);
        assert_eq!(resp_typed.error_type, resp_any.error_type);
        assert_eq!(resp_typed.message, resp_any.message);
    }

    // ---- userFixable pass-through for DEPENDENCY_*, VERSION_CONSTRAINT_INVALID,
    // BINDING_* — apcore 0.24 resolves user_fixable at construction via
    // user_fixable_for_code; the bridge passes it through attach_ai_guidance. ----

    #[test]
    fn test_user_fixable_dependency_not_found_passes_through() {
        let err = ModuleError::new(ApcoreErrorCode::DependencyNotFound, "missing dependency");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "DEPENDENCY_NOT_FOUND");
        assert_eq!(resp.user_fixable, Some(true));
    }

    #[test]
    fn test_user_fixable_dependency_version_mismatch_passes_through() {
        let err = ModuleError::new(
            ApcoreErrorCode::DependencyVersionMismatch,
            "version mismatch",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "DEPENDENCY_VERSION_MISMATCH");
        assert_eq!(resp.user_fixable, Some(true));
    }

    #[test]
    fn test_user_fixable_version_constraint_invalid_passes_through() {
        let err = ModuleError::new(
            ApcoreErrorCode::VersionConstraintInvalid,
            "invalid constraint",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "VERSION_CONSTRAINT_INVALID");
        assert_eq!(resp.user_fixable, Some(true));
    }

    #[test]
    fn test_user_fixable_binding_schema_inference_failed_passes_through() {
        let err = ModuleError::new(
            ApcoreErrorCode::BindingSchemaInferenceFailed,
            "could not infer schema",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "BINDING_SCHEMA_INFERENCE_FAILED");
        assert_eq!(resp.user_fixable, Some(true));
    }

    #[test]
    fn test_user_fixable_binding_schema_mode_conflict_passes_through() {
        let err = ModuleError::new(
            ApcoreErrorCode::BindingSchemaModeConflict,
            "schema mode conflict",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "BINDING_SCHEMA_MODE_CONFLICT");
        assert_eq!(resp.user_fixable, Some(true));
    }

    #[test]
    fn test_user_fixable_binding_strict_schema_incompatible_passes_through() {
        let err = ModuleError::new(
            ApcoreErrorCode::BindingStrictSchemaIncompatible,
            "strict schema incompatible",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "BINDING_STRICT_SCHEMA_INCOMPATIBLE");
        assert_eq!(resp.user_fixable, Some(true));
    }

    #[test]
    fn test_user_fixable_none_for_unlisted_codes() {
        // Codes absent from user_fixable_for_code (e.g. MODULE_EXECUTE_ERROR)
        // keep userFixable=None unless the caller explicitly sets it.
        let err = ModuleError::new(ApcoreErrorCode::ModuleExecuteError, "execute failed");
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "MODULE_EXECUTE_ERROR");
        assert!(resp.user_fixable.is_none());
    }

    #[test]
    fn test_circuit_breaker_open_maps_to_retryable_with_guidance() {
        // apcore 0.20.0 sync alignment A-001: CIRCUIT_BREAKER_OPEN must
        // surface as retryable=true with an AI-facing recovery hint so
        // downstream agents back off instead of hammering an open breaker.
        let err = ModuleError::new(
            ApcoreErrorCode::CircuitBreakerOpen,
            "Circuit open for module 'demo.module' — call rejected",
        );
        let resp = ErrorMapper::to_mcp_error(&err);
        assert_eq!(resp.error_type, "CIRCUIT_BREAKER_OPEN");
        assert_eq!(resp.retryable, Some(true));
        assert!(
            resp.ai_guidance
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("circuit"),
            "ai_guidance should mention the circuit breaker; got {:?}",
            resp.ai_guidance
        );
    }
}
