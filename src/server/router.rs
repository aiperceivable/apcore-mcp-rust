//! ExecutionRouter — routes MCP tool calls to the apcore executor.
//!
//! Handles argument validation, execution dispatch, and output formatting.

// TODO(D11-110): Audit all fallible call sites (?, unwrap, expect, .await?) to ensure every
// error path funnels through ErrorMapper before propagation. See audit-report D11-110.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::Stream;

use apcore::trace_context::{TraceContext, TraceParent};
use apcore::CancelToken;

use crate::auth::middleware::AUTH_IDENTITY;
use crate::helpers::{ElicitResult, MCP_ELICIT_CALL_ID_KEY, MCP_ELICIT_KEY, MCP_PROGRESS_KEY};
use crate::server::approval_bridge::ApprovalBridge;
use crate::server::async_task_bridge::AsyncTaskBridge;
use crate::server::elicit_registry::ElicitGuard;
use crate::server::session::SessionRegistry;

/// A boxed stream of result chunks from a streaming executor.
pub type StreamResult = Pin<Box<dyn Stream<Item = Result<Value, ExecutorError>> + Send>>;

// ---------------------------------------------------------------------------
// Task 1: Executor trait
// ---------------------------------------------------------------------------

/// Errors returned by [`Executor`] methods.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// A module execution failed with a structured error code.
    #[error("{message}")]
    Execution {
        code: String,
        message: String,
        details: Option<Value>,
    },
    /// Input validation failed.
    #[error("validation failed: {0}")]
    Validation(String),
    /// Any other error.
    #[error("{0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// A single field-level validation error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    /// The field that failed validation (if applicable).
    pub field: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Nested validation errors (e.g. for nested objects).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ValidationError>,
}

/// Result of validating module inputs.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the inputs are valid.
    pub valid: bool,
    /// Field-level errors (empty when `valid` is true).
    pub errors: Vec<ValidationError>,
    /// Whether this module requires human approval before execution.
    /// Surfaces from the apcore preflight gate so AI agents calling
    /// `validate_tool` can decide whether to surface the approval flow
    /// before invoking. [A-D-005]
    pub requires_approval: bool,
}

impl Default for ValidationResult {
    fn default() -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            requires_approval: false,
        }
    }
}

/// Abstraction over the apcore execution pipeline.
///
/// Provides async `call_async`, optional `stream`, and optional `validate`
/// methods. Object-safe so it can be used as `Box<dyn Executor>`.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute a module by ID and return the result.
    ///
    /// `version_hint` optionally pins a module version for dispatch.
    async fn call_async(
        &self,
        module_id: &str,
        inputs: &Value,
        context: Option<&Value>,
        version_hint: Option<&str>,
    ) -> Result<Value, ExecutorError>;

    /// Return a stream of result chunks for a module, or `None` if
    /// the executor does not support streaming.
    ///
    /// `version_hint` optionally pins the streamed module's version
    /// for parity with `call_async`. Pre-fix Rust dropped the hint on
    /// the streaming path; Python and TS both forward it. [A-D-030]
    fn stream(
        &self,
        _module_id: &str,
        _inputs: &Value,
        _context: Option<&Value>,
        _version_hint: Option<&str>,
    ) -> Option<StreamResult> {
        None
    }

    /// Validate inputs for a module, or `None` if the executor does
    /// not support pre-execution validation.
    fn validate(
        &self,
        _module_id: &str,
        _inputs: &Value,
        _context: Option<&Value>,
    ) -> Option<ValidationResult> {
        None
    }

    /// Execute a module and return the result along with a pipeline trace.
    ///
    /// Returns `None` if the executor does not support tracing. When
    /// supported, returns `(result, trace)` where `trace` is a JSON
    /// value containing `strategy_name`, `total_duration_ms`, and `steps`.
    async fn call_with_trace(
        &self,
        _module_id: &str,
        _inputs: &Value,
        _context: Option<&Value>,
        _version_hint: Option<&str>,
    ) -> Option<Result<(Value, Value), ExecutorError>> {
        None
    }

    /// Execute a module with the caller's typed apcore [`Context`], returning
    /// `None` when this executor cannot accept one.
    ///
    /// # Why a typed context is needed at all
    ///
    /// The rest of this trait is deliberately apcore-agnostic: it passes the
    /// context as JSON so a non-apcore executor can implement it. But JSON is
    /// a copy. apcore's `Context.data` is an `Arc<RwLock<..>>` shared by
    /// `clone()` and `child()` all the way down to the `ApprovalRequest` an
    /// `ApprovalHandler` receives, and that sharing is the only channel by
    /// which a per-call value written by the router can reach the handler.
    /// Serializing the context loses it — apcore's own docs note that
    /// deserialization creates a fresh `Arc`.
    ///
    /// Without this, `ApcoreExecutorAdapter` passed `None` for the context and
    /// apcore fabricated an anonymous one, so everything the router wrote (the
    /// elicit call id, and the resolved identity with it) was discarded before
    /// the approval gate ran. See [`crate::server::elicit_registry`].
    ///
    /// `want_trace` folds the traced and untraced paths into one method rather
    /// than doubling the trait. The returned trace is `Some` only when it was
    /// asked for and the executor produced one.
    ///
    /// [`Context`]: apcore::Context
    async fn call_typed(
        &self,
        _module_id: &str,
        _inputs: &Value,
        _context: &apcore::Context<Value>,
        _version_hint: Option<&str>,
        _want_trace: bool,
    ) -> Option<Result<(Value, Option<Value>), ExecutorError>> {
        None
    }

    /// Return the descriptor-default version hint for `module_id`, if any.
    ///
    /// Used by [`ExecutionRouter::handle_call`] as the lowest-priority
    /// source in the spec's 3-source `version_hint` cascade (after
    /// `extras.version_hint` and `_meta.apcore.version`). [A-D-006]
    ///
    /// Default impl returns `None`. Real apcore-backed executors should
    /// override to return `descriptor.metadata.version_hint`.
    fn version_hint_default(&self, _module_id: &str) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// Task 2: deep_merge
// ---------------------------------------------------------------------------

/// Everything [`ExecutionRouter::build_context`] produces for one call.
///
/// `.3` is the elicit guard and MUST be bound for the life of the dispatch;
/// see [`ExecutionRouter::build_context`] for why a bare `_` reintroduces the
/// bug it exists to prevent.
type BuiltContext = (
    Value,
    HashMap<String, Box<dyn std::any::Any + Send + Sync>>,
    apcore::Context<Value>,
    Option<ElicitGuard>,
);

/// Build the per-call [`ElicitCallback`] that forwards to the MCP session.
///
/// Extracted because the callback is needed twice per call — once for the
/// legacy side-channel map and once for the registry an `ApprovalHandler`
/// reads — and the two must behave identically. A closure is not `Clone`, so
/// sharing one instance is not an option; sharing the constructor is.
fn make_elicit_callback(session: Arc<dyn SessionHandle>) -> crate::helpers::ElicitCallback {
    Box::new(move |message, requested_schema| {
        let session = Arc::clone(&session);
        Box::pin(async move {
            let schema = requested_schema.unwrap_or(serde_json::json!({}));
            match session.elicit_form(&message, &schema).await {
                Ok(result) => Some(result),
                Err(e) => {
                    tracing::warn!("Elicitation request failed: {e}");
                    None
                }
            }
        })
    })
}

/// Maximum recursion depth for [`deep_merge`].
const DEEP_MERGE_MAX_DEPTH: usize = 32;

/// Recursively merge `overlay` into `base`, capped at [`DEEP_MERGE_MAX_DEPTH`].
///
/// When both sides have an object for the same key, the merge recurses.
/// All other types (arrays, scalars, null) are overwritten by `overlay`.
pub(crate) fn deep_merge(base: &Value, overlay: &Value, depth: usize) -> Value {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            if depth >= DEEP_MERGE_MAX_DEPTH {
                // Flat merge: overlay keys win, no further recursion.
                let mut merged = base_map.clone();
                for (k, v) in overlay_map {
                    merged.insert(k.clone(), v.clone());
                }
                return Value::Object(merged);
            }
            let mut merged = base_map.clone();
            for (k, v) in overlay_map {
                let new_val = match merged.get(k) {
                    Some(existing_val) => deep_merge(existing_val, v, depth + 1),
                    None => v.clone(),
                };
                merged.insert(k.clone(), new_val);
            }
            Value::Object(merged)
        }
        _ => overlay.clone(),
    }
}

// ---------------------------------------------------------------------------
// Task 3: Output formatting
// ---------------------------------------------------------------------------

/// A custom formatter that converts execution results into text.
///
/// Only called for `Value::Object` results. Must be `Send + Sync` so it
/// can be shared across async tasks.
pub type OutputFormatter =
    Box<dyn Fn(&Value) -> Result<String, Box<dyn std::error::Error>> + Send + Sync>;

/// Cloneable, shareable output-formatter closure for configuration structs
/// (which derive `Clone`). A `Box`-based [`OutputFormatter`] cannot be cloned,
/// so config surfaces hold an `Arc` variant and convert to a [`OutputFormatter`]
/// via [`shared_to_output_formatter`] when forwarding to the builder.
pub type SharedOutputFormatter =
    Arc<dyn Fn(&Value) -> Result<String, Box<dyn std::error::Error>> + Send + Sync>;

/// Wrap a [`SharedOutputFormatter`] into a boxed [`OutputFormatter`].
pub fn shared_to_output_formatter(shared: SharedOutputFormatter) -> OutputFormatter {
    Box::new(move |value| shared(value))
}

/// Built-in output formats supported by the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Standard JSON serialization (default).
    #[default]
    Json,
    /// RFC 4180 CSV via apcore-toolkit.
    Csv,
    /// JSON Lines via apcore-toolkit.
    Jsonl,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(OutputFormat::Json),
            "csv" => Ok(OutputFormat::Csv),
            "jsonl" => Ok(OutputFormat::Jsonl),
            _ => Err(format!("Unknown output format: {s}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Task 4: Context construction types
// ---------------------------------------------------------------------------

/// Async function for sending MCP notifications (e.g. progress).
pub type SendNotificationFn = Arc<
    dyn Fn(
            Value,
        ) -> Pin<
            Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>,
        > + Send
        + Sync,
>;

/// Handle to the MCP session for elicitation requests.
#[async_trait]
pub trait SessionHandle: Send + Sync {
    /// Send an elicitation form to the client and return the result.
    async fn elicit_form(
        &self,
        message: &str,
        requested_schema: &Value,
    ) -> Result<ElicitResult, Box<dyn std::error::Error + Send + Sync>>;
}

/// Progress token — either a string or integer as per MCP spec.
#[derive(Debug, Clone)]
pub enum ProgressToken {
    /// A string progress token.
    String(String),
    /// An integer progress token.
    Integer(i64),
}

/// Structured extra parameters extracted from the MCP call metadata.
pub struct CallExtra {
    /// Progress token for streaming notifications.
    pub progress_token: Option<ProgressToken>,
    /// Notification sender for progress updates.
    pub send_notification: Option<SendNotificationFn>,
    /// Session handle for elicitation.
    pub session: Option<Arc<dyn SessionHandle>>,
    /// Identity from auth middleware (JSON form, legacy).
    pub identity: Option<Value>,
    /// Typed identity from auth middleware for `apcore::Context` construction.
    pub typed_identity: Option<apcore::Identity>,
}

/// Convert a [`ProgressToken`] to a JSON [`Value`].
fn progress_token_to_value(token: &ProgressToken) -> Value {
    match token {
        ProgressToken::String(s) => Value::String(s.clone()),
        ProgressToken::Integer(i) => serde_json::json!(*i),
    }
}

// ---------------------------------------------------------------------------
// ExecutionRouter
// ---------------------------------------------------------------------------

/// A single piece of MCP content returned from a tool execution.
#[derive(Debug, Clone)]
pub struct ContentItem {
    /// Content type (e.g. "text", "image", "resource").
    pub content_type: String,
    /// The content payload.
    pub data: Value,
}

/// Routes incoming MCP tool calls to the underlying apcore executor.
pub struct ExecutionRouter {
    executor: Option<Box<dyn Executor>>,
    validate_inputs: bool,
    redact_output: bool,
    /// When true, use `call_with_trace` (if available) and include pipeline
    /// trace data in the returned content.
    trace: bool,
    output_formatter: Option<OutputFormatter>,
    output_format: OutputFormat,
    /// Per-tool input schemas for `redact_sensitive` logging.
    /// Keys are tool names, values are their JSON Schema definitions.
    tool_schemas: HashMap<String, Value>,
    /// Per-tool output schemas for `redact_sensitive` output redaction.
    /// Keys are tool names, values are their output JSON Schema definitions.
    output_schemas: HashMap<String, Value>,
    /// Optional bridge for routing async-hinted modules and handling
    /// `__apcore_task_*` meta-tools.
    async_bridge: Option<Arc<AsyncTaskBridge>>,
    /// Optional bridge for handling the `__apcore_approval_check` meta-tool
    /// (Phase B async approval polling). Symmetric with `async_bridge`.
    approval_bridge: Option<Arc<ApprovalBridge>>,
    /// [B-002] Per-server `call_id → CancelToken` map. Populated on
    /// tool-call entry by handle_call(); consulted by cancel(call_id).
    /// Transport layer should forward inbound MCP
    /// `notifications/cancelled` to `router.cancel(call_id, reason)`.
    cancel_tokens: std::sync::Arc<std::sync::Mutex<HashMap<String, std::sync::Arc<CancelToken>>>>,
    /// Live MCP sessions, shared with the transport layer.
    ///
    /// [`extract_extra`](Self::extract_extra) consults it to turn the session
    /// id stamped onto an inbound message back into the connection that
    /// message arrived on. That lookup is what makes elicitation reachable
    /// from a transport-dispatched tool call: `CallExtra.session` used to be
    /// unconditionally `None` because a plain JSON `_meta` cannot carry a
    /// trait object, so no server-initiated request could ever be addressed.
    session_registry: Arc<SessionRegistry>,
}

/// [B-002] Maximum number of cancel-token entries (active + tombstones)
/// kept in the per-router map. When exceeded, oldest entries are evicted
/// in FIFO order to prevent unbounded growth from spam-cancel tombstones.
const CANCEL_TOKENS_MAX: usize = 4096;

// [B-002] Cancellation uses apcore's real `CancelToken` (imported above), not a
// local mirror. apcore::CancelToken is the same cooperative `AtomicBool` token
// (`Arc`-backed, `Clone`) and a strict superset of the previous local struct
// (it adds `check`/`check_for`/`reset`). Using the real type keeps the bridge
// from drifting and lets the token be threaded into `Context.cancel_token` so
// the executor pipeline and modules observe inbound `notifications/cancelled`.

/// RAII guard returned by [`ExecutionRouter::_cancel_guard`]. On drop,
/// releases the call_id slot from the router's cancel_tokens map so
/// the map doesn't grow unboundedly. [B-002]
struct CancelGuard {
    call_id: String,
    tokens: std::sync::Arc<std::sync::Mutex<HashMap<String, std::sync::Arc<CancelToken>>>>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.tokens.lock() {
            guard.remove(&self.call_id);
        }
    }
}

impl ExecutionRouter {
    /// Create a stub router for testing.
    ///
    /// The stub router does not execute any modules. Its `handle_call`
    /// method will panic if invoked (it is only used to verify handler
    /// wiring, not actual execution).
    pub fn stub() -> Self {
        Self {
            executor: None,
            validate_inputs: false,
            redact_output: true,
            trace: false,
            output_formatter: None,
            output_format: OutputFormat::Json,
            tool_schemas: HashMap::new(),
            output_schemas: HashMap::new(),
            async_bridge: None,
            approval_bridge: None,
            cancel_tokens: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_registry: Arc::new(SessionRegistry::new()),
        }
    }

    /// Create a router without an executor but with optional formatter.
    ///
    /// Useful when the executor is not yet available (e.g. during startup)
    /// but the output formatter should be pre-configured.
    pub fn new_with_formatter(
        validate_inputs: bool,
        output_formatter: Option<OutputFormatter>,
    ) -> Self {
        Self {
            executor: None,
            validate_inputs,
            redact_output: true,
            trace: false,
            output_formatter,
            output_format: OutputFormat::Json,
            tool_schemas: HashMap::new(),
            output_schemas: HashMap::new(),
            async_bridge: None,
            approval_bridge: None,
            cancel_tokens: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_registry: Arc::new(SessionRegistry::new()),
        }
    }

    /// Create a new router.
    ///
    /// # Arguments
    /// * `executor` - The executor to delegate tool calls to.
    /// * `validate_inputs` - Whether to validate tool inputs against their schema.
    /// * `output_formatter` - Optional custom output formatter.
    pub fn new(
        executor: Box<dyn Executor>,
        validate_inputs: bool,
        output_formatter: Option<OutputFormatter>,
    ) -> Self {
        Self {
            executor: Some(executor),
            validate_inputs,
            redact_output: true,
            trace: false,
            output_formatter,
            output_format: OutputFormat::Json,
            tool_schemas: HashMap::new(),
            output_schemas: HashMap::new(),
            async_bridge: None,
            approval_bridge: None,
            cancel_tokens: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_registry: Arc::new(SessionRegistry::new()),
        }
    }

    /// Set the output format to use for all tool responses.
    ///
    /// When set to `Csv` or `Jsonl`, automatically uses the corresponding
    /// formatter from `apcore-toolkit`. Takes precedence over any custom
    /// `output_formatter`.
    pub fn with_output_format(mut self, format: OutputFormat) -> Self {
        self.output_format = format;
        if format != OutputFormat::Json {
            self.output_formatter = Some(Self::resolve_builtin_formatter(format));
        }
        self
    }

    /// Internal: Resolve a built-in format name to an [`OutputFormatter`].
    fn resolve_builtin_formatter(format: OutputFormat) -> OutputFormatter {
        Box::new(move |value| {
            // Tabular formatters expect a slice of Maps.
            // Map the Value to a Vec of Maps.
            let rows: Vec<serde_json::Map<String, Value>> = match value {
                Value::Array(arr) => arr.iter().filter_map(|v| v.as_object().cloned()).collect(),
                Value::Object(obj) => vec![obj.clone()],
                _ => Vec::new(),
            };

            if rows.is_empty() {
                tracing::warn!(
                    "output_format={:?} requested but result is not tabular; falling back to JSON",
                    format
                );
                return Ok(serde_json::to_string(value)?);
            }

            match format {
                OutputFormat::Csv => Ok(apcore_toolkit::format_csv(&rows, false)),
                OutputFormat::Jsonl => Ok(apcore_toolkit::format_jsonl(&rows)),
                OutputFormat::Json => Ok(serde_json::to_string(value)?),
            }
        })
    }

    /// Set the tool schemas used for `redact_sensitive` logging.
    ///
    /// Keys are tool names, values are their JSON Schema definitions.
    /// When set, tool inputs are redacted before being logged at debug
    /// level, replacing values marked with `x-sensitive: true`.
    pub fn with_tool_schemas(mut self, schemas: HashMap<String, Value>) -> Self {
        self.tool_schemas = schemas;
        self
    }

    /// Set the output schemas used for `redact_sensitive` output redaction.
    ///
    /// Keys are tool names, values are their output JSON Schema definitions.
    /// When set and `redact_output` is true, tool outputs are redacted before
    /// formatting, replacing values marked with `x-sensitive: true`.
    pub fn with_output_schemas(mut self, schemas: HashMap<String, Value>) -> Self {
        self.output_schemas = schemas;
        self
    }

    /// Set whether to redact sensitive fields from tool outputs.
    ///
    /// When `true` (the default) and an output schema is registered for a
    /// tool, `apcore::redact_sensitive` is applied to the execution result
    /// before formatting.
    pub fn with_redact_output(mut self, redact: bool) -> Self {
        self.redact_output = redact;
        self
    }

    /// Set whether to include pipeline trace data in tool responses.
    ///
    /// When `true`, the router will attempt to use `call_with_trace` on the
    /// executor. If the executor supports tracing, the response will include
    /// an additional content item with the pipeline trace (strategy name,
    /// duration, steps). Defaults to `false`.
    pub fn with_trace(mut self, trace: bool) -> Self {
        self.trace = trace;
        self
    }

    /// Attach an [`AsyncTaskBridge`] so the router can route async-hinted
    /// modules through `AsyncTaskManager::submit` and service the reserved
    /// `__apcore_task_*` meta-tools.
    /// Wire an [`AsyncTaskBridge`] into the router for F-043 dispatch.
    ///
    /// Async-hinted module classification is performed dynamically at
    /// call time via `bridge.is_async_module_registered_self(tool_name)`,
    /// so newly-registered async modules (post-construction) are routed
    /// correctly. The pre-fix static `async_module_ids` set was stale on
    /// registry mutations and has been dropped. [A-D-031]
    pub fn with_async_bridge(mut self, bridge: Arc<AsyncTaskBridge>) -> Self {
        self.async_bridge = Some(bridge);
        self
    }

    /// Attach an [`ApprovalBridge`] so the router can service the reserved
    /// `__apcore_approval_check` meta-tool (Phase B async approval polling).
    ///
    /// Symmetric with [`with_async_bridge`](Self::with_async_bridge): when set,
    /// `handle_call` short-circuits the approval meta-tool to the bridge before
    /// any executor dispatch.
    pub fn with_approval_bridge(mut self, bridge: Arc<ApprovalBridge>) -> Self {
        self.approval_bridge = Some(bridge);
        self
    }

    /// Cooperatively cancel an in-flight tool call. [B-002]
    ///
    /// Looks up the [`CancelToken`] registered for `call_id` in
    /// `handle_call` and calls `token.cancel()`. Modules executing
    /// inside the apcore pipeline that check `context.cancel_token`
    /// (when wired) will observe the cancellation. When `call_id` is
    /// unknown (cancel arrived before submit, or after the call
    /// completed), records a tombstone so a subsequent same-id submit
    /// immediately sees the cancellation.
    ///
    /// Returns `true` when an active token was found and cancelled;
    /// `false` when the `call_id` was unknown (tombstone recorded).
    /// Wired by transport-level handlers parsing inbound MCP
    /// `notifications/cancelled` messages.
    pub fn cancel_call(&self, call_id: &str, _reason: Option<&str>) -> bool {
        let mut guard = self.cancel_tokens.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(token) = guard.get(call_id) {
            token.cancel();
            return true;
        }
        let tombstone = std::sync::Arc::new(CancelToken::new());
        tombstone.cancel();
        guard.insert(call_id.to_string(), tombstone);
        Self::_evict_cancel_tokens_locked(&mut guard);
        false
    }

    /// Internal: register a CancelToken for `call_id` and return an RAII
    /// guard that releases the slot when dropped, plus a clone of the token
    /// to thread into the execution Context. The returned token shares the
    /// same cancellation state (`Arc<AtomicBool>` inside apcore::CancelToken)
    /// as the one held in the map, so `cancel_call` flips both. [B-002]
    fn _cancel_guard(&self, call_id: &str) -> (CancelGuard, CancelToken) {
        let token = std::sync::Arc::new(CancelToken::new());
        let mut guard = self.cancel_tokens.lock().unwrap_or_else(|p| p.into_inner());
        // Tombstone race: if a tombstone was recorded earlier via
        // cancel_call, propagate the cancelled state to the new token.
        if let Some(existing) = guard.get(call_id) {
            if existing.is_cancelled() {
                token.cancel();
            }
        }
        guard.insert(call_id.to_string(), std::sync::Arc::clone(&token));
        Self::_evict_cancel_tokens_locked(&mut guard);
        let cancel_token = (*token).clone();
        (
            CancelGuard {
                call_id: call_id.to_string(),
                tokens: std::sync::Arc::clone(&self.cancel_tokens),
            },
            cancel_token,
        )
    }

    /// [B-002] When the cancel-tokens map exceeds [`CANCEL_TOKENS_MAX`],
    /// drop already-cancelled tombstones first, then drop arbitrary
    /// entries until back within bounds. Active tokens (where the
    /// CancelGuard is still alive — strong-count > 1) are preferred to
    /// be kept; only cancelled tombstones are dropped first.
    fn _evict_cancel_tokens_locked(
        map: &mut std::sync::MutexGuard<'_, HashMap<String, std::sync::Arc<CancelToken>>>,
    ) {
        if map.len() <= CANCEL_TOKENS_MAX {
            return;
        }
        // Phase 1: drop cancelled tombstones (no live CancelGuard
        // referencing them — strong_count == 1, only the map holds it).
        let to_drop: Vec<String> = map
            .iter()
            .filter(|(_, tok)| tok.is_cancelled() && std::sync::Arc::strong_count(tok) == 1)
            .map(|(k, _)| k.clone())
            .take(map.len() - CANCEL_TOKENS_MAX)
            .collect();
        for k in to_drop {
            map.remove(&k);
        }
        // Phase 2: if still over the cap (rare — would mean lots of
        // simultaneously-active calls), drop arbitrary entries to
        // restore the bound. Strict in-flight calls get a stale
        // CancelGuard that no longer maps back, but the call still
        // completes (just no cancel possible).
        while map.len() > CANCEL_TOKENS_MAX {
            let key = map.keys().next().cloned();
            match key {
                Some(k) => {
                    map.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Direct accessor used by tests and callers that need to cancel tasks
    /// on session disconnect.
    pub fn async_bridge(&self) -> Option<&Arc<AsyncTaskBridge>> {
        self.async_bridge.as_ref()
    }

    /// Redact sensitive fields from tool inputs for safe logging.
    ///
    /// If a schema is registered for `tool_name`, uses
    /// `apcore::redact_sensitive` to replace `x-sensitive` fields with
    /// `"***REDACTED***"`. Returns the original inputs unchanged when no
    /// schema is available.
    fn redact_inputs(&self, tool_name: &str, inputs: &Value) -> Value {
        match self.tool_schemas.get(tool_name) {
            Some(schema) => apcore::redact_sensitive(inputs, schema),
            None => inputs.clone(),
        }
    }

    /// Redact sensitive fields from tool output before formatting.
    ///
    /// If `redact_output` is enabled and an output schema is registered for
    /// `tool_name`, uses `apcore::redact_sensitive` to replace `x-sensitive`
    /// fields with `"***REDACTED***"`. Returns the original result unchanged
    /// when no schema is available or redaction is disabled.
    fn redact_output(&self, tool_name: &str, result: &Value) -> Value {
        if !self.redact_output {
            return result.clone();
        }
        match self.output_schemas.get(tool_name) {
            Some(schema) => apcore::redact_sensitive(result, schema),
            None => result.clone(),
        }
    }

    /// Format an execution result into text for LLM consumption.
    ///
    /// Uses the configured `output_formatter` if set, otherwise falls back
    /// to `serde_json::to_string`. Built-in formatters (CSV, JSONL) are applied
    /// to both objects and arrays; custom formatters are applied to objects only.
    fn format_result(&self, result: &Value) -> String {
        if let Some(ref formatter) = self.output_formatter {
            let apply = result.is_object()
                || (result.is_array() && self.output_format != OutputFormat::Json);
            if apply {
                match formatter(result) {
                    Ok(text) => return text,
                    Err(e) => {
                        tracing::debug!("output_formatter failed, falling back to json: {e}");
                    }
                }
            }
        }
        serde_json::to_string(result).unwrap_or_else(|_| "null".to_string())
    }

    // -----------------------------------------------------------------------
    // Task 6b: Preflight validation
    // -----------------------------------------------------------------------

    /// Validate a tool's arguments without executing the tool.
    ///
    /// Calls `executor.validate(tool_name, arguments)` if the executor
    /// supports pre-execution validation. Returns a structured JSON object:
    /// ```json
    /// { "valid": bool, "checks": [...], "requires_approval": bool }
    /// ```
    ///
    /// On unexpected errors, returns:
    /// ```json
    /// { "valid": false, "checks": [{"check": "unexpected", "message": "..."}] }
    /// ```
    pub async fn validate_tool(&self, tool_name: &str, arguments: &Value) -> Value {
        let executor = match &self.executor {
            Some(e) => e,
            None => {
                return serde_json::json!({
                    "valid": false,
                    "checks": [{"check": "unexpected", "message": "No executor configured"}],
                    "requires_approval": false
                });
            }
        };

        // catch_unwind guards against panics from third-party executor
        // implementations — validation should never take down the server.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            executor.validate(tool_name, arguments, None)
        })) {
            Ok(Some(validation)) => {
                let checks: Vec<Value> = validation
                    .errors
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "check": e.field.as_deref().unwrap_or("general"),
                            "message": e.message,
                            "passed": false
                        })
                    })
                    .collect();
                // Propagate the executor's requires_approval flag so AI
                // agents querying validate_tool before invoking can decide
                // whether to surface the approval flow. [A-D-005]
                serde_json::json!({
                    "valid": validation.valid,
                    "checks": checks,
                    "requires_approval": validation.requires_approval,
                })
            }
            Ok(None) => {
                // Executor does not support validation — report as valid.
                serde_json::json!({
                    "valid": true,
                    "checks": [],
                    "requires_approval": false
                })
            }
            Err(_) => {
                serde_json::json!({
                    "valid": false,
                    "checks": [{"check": "unexpected", "message": "validation panicked"}],
                    "requires_approval": false
                })
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task 7: Input validation
    // -----------------------------------------------------------------------

    /// Format validation errors into a human-readable string.
    ///
    /// Field errors are formatted as `"field: message"` and joined by `"; "`.
    /// Nested errors (sub-errors) are flattened. Errors without a field fall
    /// back to the message text alone.
    fn format_validation_errors(errors: &[ValidationError]) -> String {
        let parts: Vec<String> = errors
            .iter()
            .flat_map(|e| {
                if !e.errors.is_empty() {
                    e.errors
                        .iter()
                        .map(|sub| {
                            format!("{}: {}", sub.field.as_deref().unwrap_or("?"), sub.message)
                        })
                        .collect::<Vec<_>>()
                } else if let Some(ref field) = e.field {
                    vec![format!("{field}: {}", e.message)]
                } else {
                    vec![e.message.clone()]
                }
            })
            .collect();
        parts.join("; ")
    }

    // -----------------------------------------------------------------------
    // Task 8: handle_call orchestrator
    // -----------------------------------------------------------------------

    /// Handle an MCP tool call.
    ///
    /// Orchestrates the full execution flow:
    /// 1. Extract extras and build per-call context
    /// 2. Pre-execution input validation (when `validate_inputs` is true)
    /// 3. Select streaming vs non-streaming execution path
    /// 4. Return `(content_items, is_error, trace_id)`
    ///
    /// # Arguments
    /// * `tool_name` - The normalized MCP tool name.
    /// * `arguments` - The tool arguments as a JSON object.
    /// * `extra` - Additional MCP call metadata.
    ///
    /// # Returns
    /// A tuple of (content items, is_error flag, optional trace_id).
    pub async fn handle_call(
        &self,
        tool_name: &str,
        arguments: &Value,
        extra: Option<&Value>,
    ) -> (Vec<ContentItem>, bool, Option<String>) {
        // [B-002] Register a CancelToken for this call so an inbound MCP
        // `notifications/cancelled` (forwarded into `router.cancel_call(call_id)`)
        // cooperatively cancels in-flight work. call_id sourced from
        // extras.call_id, _meta.progressToken, or generated. Released
        // in `_release_cancel_token` at the end of this function via the
        // RAII guard.
        let call_id = extra
            .and_then(|v| v.get("call_id"))
            .and_then(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .or_else(|| {
                extra
                    .and_then(|v| v.get("_meta"))
                    .and_then(|v| v.get("progressToken"))
                    .and_then(|v| {
                        v.as_str()
                            .map(String::from)
                            .or_else(|| v.as_i64().map(|n| n.to_string()))
                    })
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (_cancel_guard, cancel_token) = self._cancel_guard(&call_id);

        let redacted = self.redact_inputs(tool_name, arguments);
        tracing::debug!(
            tool = tool_name,
            inputs = %redacted,
            "Executing tool call"
        );

        // Extract streaming helpers and identity from extra
        let (progress_token, send_notification, session, identity) = self.extract_extra(extra);

        // Resolve version_hint via the spec's 3-source cascade: [A-D-006]
        //   1. extras.version_hint  (SDK caller-supplied, highest priority)
        //   2. extras._meta.apcore.version  (MCP client-supplied)
        //   3. registry.get_definition(tool_name).metadata.version_hint
        //      (descriptor default, lowest)
        // Pre-fix Rust read only source #2.
        let version_hint: Option<String> = {
            // Source #1: extras.version_hint (snake_case) or .versionHint (camelCase)
            let from_extras = extra
                .and_then(|v| v.get("version_hint").or_else(|| v.get("versionHint")))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // Source #2: extras._meta.apcore.version
            let from_meta = || {
                extra
                    .and_then(|v| v.get("_meta"))
                    .and_then(|v| v.get("apcore"))
                    .and_then(|v| v.get("version"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            };
            // Source #3: executor.version_hint_default(module_id) — proxies
            // descriptor.metadata.version_hint via the Executor trait.
            let from_descriptor = || {
                self.executor
                    .as_ref()
                    .and_then(|exec| exec.version_hint_default(tool_name))
            };
            from_extras.or_else(from_meta).or_else(from_descriptor)
        };

        // Parse W3C traceparent from `_meta.traceparent` (if present).
        let trace_parent = Self::extract_traceparent(extra);

        // Build per-call context with inherited trace_id when provided.
        let call_extra = CallExtra {
            progress_token,
            send_notification,
            session,
            identity,
            typed_identity: None,
        };
        // `_elicit_guard` keeps the registered elicit callback alive for the
        // whole dispatch below. Binding it to a bare `_` would drop it here and
        // deregister the callback before the approval gate ever runs.
        let (context_value, _context_data, apcore_ctx, _elicit_guard) =
            Self::build_context_with_trace(&call_extra, trace_parent.clone(), Some(cancel_token));

        // Short-circuit the `__apcore_approval_check` meta-tool via the
        // ApprovalBridge, when configured. Symmetric with the AsyncTaskBridge
        // dispatch below.
        if let Some(approval_bridge) = &self.approval_bridge {
            if ApprovalBridge::is_meta_tool(tool_name) {
                let (value, is_error) =
                    approval_bridge.handle_meta_tool(tool_name, arguments).await;
                let result = if is_error {
                    Err(apcore::errors::ModuleError::new(
                        apcore::errors::ErrorCode::GeneralInvalidInput,
                        value
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("approval check failed")
                            .to_string(),
                    ))
                } else {
                    Ok(value)
                };
                return Self::meta_tool_response(result, &apcore_ctx);
            }
        }

        // Short-circuit async-task meta-tools and async-routed modules via
        // the AsyncTaskBridge, when configured.
        if let Some(bridge) = &self.async_bridge {
            let progress_token_val = extra.and_then(|v| v.get("progressToken")).cloned();
            let session_key = extra
                .and_then(|v| v.get("sessionId"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // [A-D-FA-7] Same three-tier resolution the context builder uses.
            // Reading only `call_extra.identity` here left the bridge blind to
            // the ambient identity, so any transport path that binds
            // AUTH_IDENTITY without populating `extra` submitted tasks
            // anonymously — where Python reads `auth_identity_var` and
            // TypeScript `getCurrentIdentity()` inside their handlers.
            let resolved_identity = Self::resolve_identity(&call_extra);
            // [A-D-220] Pass send_notification through so the bridge can
            // store it in its progress_senders map and fan out
            // `notifications/progress` via emit_progress(). Cloning the
            // Arc is cheap; the original sender remains available for the
            // non-bridge code paths below.
            let bridge_sender = call_extra.send_notification.as_ref().map(Arc::clone);
            if let Some(result) = bridge
                .handle_meta_tool(
                    tool_name,
                    arguments,
                    resolved_identity.clone(),
                    progress_token_val.clone(),
                    bridge_sender.clone(),
                    session_key.as_deref(),
                )
                .await
            {
                return Self::meta_tool_response(result, &apcore_ctx);
            }
            // Async-hint dispatch: dynamic descriptor lookup against the
            // bridge's own registry. Catches modules registered AFTER
            // router construction (RegistryListener-driven dynamic-tool
            // registration) — the pre-fix static `async_module_ids` set
            // was frozen at startup and stale on mutations. [A-D-031]
            if bridge.is_async_module_registered_self(tool_name) {
                let res = bridge
                    .submit(
                        tool_name,
                        arguments.clone(),
                        resolved_identity,
                        progress_token_val,
                        bridge_sender,
                        session_key.as_deref(),
                    )
                    .await;
                // Wrap into the spec envelope `{task_id, status: "pending"}`
                // rather than serialising the full TaskInfo. [A-D-007] —
                // matches what the meta-tool path does in handle_submit.
                return Self::meta_tool_response(
                    res.map(|info| {
                        json!({
                            "task_id": info.task_id,
                            "status": "pending",
                        })
                    }),
                    &apcore_ctx,
                );
            }
        }

        // Re-extract after building context (we moved them into CallExtra)
        let (progress_token, send_notification, _, _) = self.extract_extra(extra);

        // Pre-execution validation. catch_unwind guards against panics from
        // third-party executor implementations (symmetric with validate_tool's
        // panic-safety) — a buggy validator must not bring down handle_call.
        // [A-D-029]
        if self.validate_inputs {
            if let Some(ref executor) = self.executor {
                let validate_result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        executor.validate(tool_name, arguments, Some(&context_value))
                    }));
                match validate_result {
                    Ok(Some(validation)) if !validation.valid => {
                        let detail = Self::format_validation_errors(&validation.errors);
                        return (
                            vec![ContentItem {
                                content_type: "text".into(),
                                data: Value::String(format!("Validation failed: {detail}")),
                            }],
                            true,
                            None,
                        );
                    }
                    Ok(Some(_)) => { /* valid, continue */ }
                    Ok(None) => { /* executor doesn't support validate, skip */ }
                    Err(_) => {
                        return (
                            vec![ContentItem {
                                content_type: "text".into(),
                                data: Value::String("Validation failed: validator panicked".into()),
                            }],
                            true,
                            None,
                        );
                    }
                }
            }
        }

        // Select execution path: try streaming if both prerequisites present;
        // handle_stream will fall back to non-streaming if executor doesn't
        // support streaming.
        if let (Some(ref pt), Some(ref sn)) = (progress_token, send_notification) {
            self.handle_stream(
                tool_name,
                arguments,
                pt,
                sn,
                Some(&context_value),
                version_hint.as_deref(),
                Some(&apcore_ctx),
            )
            .await
        } else {
            self.handle_call_async_with_hint(
                tool_name,
                arguments,
                Some(&context_value),
                version_hint.as_deref(),
                Some(&apcore_ctx),
            )
            .await
        }
    }

    /// Read the session id the transport stamped onto an inbound message.
    ///
    /// Accepts both shapes the extra `Value` takes in practice: the
    /// `params._meta` object the transport dispatch path passes straight
    /// through, and the wrapper object an SDK caller builds with `_meta`
    /// nested inside it.
    fn session_id_from_extra(extra: Option<&Value>) -> Option<&str> {
        let extra = extra?;
        extra
            .get("sessionId")
            .or_else(|| extra.get("_meta").and_then(|meta| meta.get("sessionId")))
            .and_then(Value::as_str)
    }

    /// Extract progress_token, send_notification, session, and identity from
    /// the extra `Value`. Returns `(None, None, None, None)` when extra is
    /// `None`.
    ///
    /// `send_notification` stays `None`: it is a closure, and a plain JSON
    /// `Value` cannot carry one. `session` no longer does. The transport
    /// stamps its session id onto every inbound message
    /// ([`stamp_session_id`]), and the live connection behind that id lives in
    /// [`SessionRegistry`] — so the trait object is recovered by lookup rather
    /// than by deserialization. Without this the `session` slot was
    /// hard-coded `None`, which is why an interactive approval prompt could
    /// never reach a client (apexe#29).
    ///
    /// [`stamp_session_id`]: crate::server::transport
    #[allow(clippy::type_complexity)]
    fn extract_extra(
        &self,
        extra: Option<&Value>,
    ) -> (
        Option<ProgressToken>,
        Option<SendNotificationFn>,
        Option<Arc<dyn SessionHandle>>,
        Option<Value>,
    ) {
        // In the current integration the factory passes a plain JSON Value
        // which does not carry callbacks. Real callbacks would come from a
        // typed CallExtra.  For now we extract identity from JSON.
        let identity = extra.and_then(|v| v.get("identity")).cloned();

        let progress_token = extra.and_then(|v| v.get("progress_token")).and_then(|v| {
            if let Some(s) = v.as_str() {
                Some(ProgressToken::String(s.to_string()))
            } else {
                v.as_i64().map(ProgressToken::Integer)
            }
        });

        let session = Self::session_id_from_extra(extra)
            .and_then(|id| self.session_registry.get(id))
            .map(|session| -> Arc<dyn SessionHandle> { session });

        (progress_token, None, session, identity)
    }

    /// The registry of live sessions this router resolves elicitation through.
    ///
    /// The transport layer registers each connection here; handing out the
    /// `Arc` is how the two sides come to share one map without the transport
    /// needing a reference to the router.
    pub fn session_registry(&self) -> Arc<SessionRegistry> {
        Arc::clone(&self.session_registry)
    }

    /// Handle an MCP tool call with a typed `CallExtra`.
    ///
    /// This is the full-featured entry point that supports streaming,
    /// elicitation, and progress callbacks via the `CallExtra` struct.
    pub async fn handle_call_with_extra(
        &self,
        tool_name: &str,
        arguments: &Value,
        extra: Option<CallExtra>,
    ) -> (Vec<ContentItem>, bool, Option<String>) {
        let redacted = self.redact_inputs(tool_name, arguments);
        tracing::debug!(
            tool = tool_name,
            inputs = %redacted,
            "Executing tool call"
        );

        let extra = extra.unwrap_or_else(|| CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        });

        // Build per-call context
        let (context_value, _context_data, apcore_ctx, _elicit_guard) = Self::build_context(&extra);

        // Pre-execution validation. catch_unwind guards against panics from
        // third-party executor implementations (symmetric with validate_tool's
        // panic-safety) — a buggy validator must not bring down handle_call.
        // [A-D-029]
        if self.validate_inputs {
            if let Some(ref executor) = self.executor {
                let validate_result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        executor.validate(tool_name, arguments, Some(&context_value))
                    }));
                match validate_result {
                    Ok(Some(validation)) if !validation.valid => {
                        let detail = Self::format_validation_errors(&validation.errors);
                        return (
                            vec![ContentItem {
                                content_type: "text".into(),
                                data: Value::String(format!("Validation failed: {detail}")),
                            }],
                            true,
                            None,
                        );
                    }
                    Ok(Some(_)) => { /* valid, continue */ }
                    Ok(None) => { /* executor doesn't support validate, skip */ }
                    Err(_) => {
                        return (
                            vec![ContentItem {
                                content_type: "text".into(),
                                data: Value::String("Validation failed: validator panicked".into()),
                            }],
                            true,
                            None,
                        );
                    }
                }
            }
        }

        // Select execution path: try streaming if both prerequisites present;
        // handle_stream will fall back to non-streaming if executor doesn't
        // support streaming.
        if let (Some(ref pt), Some(ref sn)) = (extra.progress_token, extra.send_notification) {
            self.handle_stream(
                tool_name,
                arguments,
                pt,
                sn,
                Some(&context_value),
                None,
                Some(&apcore_ctx),
            )
            .await
        } else {
            self.handle_call_async_with_hint(
                tool_name,
                arguments,
                Some(&context_value),
                None,
                Some(&apcore_ctx),
            )
            .await
        }
    }

    // -----------------------------------------------------------------------
    // Task 4: Context construction
    // -----------------------------------------------------------------------

    /// Build the ContentItem response for an async-bridge meta-tool result.
    ///
    /// Serialises the JSON result into a text content item, mapping
    /// apcore errors through the error mapper so the MCP client sees a
    /// protocol error payload. Returns the `trace_id` from the current
    /// apcore context so callers can attach `_meta.traceparent`.
    fn meta_tool_response(
        result: Result<Value, apcore::errors::ModuleError>,
        apcore_ctx: &apcore::Context<Value>,
    ) -> (Vec<ContentItem>, bool, Option<String>) {
        match result {
            Ok(value) => {
                let text = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
                (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String(text),
                    }],
                    false,
                    Some(apcore_ctx.trace_id.clone()),
                )
            }
            Err(err) => {
                let mapped = crate::adapters::errors::ErrorMapper::to_mcp_error(&err);
                let text = serde_json::to_string(&mapped).unwrap_or_else(|_| err.to_string());
                (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String(text),
                    }],
                    true,
                    Some(apcore_ctx.trace_id.clone()),
                )
            }
        }
    }

    /// Extract a W3C `TraceParent` from an MCP `_meta` value.
    ///
    /// Returns `None` when `_meta` is absent, when `_meta.traceparent` is
    /// missing, or when the string fails validation. Uses apcore's
    /// built-in header validation (W3C §2.2).
    pub fn extract_traceparent(extra: Option<&Value>) -> Option<TraceParent> {
        let raw = extra?.get("_meta")?.get("traceparent")?.as_str()?;
        let mut headers = HashMap::new();
        headers.insert("traceparent".to_string(), raw.to_string());
        TraceContext::extract(&headers)
    }

    /// Inject a `traceparent` header derived from an apcore `Context`
    /// into the `_meta` object returned to the MCP client.
    ///
    /// Returns a `Value::Object` with the `traceparent` field set, ready
    /// to be merged into the tool response's `_meta`.
    pub fn inject_traceparent_meta(apcore_ctx: &apcore::Context<Value>) -> Value {
        let headers = TraceContext::inject(apcore_ctx);
        match headers.get("traceparent") {
            Some(tp) => serde_json::json!({ "traceparent": tp }),
            None => Value::Object(serde_json::Map::new()),
        }
    }

    /// Build execution context with MCP callbacks and identity.
    ///
    /// Constructs a JSON context value and an `apcore::Context<Value>`.
    /// If callbacks are present, stores them under `MCP_PROGRESS_KEY` /
    /// `MCP_ELICIT_KEY` in both the side-channel data map (as actual
    /// callback objects) and in `apcore_ctx.data` (as marker values so
    /// modules can detect availability).
    ///
    /// Returns `(context_value, context_data, apcore_context, elicit_guard)`
    /// where `context_data` holds the callback objects that cannot be
    /// serialized into JSON, and `apcore_context` is the proper apcore
    /// `Context`.
    ///
    /// # The elicit guard MUST outlive the call
    ///
    /// `elicit_guard` owns the registry entry that
    /// [`MCP_ELICIT_CALL_ID_KEY`] in `apcore_context.data` points at. Dropping
    /// it deregisters the callback, so an approval handler running after that
    /// point resolves the id to nothing and rejects. Bind it for the whole
    /// dispatch (`let (.., _elicit_guard) = ...`) — never to a bare `_`, which
    /// drops immediately and reinstates exactly the bug this returns it to
    /// prevent.
    fn build_context(extra: &CallExtra) -> BuiltContext {
        Self::build_context_with_trace(extra, None, None)
    }

    /// Like [`build_context`] but inherits the W3C trace_id from
    /// `trace_parent` when provided, and threads `cancel_token` into the
    /// resulting Context so the executor pipeline and modules observe inbound
    /// MCP `notifications/cancelled`.
    /// Resolve the caller's identity for one tool call.
    ///
    /// Prefers the typed identity, then the JSON one carried in `extra`, then
    /// the `AUTH_IDENTITY` task-local bound by the auth middleware.
    ///
    /// [A-D-FA-7] Single source of truth. This resolution used to be spelled
    /// out in `build_context_with_trace` with all three tiers and again in the
    /// async-bridge branch of `handle_call` with only the JSON tier, so the
    /// same call could be authenticated for the executor and anonymous for the
    /// bridge.
    fn resolve_identity(extra: &CallExtra) -> Option<apcore::Identity> {
        extra
            .typed_identity
            .clone()
            .or_else(|| {
                extra
                    .identity
                    .as_ref()
                    .and_then(|v| serde_json::from_value::<apcore::Identity>(v.clone()).ok())
            })
            .or_else(|| AUTH_IDENTITY.try_with(|id| id.clone()).ok().flatten())
    }

    fn build_context_with_trace(
        extra: &CallExtra,
        trace_parent: Option<TraceParent>,
        cancel_token: Option<CancelToken>,
    ) -> BuiltContext {
        let mut data: HashMap<String, Box<dyn std::any::Any + Send + Sync>> = HashMap::new();
        let mut context_obj = serde_json::Map::new();

        let resolved_identity: Option<apcore::Identity> = Self::resolve_identity(extra);

        // Construct apcore::Context with or without identity.
        // When `trace_parent` is Some, use ContextBuilder so the incoming
        // W3C trace_id propagates into the Context (subject to apcore's
        // validation per PROTOCOL_SPEC §10.5).
        let mut apcore_ctx: apcore::Context<Value> = match (&resolved_identity, &trace_parent) {
            (Some(ident), Some(_)) => apcore::Context::<Value>::builder()
                .identity(Some(ident.clone()))
                .trace_parent(trace_parent.clone())
                .build(),
            (None, Some(_)) => apcore::Context::<Value>::builder()
                .trace_parent(trace_parent.clone())
                .build(),
            (Some(ident), None) => apcore::Context::new(ident.clone()),
            (None, None) => apcore::Context::anonymous(),
        };

        // Thread the inbound cancel token into the Context so the executor
        // pipeline (which checks `context.cancel_token`) and modules observe
        // MCP `notifications/cancelled`. Set at construction time — before the
        // Context enters the pipeline — so this is not the post-dispatch
        // mutation anti-pattern. Matches apcore-mcp-python / -typescript, which
        // pass `cancel_token` into `Context.create()`. Field assignment (rather
        // than the builder) keeps all four construction branches above intact,
        // preserving their `data`/identity initialization semantics.
        apcore_ctx.cancel_token = cancel_token;

        let has_progress = extra.progress_token.is_some() && extra.send_notification.is_some();
        let has_elicit = extra.session.is_some();

        // Inject progress callback
        if let (Some(ref token), Some(ref send_notification)) =
            (&extra.progress_token, &extra.send_notification)
        {
            let token = token.clone();
            let sn = Arc::clone(send_notification);
            let progress_cb: crate::helpers::ProgressCallback =
                Box::new(move |progress, total, message| {
                    let token_val = progress_token_to_value(&token);
                    let sn = Arc::clone(&sn);
                    Box::pin(async move {
                        let mut params = serde_json::Map::new();
                        params.insert("progressToken".to_string(), token_val);
                        params.insert("progress".to_string(), serde_json::json!(progress));
                        params.insert(
                            "total".to_string(),
                            total
                                .map(|t| serde_json::json!(t))
                                .unwrap_or(serde_json::json!(0)),
                        );
                        if let Some(msg) = message {
                            params.insert("message".to_string(), Value::String(msg));
                        }
                        let notification = serde_json::json!({
                            "method": "notifications/progress",
                            "params": Value::Object(params),
                        });
                        if let Err(e) = sn(notification).await {
                            tracing::debug!("Failed to send progress notification: {e}");
                        }
                    })
                });
            data.insert(MCP_PROGRESS_KEY.to_string(), Box::new(progress_cb));
        }

        // Inject elicit callback.
        //
        // Two copies, because they serve two different consumers. The
        // side-channel `data` map is the historical home and is typed
        // `Box<dyn Any>`, which no apcore surface can read. The registry copy
        // is the one an `ApprovalHandler` can actually reach: it is addressed
        // by the id written into `apcore_ctx.data` below, and the returned
        // guard keeps it alive for exactly this call.
        let mut elicit_guard: Option<ElicitGuard> = None;
        if let Some(ref session) = extra.session {
            data.insert(
                MCP_ELICIT_KEY.to_string(),
                Box::new(make_elicit_callback(Arc::clone(session))),
            );
            elicit_guard = Some(crate::server::elicit_registry::register(
                make_elicit_callback(Arc::clone(session)),
            ));
        }

        // Write marker values into apcore Context.data so modules can
        // detect callback availability. Actual callbacks live in the
        // side-channel `data` map since serde_json::Value cannot hold
        // function pointers.
        {
            let mut shared = apcore_ctx.data.write();
            if has_progress {
                shared.insert(MCP_PROGRESS_KEY.to_string(), serde_json::json!("available"));
            }
            if has_elicit {
                shared.insert(MCP_ELICIT_KEY.to_string(), serde_json::json!("available"));
            }
            // The handle an ApprovalHandler exchanges for the real callback.
            // `"available"` above says elicitation exists; this says how to
            // perform one. Written only when a callback was actually
            // registered, so a stale id can never be present without a live
            // entry behind it.
            if let Some(ref guard) = elicit_guard {
                shared.insert(
                    MCP_ELICIT_CALL_ID_KEY.to_string(),
                    Value::String(guard.id().to_string()),
                );
            }
        }

        // Set identity in JSON context (legacy path)
        if let Some(ref identity) = extra.identity {
            context_obj.insert("identity".to_string(), identity.clone());
        } else if let Some(ref identity) = resolved_identity {
            // Serialize the resolved identity into the JSON context
            if let Ok(val) = serde_json::to_value(identity) {
                context_obj.insert("identity".to_string(), val);
            }
        }

        // Set trace_id (use the one from apcore_ctx for consistency)
        context_obj.insert(
            "trace_id".to_string(),
            Value::String(apcore_ctx.trace_id.clone()),
        );

        (Value::Object(context_obj), data, apcore_ctx, elicit_guard)
    }

    // -----------------------------------------------------------------------
    // Task 5: Non-streaming path
    // -----------------------------------------------------------------------

    /// Build error text from an `ExecutorError`, appending AI guidance when
    /// available.
    fn build_error_text(error: &ExecutorError) -> String {
        match error {
            ExecutorError::Execution {
                code: _,
                message,
                details,
            } => {
                let mut text = message.clone();
                // Check details for guidance fields
                if let Some(ref d) = details {
                    let guidance_keys = ["retryable", "aiGuidance", "userFixable", "suggestion"];
                    let guidance: serde_json::Map<String, Value> = guidance_keys
                        .iter()
                        .filter_map(|&k| d.get(k).map(|v| (k.to_string(), v.clone())))
                        .collect();
                    if !guidance.is_empty() {
                        text = format!(
                            "{text}\n\n{}",
                            serde_json::to_string(&guidance).unwrap_or_default()
                        );
                    }
                }
                text
            }
            ExecutorError::Validation(msg) => format!("Validation failed: {msg}"),
            ExecutorError::Other(e) => e.to_string(),
        }
    }

    /// Render a [`Executor::call_typed`] outcome into the MCP content shape.
    ///
    /// Mirrors the two render blocks of the untyped paths: success formats and
    /// redacts the result, a trace (when one was produced) is appended as a
    /// `trace` content item, and an error becomes an `is_error` text item.
    fn render_call_outcome(
        &self,
        tool_name: &str,
        outcome: Result<(Value, Option<Value>), ExecutorError>,
        context: Option<&Value>,
    ) -> (Vec<ContentItem>, bool, Option<String>) {
        match outcome {
            Ok((result, trace)) => {
                let redacted_result = self.redact_output(tool_name, &result);
                let text = self.format_result(&redacted_result);
                let mut content = vec![ContentItem {
                    content_type: "text".into(),
                    data: Value::String(text),
                }];
                if let Some(trace) = trace {
                    content.push(ContentItem {
                        content_type: "trace".into(),
                        data: trace,
                    });
                }
                let trace_id = context
                    .and_then(|c| c.get("trace_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (content, false, trace_id)
            }
            Err(error) => {
                tracing::error!("handle_call typed error for {tool_name}: {error}");
                let text = Self::build_error_text(&error);
                (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String(text),
                    }],
                    true,
                    None,
                )
            }
        }
    }

    /// `apcore_ctx` carries the typed context to [`Executor::call_typed`]. It
    /// is `None` only on paths that have no apcore context to give (the
    /// convenience `handle_call_async` wrapper and its tests); the real
    /// `tools/call` dispatch always supplies one, because the elicit call id
    /// and the resolved identity live in it and nowhere else.
    async fn handle_call_async_with_hint(
        &self,
        tool_name: &str,
        arguments: &Value,
        context: Option<&Value>,
        version_hint: Option<&str>,
        apcore_ctx: Option<&apcore::Context<Value>>,
    ) -> (Vec<ContentItem>, bool, Option<String>) {
        let executor = match &self.executor {
            Some(e) => e,
            None => {
                return (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String("No executor configured".into()),
                    }],
                    true,
                    None,
                );
            }
        };

        // Typed-context path. Preferred whenever the caller has an apcore
        // Context, because it is the only dispatch that preserves the shared
        // `data` map the approval gate reads.
        //
        // Wrapped in the same panic guard as `call_async` below [A-D-029]:
        // this is now the primary production dispatch, so a panicking executor
        // impl reaching the caller as a lost connection instead of an
        // `is_error` result would be a regression in exactly the path that
        // matters most.
        use futures::FutureExt;
        if let Some(ctx) = apcore_ctx {
            let typed_future = std::panic::AssertUnwindSafe(executor.call_typed(
                tool_name,
                arguments,
                ctx,
                version_hint,
                self.trace,
            ));
            match typed_future.catch_unwind().await {
                Ok(Some(typed_result)) => {
                    return self.render_call_outcome(tool_name, typed_result, context)
                }
                // `None` means this executor cannot take a typed context; fall
                // through to the untyped paths below.
                Ok(None) => {}
                Err(_panic) => {
                    tracing::error!("handle_call: executor panicked for {tool_name}");
                    return (
                        vec![ContentItem {
                            content_type: "text".into(),
                            data: Value::String(format!(
                                "Internal error: executor panicked while invoking {tool_name}"
                            )),
                        }],
                        true,
                        None,
                    );
                }
            }
        }

        // When trace mode is enabled, attempt call_with_trace first.
        if self.trace {
            if let Some(trace_result) = executor
                .call_with_trace(tool_name, arguments, context, version_hint)
                .await
            {
                return match trace_result {
                    Ok((result, trace)) => {
                        let redacted_result = self.redact_output(tool_name, &result);
                        let text = self.format_result(&redacted_result);
                        let mut content = vec![ContentItem {
                            content_type: "text".into(),
                            data: Value::String(text),
                        }];
                        // Attach compact trace as side-channel content for
                        // routers/tests that want it inline. The factory
                        // maps it into `_meta.trace` via `last_trace()`.
                        content.push(ContentItem {
                            content_type: "trace".into(),
                            data: trace.clone(),
                        });
                        let trace_id = context
                            .and_then(|c| c.get("trace_id"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        (content, false, trace_id)
                    }
                    Err(error) => {
                        tracing::error!("handle_call trace error for {tool_name}: {error}");
                        let text = Self::build_error_text(&error);
                        (
                            vec![ContentItem {
                                content_type: "text".into(),
                                data: Value::String(text),
                            }],
                            true,
                            None,
                        )
                    }
                };
            }
            // Executor does not support call_with_trace; fall through to normal path.
            tracing::debug!(
                "Trace enabled but executor does not support call_with_trace for {tool_name}, \
                 falling back to call_async"
            );
        }

        // Symmetric panic-safety with validate_tool / handle_call's
        // pre-execution validate guard. A panicking third-party executor
        // impl must NOT bring down the call; convert to an is_error result.
        // Uses futures::FutureExt::catch_unwind to catch panics across
        // .await points — std::panic::catch_unwind cannot catch async
        // panics on its own. [A-D-029]
        let call_future = std::panic::AssertUnwindSafe(executor.call_async(
            tool_name,
            arguments,
            context,
            version_hint,
        ));
        match call_future.catch_unwind().await {
            Ok(Ok(result)) => {
                let redacted_result = self.redact_output(tool_name, &result);
                let text = self.format_result(&redacted_result);
                let content = vec![ContentItem {
                    content_type: "text".into(),
                    data: Value::String(text),
                }];
                let trace_id = context
                    .and_then(|c| c.get("trace_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (content, false, trace_id)
            }
            Ok(Err(error)) => {
                tracing::error!("handle_call error for {tool_name}: {error}");
                let text = Self::build_error_text(&error);
                (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String(text),
                    }],
                    true,
                    None,
                )
            }
            Err(_panic) => {
                tracing::error!("handle_call: executor panicked for {tool_name}");
                (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String(format!(
                            "Internal error: executor panicked while invoking {tool_name}"
                        )),
                    }],
                    true,
                    None,
                )
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task 6: Streaming path
    // -----------------------------------------------------------------------

    // TODO(apcore>=0.19): streaming traces (stream_with_trace) not exposed
    // by apcore 0.18; when available, capture step traces during streaming
    // and attach them to `_meta.trace` like the non-streaming path.

    /// Streaming execution via executor.stream().
    ///
    /// Iterates the async stream, sends progress notifications for each
    /// chunk, accumulates results via deep merge, and returns the final
    /// result. Falls back to non-streaming if the executor returns `None`.
    // Eight parameters, one over clippy's threshold. The alternative is a
    // parameter struct whose only purpose is to satisfy the lint: every
    // argument here is genuinely independent, and the file already carries the
    // same judgement on `extract_extra`. `apcore_ctx` is the newest and the
    // one that must not be dropped — see `handle_call_async_with_hint`.
    #[allow(clippy::too_many_arguments)]
    async fn handle_stream(
        &self,
        tool_name: &str,
        arguments: &Value,
        progress_token: &ProgressToken,
        send_notification: &SendNotificationFn,
        context: Option<&Value>,
        version_hint: Option<&str>,
        apcore_ctx: Option<&apcore::Context<Value>>,
    ) -> (Vec<ContentItem>, bool, Option<String>) {
        use tokio_stream::StreamExt;

        let executor = match &self.executor {
            Some(e) => e,
            None => {
                return (
                    vec![ContentItem {
                        content_type: "text".into(),
                        data: Value::String("No executor configured".into()),
                    }],
                    true,
                    None,
                );
            }
        };

        let stream = match executor.stream(tool_name, arguments, context, version_hint) {
            Some(s) => s,
            None => {
                // Fallback to non-streaming
                // `ApcoreExecutorAdapter` does not implement `stream`, so the
                // real server always lands here — which is why the typed
                // context has to be forwarded rather than dropped.
                return self
                    .handle_call_async_with_hint(
                        tool_name,
                        arguments,
                        context,
                        version_hint,
                        apcore_ctx,
                    )
                    .await;
            }
        };

        tokio::pin!(stream);

        let mut accumulated = Value::Object(serde_json::Map::new());
        let mut chunk_index: usize = 0;

        loop {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    // Per-chunk redaction MUST happen before the chunk is
                    // serialized into the progress notification — otherwise
                    // x-sensitive credentials emitted mid-stream leak to
                    // the MCP client even though the final accumulated
                    // value is later redacted. Mirrors Python's
                    // _handle_stream invariant. [A-D-003]
                    let safe_chunk = self.redact_output(tool_name, &chunk);

                    // Send progress notification
                    let notification = serde_json::json!({
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": progress_token_to_value(progress_token),
                            "progress": chunk_index + 1,
                            "total": null,
                            "message": serde_json::to_string(&safe_chunk).unwrap_or_default(),
                        }
                    });
                    if let Err(e) = send_notification(notification).await {
                        tracing::debug!("Failed to send progress notification: {e}");
                    }

                    // Accumulate the original (un-redacted) chunk so the
                    // final result still has full fidelity for redaction
                    // at the response boundary; redaction is reapplied
                    // there. The mid-stream notification is the leak
                    // vector this fix closes.
                    accumulated = deep_merge(&accumulated, &chunk, 0);
                    chunk_index += 1;
                }
                Some(Err(error)) => {
                    tracing::error!("handle_call stream error for {tool_name}: {error}");
                    let text = Self::build_error_text(&error);
                    return (
                        vec![ContentItem {
                            content_type: "text".into(),
                            data: Value::String(text),
                        }],
                        true,
                        None,
                    );
                }
                None => break,
            }
        }

        let redacted_accumulated = self.redact_output(tool_name, &accumulated);
        let text = self.format_result(&redacted_accumulated);
        let content = vec![ContentItem {
            content_type: "text".into(),
            data: Value::String(text),
        }];
        let trace_id = context
            .and_then(|c| c.get("trace_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        (content, false, trace_id)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    // ---- Task 1: Executor trait tests ----

    /// A mock executor for testing. Returns `inputs` as the result.
    struct MockExecutor;

    #[async_trait]
    impl Executor for MockExecutor {
        async fn call_async(
            &self,
            module_id: &str,
            inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            Ok(json!({ "module": module_id, "echo": inputs }))
        }
    }

    /// A mock executor that supports streaming and validation.
    struct FullMockExecutor;

    #[async_trait]
    impl Executor for FullMockExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            Ok(inputs.clone())
        }

        fn stream(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Option<StreamResult> {
            let chunks = vec![Ok(json!({"a": 1})), Ok(json!({"b": 2}))];
            Some(Box::pin(tokio_stream::iter(chunks)))
        }

        fn validate(
            &self,
            _module_id: &str,
            inputs: &Value,
            _context: Option<&Value>,
        ) -> Option<ValidationResult> {
            // If inputs has "invalid" key, return invalid
            if inputs.get("invalid").is_some() {
                Some(ValidationResult {
                    valid: false,
                    errors: vec![ValidationError {
                        field: Some("invalid".to_string()),
                        message: "field is not allowed".to_string(),
                        errors: vec![],
                    }],
                    requires_approval: false,
                })
            } else {
                Some(ValidationResult {
                    valid: true,
                    errors: vec![],
                    requires_approval: false,
                })
            }
        }
    }

    #[tokio::test]
    async fn test_mock_executor_call_async() {
        let executor = MockExecutor;
        let result = executor
            .call_async("test.module", &json!({"key": "value"}), None, None)
            .await
            .unwrap();
        assert_eq!(result["module"], "test.module");
        assert_eq!(result["echo"]["key"], "value");
    }

    #[test]
    fn test_mock_executor_stream_none() {
        let executor = MockExecutor;
        let stream = executor.stream("test.module", &json!({}), None, None);
        assert!(stream.is_none());
    }

    #[test]
    fn test_mock_executor_stream_some() {
        let executor = FullMockExecutor;
        let stream = executor.stream("test.module", &json!({}), None, None);
        assert!(stream.is_some());
    }

    #[tokio::test]
    async fn test_mock_executor_stream_yields_chunks() {
        use tokio_stream::StreamExt;

        let executor = FullMockExecutor;
        let mut stream = executor
            .stream("test.module", &json!({}), None, None)
            .unwrap();

        let chunk1 = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk1, json!({"a": 1}));

        let chunk2 = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk2, json!({"b": 2}));

        assert!(stream.next().await.is_none());
    }

    #[test]
    fn test_mock_executor_validate_none() {
        let executor = MockExecutor;
        let result = executor.validate("test.module", &json!({}), None);
        assert!(result.is_none());
    }

    #[test]
    fn test_mock_executor_validate_valid() {
        let executor = FullMockExecutor;
        let result = executor
            .validate("test.module", &json!({"name": "ok"}), None)
            .unwrap();
        assert!(result.valid);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_mock_executor_validate_invalid() {
        let executor = FullMockExecutor;
        let result = executor
            .validate("test.module", &json!({"invalid": true}), None)
            .unwrap();
        assert!(!result.valid);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].field, Some("invalid".to_string()));
    }

    #[test]
    fn test_executor_is_object_safe() {
        // Compile-time test: Box<dyn Executor> must compile.
        fn _assert_object_safe(_e: Box<dyn Executor>) {}
    }

    // ---- Task 2: deep_merge tests ----

    #[test]
    fn test_deep_merge_flat_objects() {
        let base = json!({"a": 1, "b": 2});
        let overlay = json!({"b": 3, "c": 4});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": 1, "b": 3, "c": 4}));
    }

    #[test]
    fn test_deep_merge_nested_objects() {
        let base = json!({"a": {"x": 1, "y": 2}});
        let overlay = json!({"a": {"y": 3, "z": 4}});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": {"x": 1, "y": 3, "z": 4}}));
    }

    #[test]
    fn test_deep_merge_overlay_overwrites_non_object() {
        let base = json!({"a": "string"});
        let overlay = json!({"a": {"nested": true}});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": {"nested": true}}));
    }

    #[test]
    fn test_deep_merge_base_dict_overlay_scalar() {
        let base = json!({"a": {"nested": true}});
        let overlay = json!({"a": "scalar"});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": "scalar"}));
    }

    #[test]
    fn test_deep_merge_empty_base() {
        let base = json!({});
        let overlay = json!({"a": 1});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": 1}));
    }

    #[test]
    fn test_deep_merge_empty_overlay() {
        let base = json!({"a": 1});
        let overlay = json!({});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": 1}));
    }

    #[test]
    fn test_deep_merge_both_empty() {
        let base = json!({});
        let overlay = json!({});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_deep_merge_depth_cap() {
        // At depth 32, should do flat merge (overlay wins for conflicting keys)
        let base = json!({"a": {"inner": "base"}});
        let overlay = json!({"a": {"inner": "overlay", "extra": true}});
        let result = deep_merge(&base, &overlay, DEEP_MERGE_MAX_DEPTH);
        // At max depth, the entire overlay value for "a" wins (flat merge of top-level keys)
        assert_eq!(result["a"]["inner"], "overlay");
        assert_eq!(result["a"]["extra"], true);
    }

    #[test]
    fn test_deep_merge_depth_31_still_recurses() {
        // At depth 31 (one below max), recursion still happens
        let base = json!({"a": {"x": 1, "y": 2}});
        let overlay = json!({"a": {"y": 3}});
        let result = deep_merge(&base, &overlay, 31);
        // Should recurse into "a" and merge
        assert_eq!(result, json!({"a": {"x": 1, "y": 3}}));
    }

    #[test]
    fn test_deep_merge_non_object_inputs() {
        // When base is not an object, overlay wins
        let result = deep_merge(&json!("string"), &json!(42), 0);
        assert_eq!(result, json!(42));

        let result = deep_merge(&json!([1, 2]), &json!([3, 4]), 0);
        assert_eq!(result, json!([3, 4]));
    }

    #[test]
    fn test_deep_merge_three_levels_deep() {
        let base = json!({"a": {"b": {"c": 1, "d": 2}}});
        let overlay = json!({"a": {"b": {"d": 3, "e": 4}}});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"a": {"b": {"c": 1, "d": 3, "e": 4}}}));
    }

    #[test]
    fn test_deep_merge_array_not_merged() {
        // Arrays are overwritten, not concatenated
        let base = json!({"items": [1, 2, 3]});
        let overlay = json!({"items": [4, 5]});
        let result = deep_merge(&base, &overlay, 0);
        assert_eq!(result, json!({"items": [4, 5]}));
    }

    // ---- Task 3: Output formatting tests ----

    #[test]
    fn test_format_result_default_json() {
        let router = ExecutionRouter::stub();
        let result = router.format_result(&json!({"key": "value"}));
        assert_eq!(result, r#"{"key":"value"}"#);
    }

    #[test]
    fn test_format_result_default_string() {
        let router = ExecutionRouter::stub();
        let result = router.format_result(&json!("hello"));
        assert_eq!(result, r#""hello""#);
    }

    #[test]
    fn test_new_with_formatter_preserves_settings() {
        let formatter: OutputFormatter = Box::new(|val| {
            let obj = val.as_object().unwrap();
            Ok(format!("keys={}", obj.len()))
        });
        let router = ExecutionRouter::new_with_formatter(true, Some(formatter));
        assert!(router.validate_inputs);
        assert!(router.executor.is_none());
        assert_eq!(router.format_result(&json!({"a": 1})), "keys=1");
    }

    #[test]
    fn test_new_with_formatter_none() {
        let router = ExecutionRouter::new_with_formatter(false, None);
        assert!(!router.validate_inputs);
        assert!(router.output_formatter.is_none());
        assert_eq!(router.format_result(&json!(42)), "42");
    }

    #[test]
    fn test_format_result_default_number() {
        let router = ExecutionRouter::stub();
        let result = router.format_result(&json!(42));
        assert_eq!(result, "42");
    }

    #[test]
    fn test_format_result_default_null() {
        let router = ExecutionRouter::stub();
        let result = router.format_result(&Value::Null);
        assert_eq!(result, "null");
    }

    #[test]
    fn test_format_result_custom_formatter() {
        let formatter: OutputFormatter = Box::new(|val| {
            let obj = val.as_object().unwrap();
            Ok(format!("custom: {} keys", obj.len()))
        });
        let router = ExecutionRouter {
            executor: None,
            validate_inputs: false,
            redact_output: false,
            trace: false,
            output_formatter: Some(formatter),
            output_format: OutputFormat::Json,
            tool_schemas: HashMap::new(),
            output_schemas: HashMap::new(),
            async_bridge: None,
            approval_bridge: None,
            cancel_tokens: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_registry: Arc::new(SessionRegistry::new()),
        };
        let result = router.format_result(&json!({"a": 1, "b": 2}));
        assert_eq!(result, "custom: 2 keys");
    }

    #[test]
    fn test_format_result_custom_formatter_non_object_ignored() {
        let formatter: OutputFormatter = Box::new(|_val| Ok("should not be called".to_string()));
        let router = ExecutionRouter {
            executor: None,
            validate_inputs: false,
            redact_output: false,
            trace: false,
            output_formatter: Some(formatter),
            output_format: OutputFormat::Json,
            tool_schemas: HashMap::new(),
            output_schemas: HashMap::new(),
            async_bridge: None,
            approval_bridge: None,
            cancel_tokens: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_registry: Arc::new(SessionRegistry::new()),
        };
        // Non-object values should fall back to JSON
        assert_eq!(router.format_result(&json!("string")), r#""string""#);
        assert_eq!(router.format_result(&json!(123)), "123");
        assert_eq!(router.format_result(&json!([1, 2])), "[1,2]");
    }

    #[test]
    fn test_format_result_custom_formatter_error_fallback() {
        let formatter: OutputFormatter = Box::new(|_val| Err("formatter exploded".into()));
        let router = ExecutionRouter {
            executor: None,
            validate_inputs: false,
            redact_output: false,
            trace: false,
            output_formatter: Some(formatter),
            output_format: OutputFormat::Json,
            tool_schemas: HashMap::new(),
            output_schemas: HashMap::new(),
            async_bridge: None,
            approval_bridge: None,
            cancel_tokens: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_registry: Arc::new(SessionRegistry::new()),
        };
        // Should fall back to JSON when formatter returns an error
        let result = router.format_result(&json!({"key": "value"}));
        assert_eq!(result, r#"{"key":"value"}"#);
    }

    // ==================================================================
    // Task 4: Context construction tests
    // ==================================================================

    fn make_send_notification() -> (SendNotificationFn, Arc<std::sync::Mutex<Vec<Value>>>) {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let captured_clone = captured.clone();
        let sn: SendNotificationFn = Arc::new(move |val| {
            let captured = captured_clone.clone();
            Box::pin(async move {
                captured.lock().unwrap().push(val);
                Ok(())
            })
        });
        (sn, captured)
    }

    struct MockSession {
        result: ElicitResult,
    }

    #[async_trait]
    impl SessionHandle for MockSession {
        async fn elicit_form(
            &self,
            _message: &str,
            _requested_schema: &Value,
        ) -> Result<ElicitResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.result.clone())
        }
    }

    #[allow(dead_code)]
    struct FailingSession;

    #[async_trait]
    impl SessionHandle for FailingSession {
        async fn elicit_form(
            &self,
            _message: &str,
            _requested_schema: &Value,
        ) -> Result<ElicitResult, Box<dyn std::error::Error + Send + Sync>> {
            Err("session error".into())
        }
    }

    #[test]
    fn test_build_context_with_progress_callback() {
        let (sn, _) = make_send_notification();
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok-1".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(data.contains_key(MCP_PROGRESS_KEY));
    }

    #[test]
    fn test_build_context_without_progress() {
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(!data.contains_key(MCP_PROGRESS_KEY));
    }

    #[test]
    fn test_build_context_with_elicit_callback() {
        let session = Arc::new(MockSession {
            result: ElicitResult {
                action: crate::helpers::ElicitAction::Accept,
                content: None,
            },
        });
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: Some(session),
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(data.contains_key(MCP_ELICIT_KEY));
    }

    #[test]
    fn test_build_context_without_session() {
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(!data.contains_key(MCP_ELICIT_KEY));
    }

    #[test]
    fn test_build_context_with_identity() {
        let identity = json!({"id": "user-1", "type": "user"});
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: Some(identity.clone()),
            typed_identity: None,
        };
        let (ctx, _, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert_eq!(ctx["identity"], identity);
    }

    #[test]
    fn test_build_context_without_identity() {
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (ctx, _, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(ctx.get("identity").is_none());
    }

    // ==================================================================
    // apcore::Context construction and identity propagation tests
    // ==================================================================

    #[test]
    fn test_build_context_creates_apcore_context_anonymous() {
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(apcore_ctx.identity.is_none());
        assert!(!apcore_ctx.trace_id.is_empty());
    }

    #[test]
    fn test_build_context_creates_apcore_context_with_typed_identity() {
        let identity = apcore::Identity::new(
            "user-42".to_string(),
            "human".to_string(),
            vec!["admin".to_string()],
            Default::default(),
        );
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: Some(identity.clone()),
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let ctx_id = apcore_ctx
            .identity
            .as_ref()
            .expect("identity should be set");
        assert_eq!(ctx_id.id(), "user-42");
        assert_eq!(ctx_id.identity_type(), "human");
        assert_eq!(ctx_id.roles(), vec!["admin"]);
    }

    #[test]
    fn test_build_context_deserializes_json_identity_into_apcore_context() {
        let identity_json = json!({
            "id": "svc-1",
            "type": "service",
            "roles": ["reader"]
        });
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: Some(identity_json),
            typed_identity: None,
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let ctx_id = apcore_ctx
            .identity
            .as_ref()
            .expect("identity should be set");
        assert_eq!(ctx_id.id(), "svc-1");
        assert_eq!(ctx_id.identity_type(), "service");
    }

    #[test]
    fn test_build_context_typed_identity_takes_precedence() {
        let typed = apcore::Identity::new(
            "typed-user".to_string(),
            "human".to_string(),
            vec![],
            Default::default(),
        );
        let json_identity = json!({
            "id": "json-user",
            "type": "service",
            "roles": []
        });
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: Some(json_identity),
            typed_identity: Some(typed),
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let ctx_id = apcore_ctx.identity.as_ref().unwrap();
        assert_eq!(
            ctx_id.id(),
            "typed-user",
            "typed_identity should take precedence"
        );
    }

    #[test]
    fn test_build_context_writes_progress_marker_to_apcore_data() {
        let (sn, _) = make_send_notification();
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let data = apcore_ctx.data.read();
        assert_eq!(data.get(MCP_PROGRESS_KEY), Some(&json!("available")));
        assert!(!data.contains_key(MCP_ELICIT_KEY));
    }

    #[test]
    fn test_build_context_writes_elicit_marker_to_apcore_data() {
        let session = Arc::new(MockSession {
            result: ElicitResult {
                action: crate::helpers::ElicitAction::Accept,
                content: None,
            },
        });
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: Some(session),
            identity: None,
            typed_identity: None,
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let data = apcore_ctx.data.read();
        assert_eq!(data.get(MCP_ELICIT_KEY), Some(&json!("available")));
        assert!(!data.contains_key(MCP_PROGRESS_KEY));
    }

    #[test]
    fn test_build_context_no_markers_when_no_callbacks() {
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let data = apcore_ctx.data.read();
        assert!(data.is_empty());
    }

    #[test]
    fn test_build_context_trace_id_consistent() {
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (ctx_value, _, apcore_ctx, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let json_trace = ctx_value["trace_id"].as_str().unwrap();
        assert_eq!(json_trace, &apcore_ctx.trace_id);
    }

    // ==================================================================
    // redact_sensitive logging tests
    // ==================================================================

    #[test]
    fn test_redact_inputs_no_schema() {
        let router = ExecutionRouter::stub();
        let inputs = json!({"password": "secret", "name": "Alice"});
        let redacted = router.redact_inputs("unknown_tool", &inputs);
        // Without schema, inputs are returned as-is
        assert_eq!(redacted, inputs);
    }

    #[test]
    fn test_redact_inputs_with_sensitive_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "password": {"type": "string", "x-sensitive": true},
                "name": {"type": "string"}
            }
        });
        let mut schemas = HashMap::new();
        schemas.insert("my_tool".to_string(), schema);

        let router = ExecutionRouter::stub().with_tool_schemas(schemas);
        let inputs = json!({"password": "secret123", "name": "Alice"});
        let redacted = router.redact_inputs("my_tool", &inputs);

        assert_eq!(redacted["name"], "Alice");
        assert_eq!(redacted["password"], apcore::REDACTED_VALUE);
    }

    #[test]
    fn test_redact_inputs_secret_prefix_key() {
        let schema = json!({"type": "object", "properties": {}});
        let mut schemas = HashMap::new();
        schemas.insert("my_tool".to_string(), schema);

        let router = ExecutionRouter::stub().with_tool_schemas(schemas);
        let inputs = json!({"_secret_token": "abc", "name": "Bob"});
        let redacted = router.redact_inputs("my_tool", &inputs);

        assert_eq!(redacted["name"], "Bob");
        assert_eq!(redacted["_secret_token"], apcore::REDACTED_VALUE);
    }

    #[test]
    fn test_with_tool_schemas_builder() {
        let mut schemas = HashMap::new();
        schemas.insert("tool_a".to_string(), json!({"type": "object"}));
        let router = ExecutionRouter::stub().with_tool_schemas(schemas);
        assert!(router.tool_schemas.contains_key("tool_a"));
    }

    #[tokio::test]
    async fn test_progress_callback_sends_notification() {
        let (sn, captured) = make_send_notification();
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok-1".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let cb = data
            .get(MCP_PROGRESS_KEY)
            .unwrap()
            .downcast_ref::<crate::helpers::ProgressCallback>()
            .unwrap();
        cb(0.5, Some(1.0), Some("halfway".into())).await;

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        let notif = &notifications[0];
        assert_eq!(notif["method"], "notifications/progress");
        assert_eq!(notif["params"]["progressToken"], "tok-1");
        assert_eq!(notif["params"]["message"], "halfway");
    }

    #[tokio::test]
    async fn test_progress_callback_includes_message() {
        let (sn, captured) = make_send_notification();
        let extra = CallExtra {
            progress_token: Some(ProgressToken::Integer(42)),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let cb = data
            .get(MCP_PROGRESS_KEY)
            .unwrap()
            .downcast_ref::<crate::helpers::ProgressCallback>()
            .unwrap();
        cb(1.0, None, Some("doing stuff".into())).await;

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications[0]["params"]["message"], "doing stuff");
    }

    #[tokio::test]
    async fn test_progress_callback_omits_message() {
        let (sn, captured) = make_send_notification();
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let cb = data
            .get(MCP_PROGRESS_KEY)
            .unwrap()
            .downcast_ref::<crate::helpers::ProgressCallback>()
            .unwrap();
        cb(1.0, None, None).await;

        let notifications = captured.lock().unwrap();
        assert!(notifications[0]["params"].get("message").is_none());
    }

    #[tokio::test]
    async fn test_elicit_callback_returns_result() {
        use crate::helpers::ElicitAction;
        let session = Arc::new(MockSession {
            result: ElicitResult {
                action: ElicitAction::Accept,
                content: Some(json!({"name": "Alice"})),
            },
        });
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: Some(session),
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        let cb = data
            .get(MCP_ELICIT_KEY)
            .unwrap()
            .downcast_ref::<crate::helpers::ElicitCallback>()
            .unwrap();
        let result = cb("confirm?".into(), None).await.unwrap();
        assert_eq!(result.action, ElicitAction::Accept);
        assert_eq!(result.content.unwrap()["name"], "Alice");
    }

    // ==================================================================
    // Task 5: Non-streaming path tests
    // ==================================================================

    /// A mock executor that always fails.
    struct FailingExecutor {
        error: ExecutorError,
    }

    impl FailingExecutor {
        fn new(code: &str, message: &str) -> Self {
            Self {
                error: ExecutorError::Execution {
                    code: code.to_string(),
                    message: message.to_string(),
                    details: None,
                },
            }
        }

        fn with_guidance(code: &str, message: &str, details: Value) -> Self {
            Self {
                error: ExecutorError::Execution {
                    code: code.to_string(),
                    message: message.to_string(),
                    details: Some(details),
                },
            }
        }
    }

    #[async_trait]
    impl Executor for FailingExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            // Clone the error fields manually since ExecutorError doesn't impl Clone
            match &self.error {
                ExecutorError::Execution {
                    code,
                    message,
                    details,
                } => Err(ExecutorError::Execution {
                    code: code.clone(),
                    message: message.clone(),
                    details: details.clone(),
                }),
                _ => Err(ExecutorError::Other("unknown".into())),
            }
        }
    }

    #[tokio::test]
    async fn test_call_async_success() {
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let (content, is_error, _trace_id) = router
            .handle_call_async_with_hint("test.module", &json!({"key": "value"}), None, None, None)
            .await;
        assert!(!is_error);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].content_type, "text");
        // Result should be JSON-formatted
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["module"], "test.module");
        assert_eq!(parsed["echo"]["key"], "value");
    }

    #[tokio::test]
    async fn test_call_async_success_with_trace_id() {
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let ctx = json!({"trace_id": "trace-abc-123"});
        let (_content, is_error, trace_id) = router
            .handle_call_async_with_hint("test.module", &json!({}), Some(&ctx), None, None)
            .await;
        assert!(!is_error);
        assert_eq!(trace_id, Some("trace-abc-123".to_string()));
    }

    #[tokio::test]
    async fn test_call_async_success_custom_formatter() {
        let formatter: OutputFormatter = Box::new(|val| {
            let obj = val.as_object().unwrap();
            Ok(format!("keys={}", obj.len()))
        });
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, Some(formatter));
        let (content, is_error, _) = router
            .handle_call_async_with_hint("test.module", &json!({"a": 1}), None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert_eq!(text, "keys=2"); // "module" and "echo"
    }

    #[tokio::test]
    async fn test_call_async_error_mapped() {
        let router = ExecutionRouter::new(
            Box::new(FailingExecutor::new(
                "MODULE_EXECUTE_ERROR",
                "division by zero",
            )),
            false,
            None,
        );
        let (content, is_error, _) = router
            .handle_call_async_with_hint("test.module", &json!({}), None, None, None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("division by zero"));
    }

    #[tokio::test]
    async fn test_call_async_error_text_includes_message() {
        let router = ExecutionRouter::new(
            Box::new(FailingExecutor::new("ERR", "something went wrong")),
            false,
            None,
        );
        let (content, is_error, _) = router
            .handle_call_async_with_hint("mod", &json!({}), None, None, None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("something went wrong"));
    }

    #[tokio::test]
    async fn test_call_async_error_is_error_true() {
        let router =
            ExecutionRouter::new(Box::new(FailingExecutor::new("ERR", "fail")), false, None);
        let (_, is_error, _) = router
            .handle_call_async_with_hint("mod", &json!({}), None, None, None)
            .await;
        assert!(is_error);
    }

    #[tokio::test]
    async fn test_call_async_error_no_trace_id() {
        let router =
            ExecutionRouter::new(Box::new(FailingExecutor::new("ERR", "fail")), false, None);
        let (_, _, trace_id) = router
            .handle_call_async_with_hint("mod", &json!({}), None, None, None)
            .await;
        assert!(trace_id.is_none());
    }

    #[tokio::test]
    async fn test_call_async_error_with_ai_guidance() {
        let details = json!({
            "retryable": true,
            "aiGuidance": "Try with smaller input",
        });
        let router = ExecutionRouter::new(
            Box::new(FailingExecutor::with_guidance("ERR", "too large", details)),
            false,
            None,
        );
        let (content, is_error, _) = router
            .handle_call_async_with_hint("mod", &json!({}), None, None, None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("too large"));
        assert!(text.contains("retryable"));
        assert!(text.contains("Try with smaller input"));
    }

    #[test]
    fn test_build_error_text_simple() {
        let error = ExecutorError::Execution {
            code: "ERR".into(),
            message: "Something broke".into(),
            details: None,
        };
        let text = ExecutionRouter::build_error_text(&error);
        assert_eq!(text, "Something broke");
    }

    #[test]
    fn test_build_error_text_with_retryable() {
        let error = ExecutorError::Execution {
            code: "ERR".into(),
            message: "Temporary failure".into(),
            details: Some(json!({"retryable": true})),
        };
        let text = ExecutionRouter::build_error_text(&error);
        assert!(text.starts_with("Temporary failure"));
        assert!(text.contains(r#""retryable":true"#));
    }

    #[test]
    fn test_build_error_text_with_all_guidance() {
        let error = ExecutorError::Execution {
            code: "ERR".into(),
            message: "Failed".into(),
            details: Some(json!({
                "retryable": true,
                "aiGuidance": "reduce input",
                "userFixable": false,
                "suggestion": "try again"
            })),
        };
        let text = ExecutionRouter::build_error_text(&error);
        assert!(text.starts_with("Failed\n\n"));
        assert!(text.contains("retryable"));
        assert!(text.contains("aiGuidance"));
        assert!(text.contains("userFixable"));
        assert!(text.contains("suggestion"));
    }

    // ==================================================================
    // Task 6: Streaming path tests
    // ==================================================================

    /// Mock executor that streams specific chunks.
    struct StreamingMockExecutor {
        chunks: Vec<Result<Value, ExecutorError>>,
    }

    #[async_trait]
    impl Executor for StreamingMockExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            Ok(json!({"fallback": true}))
        }

        fn stream(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Option<StreamResult> {
            // We need to clone the data for the stream
            let chunks: Vec<Result<Value, ExecutorError>> = self
                .chunks
                .iter()
                .map(|r| match r {
                    Ok(v) => Ok(v.clone()),
                    Err(ExecutorError::Execution {
                        code,
                        message,
                        details,
                    }) => Err(ExecutorError::Execution {
                        code: code.clone(),
                        message: message.clone(),
                        details: details.clone(),
                    }),
                    _ => Err(ExecutorError::Other("clone error".into())),
                })
                .collect();
            Some(Box::pin(tokio_stream::iter(chunks)))
        }
    }

    /// Mock executor that returns None for stream (no streaming support).
    struct NonStreamingExecutor;

    #[async_trait]
    impl Executor for NonStreamingExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            Ok(json!({"non_streaming": true}))
        }
        // stream() defaults to None
    }

    #[tokio::test]
    async fn test_stream_single_chunk() {
        let (sn, captured) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({"result": 42}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok-1".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["result"], 42);

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 1);
    }

    #[tokio::test]
    async fn test_stream_multiple_chunks_merged() {
        let (sn, captured) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![
                Ok(json!({"a": 1})),
                Ok(json!({"b": 2})),
                Ok(json!({"c": 3})),
            ],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
        assert_eq!(parsed["c"], 3);

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 3);
    }

    #[tokio::test]
    async fn test_stream_empty() {
        let (sn, captured) = make_send_notification();
        let executor = StreamingMockExecutor { chunks: vec![] };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed, json!({}));

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 0);
    }

    #[tokio::test]
    async fn test_stream_progress_notification_structure() {
        let (sn, captured) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({"key": "val"}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("my-token".into());
        router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;

        let notifications = captured.lock().unwrap();
        let notif = &notifications[0];
        assert_eq!(notif["method"], "notifications/progress");
        assert_eq!(notif["params"]["progressToken"], "my-token");
        assert_eq!(notif["params"]["progress"], 1); // 1-indexed
        assert!(notif["params"]["total"].is_null());
        // message is the JSON serialized chunk
        let msg = notif["params"]["message"].as_str().unwrap();
        let chunk: Value = serde_json::from_str(msg).unwrap();
        assert_eq!(chunk["key"], "val");
    }

    #[tokio::test]
    async fn test_stream_progress_token_string() {
        let (sn, captured) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("str-tok".into());
        router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications[0]["params"]["progressToken"], "str-tok");
    }

    #[tokio::test]
    async fn test_stream_progress_token_integer() {
        let (sn, captured) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::Integer(99);
        router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications[0]["params"]["progressToken"], 99);
    }

    #[tokio::test]
    async fn test_stream_accumulates_nested() {
        let (sn, _) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({"data": {"x": 1}})), Ok(json!({"data": {"y": 2}}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["data"]["x"], 1);
        assert_eq!(parsed["data"]["y"], 2);
    }

    #[tokio::test]
    async fn test_stream_error_mid_stream() {
        let (sn, _) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![
                Ok(json!({"a": 1})),
                Err(ExecutorError::Execution {
                    code: "ERR".into(),
                    message: "stream broke".into(),
                    details: None,
                }),
                Ok(json!({"c": 3})),
            ],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("stream broke"));
    }

    #[tokio::test]
    async fn test_stream_error_is_error_true() {
        let (sn, _) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Err(ExecutorError::Execution {
                code: "ERR".into(),
                message: "fail".into(),
                details: None,
            })],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok".into());
        let (_, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(is_error);
    }

    #[tokio::test]
    async fn test_stream_result_formatted() {
        let (sn, _) = make_send_notification();
        let formatter: OutputFormatter = Box::new(|val| {
            let obj = val.as_object().unwrap();
            Ok(format!("formatted:{}", obj.len()))
        });
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({"a": 1})), Ok(json!({"b": 2}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, Some(formatter));
        let token = ProgressToken::String("tok".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert_eq!(text, "formatted:2");
    }

    #[tokio::test]
    async fn test_stream_result_has_trace_id() {
        let (sn, _) = make_send_notification();
        let executor = StreamingMockExecutor {
            chunks: vec![Ok(json!({"a": 1}))],
        };
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let token = ProgressToken::String("tok".into());
        let ctx = json!({"trace_id": "trace-xyz"});
        let (_, is_error, trace_id) = router
            .handle_stream("mod", &json!({}), &token, &sn, Some(&ctx), None, None)
            .await;
        assert!(!is_error);
        assert_eq!(trace_id, Some("trace-xyz".to_string()));
    }

    #[tokio::test]
    async fn test_stream_fallback_to_non_streaming() {
        let (sn, captured) = make_send_notification();
        let router = ExecutionRouter::new(Box::new(NonStreamingExecutor), false, None);
        let token = ProgressToken::String("tok".into());
        let (content, is_error, _) = router
            .handle_stream("mod", &json!({}), &token, &sn, None, None, None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["non_streaming"], true);

        // No progress notifications should be sent (fell back)
        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 0);
    }

    // ==================================================================
    // Task 7: Input validation tests
    // ==================================================================

    #[test]
    fn test_format_validation_errors_single_field() {
        let errors = vec![ValidationError {
            field: Some("name".to_string()),
            message: "is required".to_string(),
            errors: vec![],
        }];
        let result = ExecutionRouter::format_validation_errors(&errors);
        assert_eq!(result, "name: is required");
    }

    #[test]
    fn test_format_validation_errors_multiple_fields() {
        let errors = vec![
            ValidationError {
                field: Some("name".to_string()),
                message: "is required".to_string(),
                errors: vec![],
            },
            ValidationError {
                field: Some("age".to_string()),
                message: "must be positive".to_string(),
                errors: vec![],
            },
        ];
        let result = ExecutionRouter::format_validation_errors(&errors);
        assert_eq!(result, "name: is required; age: must be positive");
    }

    #[test]
    fn test_format_validation_errors_nested() {
        let errors = vec![ValidationError {
            field: Some("address".to_string()),
            message: "has errors".to_string(),
            errors: vec![
                ValidationError {
                    field: Some("street".to_string()),
                    message: "is required".to_string(),
                    errors: vec![],
                },
                ValidationError {
                    field: Some("zip".to_string()),
                    message: "is invalid".to_string(),
                    errors: vec![],
                },
            ],
        }];
        let result = ExecutionRouter::format_validation_errors(&errors);
        assert_eq!(result, "street: is required; zip: is invalid");
    }

    #[test]
    fn test_format_validation_errors_no_field() {
        let errors = vec![ValidationError {
            field: None,
            message: "general error".to_string(),
            errors: vec![],
        }];
        let result = ExecutionRouter::format_validation_errors(&errors);
        assert_eq!(result, "general error");
    }

    #[test]
    fn test_format_validation_errors_nested_no_field() {
        let errors = vec![ValidationError {
            field: None,
            message: "parent".to_string(),
            errors: vec![ValidationError {
                field: None,
                message: "child error".to_string(),
                errors: vec![],
            }],
        }];
        let result = ExecutionRouter::format_validation_errors(&errors);
        assert_eq!(result, "?: child error");
    }

    #[tokio::test]
    async fn test_validation_disabled_skips() {
        // With validate_inputs=false, executor.validate() is never relevant
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), false, None);
        // FullMockExecutor would return invalid for {"invalid": true}
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({"invalid": true}), None)
            .await;
        // Should NOT return validation error — validation disabled
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(!text.contains("Validation failed"));
    }

    #[tokio::test]
    async fn test_validation_enabled_valid_inputs() {
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), true, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({"name": "ok"}), None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(!text.contains("Validation failed"));
    }

    #[tokio::test]
    async fn test_validation_enabled_invalid_inputs() {
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), true, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({"invalid": true}), None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("Validation failed"));
        assert!(text.contains("invalid: field is not allowed"));
    }

    #[tokio::test]
    async fn test_validation_returns_is_error_true() {
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), true, None);
        let (_, is_error, trace_id) = router
            .handle_call_with_extra("mod", &json!({"invalid": true}), None)
            .await;
        assert!(is_error);
        assert!(trace_id.is_none());
    }

    #[tokio::test]
    async fn test_validation_executor_lacks_validate() {
        // MockExecutor returns None from validate() — validation should be skipped
        let router = ExecutionRouter::new(Box::new(MockExecutor), true, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("test.module", &json!({"anything": true}), None)
            .await;
        // Should proceed to execution since validate returns None
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(!text.contains("Validation failed"));
    }

    // ==================================================================
    // Task 8: handle_call orchestrator tests
    // ==================================================================

    #[test]
    fn test_new_default_formatter() {
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        assert!(router.output_formatter.is_none());
        assert!(!router.validate_inputs);
        assert!(router.executor.is_some());
    }

    #[test]
    fn test_new_custom_formatter() {
        let formatter: OutputFormatter = Box::new(|_| Ok("custom".into()));
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, Some(formatter));
        assert!(router.output_formatter.is_some());
    }

    #[test]
    fn test_new_validate_inputs_flag() {
        let router = ExecutionRouter::new(Box::new(MockExecutor), true, None);
        assert!(router.validate_inputs);
    }

    #[tokio::test]
    async fn test_handle_call_non_streaming() {
        // Without progress_token, routes to non-streaming path
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("test.module", &json!({"key": "val"}), None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["module"], "test.module");
    }

    #[tokio::test]
    async fn test_handle_call_streaming() {
        // With progress_token + send_notification + executor.stream(), routes to streaming
        let (sn, captured) = make_send_notification();
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;
        assert!(!is_error);
        // FullMockExecutor streams {"a":1}, {"b":2}
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 2);
    }

    #[tokio::test]
    async fn test_handle_call_streaming_missing_send_notification() {
        // With progress_token but no send_notification, falls back to non-streaming
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({"x": 1}), Some(extra))
            .await;
        assert!(!is_error);
        // Should fall back to call_async which returns inputs as-is for FullMockExecutor
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["x"], 1);
    }

    #[tokio::test]
    async fn test_handle_call_streaming_executor_no_stream() {
        // Executor doesn't support streaming, falls back to non-streaming
        let (sn, captured) = make_send_notification();
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("test.module", &json!({}), Some(extra))
            .await;
        assert!(!is_error);
        // MockExecutor has no stream(), should fall back
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["module"], "test.module");

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 0);
    }

    #[tokio::test]
    async fn test_handle_call_validation_before_execution() {
        // With validate_inputs=true, validation runs before execution
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), true, None);
        let (_, is_error, _) = router
            .handle_call_with_extra("mod", &json!({"name": "ok"}), None)
            .await;
        assert!(!is_error);
    }

    #[tokio::test]
    async fn test_handle_call_validation_failure_short_circuits() {
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), true, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({"invalid": true}), None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("Validation failed"));
    }

    #[tokio::test]
    async fn test_handle_call_passes_identity() {
        // Identity from CallExtra is passed to context
        let identity = json!({"id": "user-1"});
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: Some(identity),
            typed_identity: None,
        };
        // Use IdentityCapturingExecutor to verify
        let router = ExecutionRouter::new(Box::new(IdentityCapturingExecutor), false, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["identity"]["id"], "user-1");
    }

    #[tokio::test]
    async fn test_handle_call_no_extra() {
        // None extra works correctly (non-streaming, no callbacks)
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let (content, is_error, _) = router
            .handle_call_with_extra("test.module", &json!({"k": "v"}), None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("test.module"));
    }

    /// Executor that captures the context identity.
    struct IdentityCapturingExecutor;

    #[async_trait]
    impl Executor for IdentityCapturingExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &Value,
            context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            // Return the context so test can inspect it
            Ok(context.cloned().unwrap_or(Value::Null))
        }
    }

    // ==================================================================
    // Task 8: handle_call (Value-based) orchestrator tests
    // ==================================================================

    #[tokio::test]
    async fn test_handle_call_value_non_streaming() {
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let (content, is_error, _) = router
            .handle_call("test.module", &json!({"key": "val"}), None)
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["module"], "test.module");
    }

    #[tokio::test]
    async fn test_handle_call_value_with_identity() {
        let router = ExecutionRouter::new(Box::new(IdentityCapturingExecutor), false, None);
        let extra = json!({"identity": {"id": "user-2"}});
        let (content, is_error, _) = router.handle_call("mod", &json!({}), Some(&extra)).await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["identity"]["id"], "user-2");
    }

    #[tokio::test]
    async fn test_handle_call_value_validation_failure() {
        let router = ExecutionRouter::new(Box::new(FullMockExecutor), true, None);
        let (content, is_error, _) = router
            .handle_call("mod", &json!({"invalid": true}), None)
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("Validation failed"));
    }

    // ==================================================================
    // Task 9: Integration tests (end-to-end)
    // ==================================================================

    /// Configurable mock executor for integration tests.
    struct ConfigurableExecutor {
        call_result: std::sync::Mutex<Option<Result<Value, ExecutorError>>>,
        stream_chunks: std::sync::Mutex<Option<Vec<Result<Value, ExecutorError>>>>,
        validate_result: std::sync::Mutex<Option<ValidationResult>>,
        calls: std::sync::Mutex<Vec<(String, Value)>>,
    }

    impl ConfigurableExecutor {
        fn new() -> Self {
            Self {
                call_result: std::sync::Mutex::new(None),
                stream_chunks: std::sync::Mutex::new(None),
                validate_result: std::sync::Mutex::new(None),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_call_result(self, result: Result<Value, ExecutorError>) -> Self {
            *self.call_result.lock().unwrap() = Some(result);
            self
        }

        fn with_stream_chunks(self, chunks: Vec<Result<Value, ExecutorError>>) -> Self {
            *self.stream_chunks.lock().unwrap() = Some(chunks);
            self
        }

        fn with_validate_result(self, result: ValidationResult) -> Self {
            *self.validate_result.lock().unwrap() = Some(result);
            self
        }

        #[allow(dead_code)]
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Executor for ConfigurableExecutor {
        async fn call_async(
            &self,
            module_id: &str,
            inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            self.calls
                .lock()
                .unwrap()
                .push((module_id.to_string(), inputs.clone()));
            match self.call_result.lock().unwrap().take() {
                Some(result) => match result {
                    Ok(v) => Ok(v),
                    Err(ExecutorError::Execution {
                        code,
                        message,
                        details,
                    }) => Err(ExecutorError::Execution {
                        code,
                        message,
                        details,
                    }),
                    Err(ExecutorError::Validation(msg)) => Err(ExecutorError::Validation(msg)),
                    Err(e) => Err(e),
                },
                None => Ok(inputs.clone()),
            }
        }

        fn stream(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Option<StreamResult> {
            let chunks = self.stream_chunks.lock().unwrap().take()?;
            let cloned: Vec<Result<Value, ExecutorError>> = chunks
                .into_iter()
                .map(|r| match r {
                    Ok(v) => Ok(v),
                    Err(ExecutorError::Execution {
                        code,
                        message,
                        details,
                    }) => Err(ExecutorError::Execution {
                        code,
                        message,
                        details,
                    }),
                    Err(ExecutorError::Validation(msg)) => Err(ExecutorError::Validation(msg)),
                    Err(e) => Err(e),
                })
                .collect();
            Some(Box::pin(tokio_stream::iter(cloned)))
        }

        fn validate(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
        ) -> Option<ValidationResult> {
            self.validate_result.lock().unwrap().take()
        }
    }

    #[tokio::test]
    async fn test_e2e_simple_call() {
        let executor =
            ConfigurableExecutor::new().with_call_result(Ok(json!({"status": "ok", "count": 42})));
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let (content, is_error, trace_id) = router
            .handle_call_with_extra("my.tool", &json!({"input": 1}), None)
            .await;
        assert!(!is_error);
        assert!(trace_id.is_some()); // context always gets a trace_id
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["count"], 42);
    }

    #[tokio::test]
    async fn test_e2e_call_with_custom_formatter() {
        let executor = ConfigurableExecutor::new().with_call_result(Ok(json!({"x": 1, "y": 2})));
        let formatter: OutputFormatter = Box::new(|val| {
            let keys: Vec<&str> = val
                .as_object()
                .unwrap()
                .keys()
                .map(|k| k.as_str())
                .collect();
            Ok(format!("Keys: {}", keys.join(", ")))
        });
        let router = ExecutionRouter::new(Box::new(executor), false, Some(formatter));
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.starts_with("Keys: "));
    }

    #[tokio::test]
    async fn test_e2e_call_error_mapped() {
        let executor =
            ConfigurableExecutor::new().with_call_result(Err(ExecutorError::Execution {
                code: "ERR_DIVIDE".into(),
                message: "cannot divide by zero".into(),
                details: None,
            }));
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("cannot divide by zero"));
    }

    #[tokio::test]
    async fn test_e2e_call_with_identity() {
        let _executor = ConfigurableExecutor::new();
        let router = ExecutionRouter::new(Box::new(IdentityCapturingExecutor), false, None);
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: Some(json!({"user": "alice", "role": "admin"})),
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["identity"]["user"], "alice");
        assert_eq!(parsed["identity"]["role"], "admin");
    }

    #[tokio::test]
    async fn test_e2e_streaming_three_chunks() {
        let (sn, captured) = make_send_notification();
        let executor = ConfigurableExecutor::new().with_stream_chunks(vec![
            Ok(json!({"part": "a"})),
            Ok(json!({"data": 1})),
            Ok(json!({"part": "c", "data": 2})),
        ]);
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok-e2e".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["part"], "c"); // last "part" wins
        assert_eq!(parsed["data"], 2); // last "data" wins

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 3);
    }

    #[tokio::test]
    async fn test_e2e_streaming_notification_content() {
        let (sn, captured) = make_send_notification();
        let executor =
            ConfigurableExecutor::new().with_stream_chunks(vec![Ok(json!({"step": "done"}))]);
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok-notif".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        let notif = &notifications[0];
        assert_eq!(notif["method"], "notifications/progress");
        assert_eq!(notif["params"]["progressToken"], "tok-notif");
        assert_eq!(notif["params"]["progress"], 1);
        let msg = notif["params"]["message"].as_str().unwrap();
        let chunk: Value = serde_json::from_str(msg).unwrap();
        assert_eq!(chunk["step"], "done");
    }

    #[tokio::test]
    async fn test_e2e_streaming_error_mid_stream() {
        let (sn, _) = make_send_notification();
        let executor = ConfigurableExecutor::new().with_stream_chunks(vec![
            Ok(json!({"a": 1})),
            Err(ExecutorError::Execution {
                code: "STREAM_ERR".into(),
                message: "stream failed at chunk 2".into(),
                details: None,
            }),
        ]);
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("stream failed at chunk 2"));
    }

    #[tokio::test]
    async fn test_e2e_streaming_fallback_no_support() {
        // Executor without stream support falls back to non-streaming
        let (sn, captured) = make_send_notification();
        let executor = ConfigurableExecutor::new().with_call_result(Ok(json!({"fallback": true})));
        // Don't set stream_chunks — stream() returns None
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (content, is_error, _) = router
            .handle_call_with_extra("mod", &json!({}), Some(extra))
            .await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["fallback"], true);

        let notifications = captured.lock().unwrap();
        assert_eq!(notifications.len(), 0);
    }

    #[tokio::test]
    async fn test_e2e_validation_pass() {
        let executor = ConfigurableExecutor::new()
            .with_validate_result(ValidationResult {
                valid: true,
                errors: vec![],
                requires_approval: false,
            })
            .with_call_result(Ok(json!({"result": "success"})));
        let router = ExecutionRouter::new(Box::new(executor), true, None);
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["result"], "success");
    }

    #[tokio::test]
    async fn test_e2e_validation_fail() {
        let executor = ConfigurableExecutor::new()
            .with_validate_result(ValidationResult {
                valid: false,
                errors: vec![ValidationError {
                    field: Some("email".to_string()),
                    message: "is not a valid email".to_string(),
                    errors: vec![],
                }],
                requires_approval: false,
            })
            .with_call_result(Ok(json!({"should": "not reach"})));
        let router = ExecutionRouter::new(Box::new(executor), true, None);
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("Validation failed"));
        assert!(text.contains("email: is not a valid email"));
    }

    #[tokio::test]
    async fn test_e2e_validation_disabled() {
        // Even with invalid validation result, execution proceeds when disabled
        let executor = ConfigurableExecutor::new()
            .with_validate_result(ValidationResult {
                valid: false,
                errors: vec![ValidationError {
                    field: Some("x".to_string()),
                    message: "bad".to_string(),
                    errors: vec![],
                }],
                requires_approval: false,
            })
            .with_call_result(Ok(json!({"executed": true})));
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["executed"], true);
    }

    #[tokio::test]
    async fn test_e2e_no_extra() {
        let executor = ConfigurableExecutor::new().with_call_result(Ok(json!({"ok": true})));
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(!is_error);
        let text = content[0].data.as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["ok"], true);
    }

    #[tokio::test]
    async fn test_e2e_error_with_ai_guidance() {
        let executor =
            ConfigurableExecutor::new().with_call_result(Err(ExecutorError::Execution {
                code: "RATE_LIMIT".into(),
                message: "Too many requests".into(),
                details: Some(json!({
                    "retryable": true,
                    "aiGuidance": "Wait 10 seconds and retry",
                    "suggestion": "reduce request rate"
                })),
            }));
        let router = ExecutionRouter::new(Box::new(executor), false, None);
        let (content, is_error, _) = router.handle_call_with_extra("mod", &json!({}), None).await;
        assert!(is_error);
        let text = content[0].data.as_str().unwrap();
        assert!(text.contains("Too many requests"));
        assert!(text.contains("retryable"));
        assert!(text.contains("Wait 10 seconds and retry"));
        assert!(text.contains("suggestion"));
    }

    #[tokio::test]
    async fn test_e2e_progress_callback_in_context() {
        // Verify that progress callback is accessible via context data
        let (sn, _) = make_send_notification();
        let extra = CallExtra {
            progress_token: Some(ProgressToken::String("tok".into())),
            send_notification: Some(sn),
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(data.contains_key(MCP_PROGRESS_KEY));
    }

    #[tokio::test]
    async fn test_e2e_elicit_callback_in_context() {
        let session = Arc::new(MockSession {
            result: ElicitResult {
                action: crate::helpers::ElicitAction::Accept,
                content: None,
            },
        });
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: Some(session),
            identity: None,
            typed_identity: None,
        };
        let (_, data, _, _elicit_guard) = ExecutionRouter::build_context(&extra);
        assert!(data.contains_key(MCP_ELICIT_KEY));
    }

    // ==================================================================
    // Task 3: trace capture via call_with_trace
    // Task 4: version_hint passthrough
    // ==================================================================

    struct TracingExecutor;

    #[async_trait]
    impl Executor for TracingExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            Ok(inputs.clone())
        }

        async fn call_with_trace(
            &self,
            module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Option<Result<(Value, Value), ExecutorError>> {
            let trace = json!({
                "module_id": module_id,
                "strategy_name": "standard",
                "total_duration_ms": 1.5,
                "steps": [
                    {"name": "acl", "duration_ms": 0.1, "skipped": false},
                    {"name": "execute", "duration_ms": 1.0, "skipped": false},
                ],
            });
            Some(Ok((json!({"ok": true}), trace)))
        }
    }

    #[tokio::test]
    async fn test_trace_enabled_emits_trace_content_item() {
        let router = ExecutionRouter::new(Box::new(TracingExecutor), false, None).with_trace(true);
        let (content, is_error, _) = router.handle_call("mod", &json!({}), None).await;
        assert!(!is_error);
        let trace_item = content
            .iter()
            .find(|c| c.content_type == "trace")
            .expect("trace content item expected");
        let steps = trace_item
            .data
            .get("steps")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(steps.len(), 2);
    }

    struct VersionHintCapturingExecutor {
        captured: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Executor for VersionHintCapturingExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            *self.captured.lock().unwrap() = version_hint.map(|s| s.to_string());
            Ok(json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn test_version_hint_flows_through() {
        // [A-D-002 regression] Real MCP traffic carries the apcore version
        // hint at extras._meta.apcore.version — not at extras.apcore.version.
        // Verifies the `_meta` hop is honored in source #2 of the cascade.
        let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let exec = VersionHintCapturingExecutor {
            captured: Arc::clone(&captured),
        };
        let router = ExecutionRouter::new(Box::new(exec), false, None);
        let extras = json!({ "_meta": { "apcore": { "version": "2.0.1" } } });
        let (_content, is_error, _) = router.handle_call("mod", &json!({}), Some(&extras)).await;
        assert!(!is_error);
        assert_eq!(captured.lock().unwrap().clone(), Some("2.0.1".to_string()));
    }

    #[tokio::test]
    async fn test_version_hint_ignores_top_level_apcore() {
        // [A-D-002 regression] Confirms that putting the apcore stanza at
        // the TOP level (legacy shape that pre-fix Rust accidentally accepted)
        // is now correctly ignored — the cascade only honors `_meta.apcore.version`.
        let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let exec = VersionHintCapturingExecutor {
            captured: Arc::clone(&captured),
        };
        let router = ExecutionRouter::new(Box::new(exec), false, None);
        let extras = json!({ "apcore": { "version": "9.9.9" } });
        let (_content, is_error, _) = router.handle_call("mod", &json!({}), Some(&extras)).await;
        assert!(!is_error);
        assert_eq!(captured.lock().unwrap().clone(), None);
    }

    // ==================================================================
    // P0 #1: W3C traceparent ↔ MCP _meta.traceparent
    // ==================================================================

    #[test]
    fn extract_traceparent_parses_valid_header() {
        // [A-D-003 regression] Real MCP traffic nests traceparent under
        // extras._meta.traceparent — extract_traceparent must honor that hop.
        let extras = json!({
            "_meta": {
                "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            }
        });
        let tp = ExecutionRouter::extract_traceparent(Some(&extras)).expect("must parse");
        assert_eq!(tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tp.parent_id, "00f067aa0ba902b7");
    }

    #[test]
    fn extract_traceparent_none_when_missing() {
        let extras = json!({});
        assert!(ExecutionRouter::extract_traceparent(Some(&extras)).is_none());
    }

    #[test]
    fn extract_traceparent_none_when_top_level_only() {
        // [A-D-003 regression] Pre-fix Rust read traceparent at the top level
        // of extras — confirm the legacy shape is now correctly rejected.
        let extras = json!({
            "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        });
        assert!(ExecutionRouter::extract_traceparent(Some(&extras)).is_none());
    }

    #[test]
    fn extract_traceparent_none_when_malformed() {
        let extras = json!({ "_meta": { "traceparent": "not-a-valid-header" } });
        assert!(ExecutionRouter::extract_traceparent(Some(&extras)).is_none());
    }

    #[test]
    fn extract_traceparent_none_when_all_zero_trace_id() {
        let extras = json!({
            "_meta": {
                "traceparent": "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
            }
        });
        assert!(ExecutionRouter::extract_traceparent(Some(&extras)).is_none());
    }

    #[tokio::test]
    async fn build_context_with_trace_inherits_trace_id() {
        let extras = json!({
            "_meta": {
                "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            }
        });
        let tp = ExecutionRouter::extract_traceparent(Some(&extras));
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let (_, _, apcore_ctx, _elicit_guard) =
            ExecutionRouter::build_context_with_trace(&extra, tp, None);
        assert_eq!(apcore_ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[tokio::test]
    async fn build_context_with_trace_threads_cancel_token() {
        // [B-002] The cancel token registered for a call must reach the
        // Context so the executor pipeline and modules observe inbound MCP
        // `notifications/cancelled` (parity with apcore-mcp-python/-typescript).
        let extra = CallExtra {
            progress_token: None,
            send_notification: None,
            session: None,
            identity: None,
            typed_identity: None,
        };
        let token = CancelToken::new();
        let (_, _, apcore_ctx, _elicit_guard) =
            ExecutionRouter::build_context_with_trace(&extra, None, Some(token.clone()));
        let ctx_token = apcore_ctx
            .cancel_token
            .as_ref()
            .expect("cancel_token threaded into Context");
        assert!(!ctx_token.is_cancelled());
        // Cancelling via a sibling clone (as `cancel_call` does on the map's
        // token) flips the Context's token — shared `Arc<AtomicBool>` state.
        token.cancel();
        assert!(apcore_ctx.cancel_token.as_ref().unwrap().is_cancelled());
    }

    #[tokio::test]
    async fn handle_call_inherits_traceparent_trace_id() {
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None);
        let extras = json!({
            "_meta": {
                "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            }
        });
        let (_content, is_error, trace_id) =
            router.handle_call("mod", &json!({}), Some(&extras)).await;
        assert!(!is_error);
        assert_eq!(
            trace_id,
            Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string())
        );
    }

    // ==================================================================
    // P0 #2: AsyncTaskBridge routing from the router
    // ==================================================================

    fn make_apcore_executor() -> Arc<apcore::executor::Executor> {
        let registry = Arc::new(apcore::registry::registry::Registry::default());
        let config = Arc::new(apcore::config::Config::default());
        Arc::new(apcore::executor::Executor::new(registry, config))
    }

    /// Build an executor with a single async-hinted module pre-registered.
    /// Used by router tests that exercise the bridge's submit path post
    /// [A-D-008] (which now rejects non-async module ids).
    fn make_apcore_executor_with_async(module_id: &str) -> Arc<apcore::executor::Executor> {
        #[derive(Debug)]
        struct AsyncDummyModule;

        #[async_trait::async_trait]
        impl apcore::module::Module for AsyncDummyModule {
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            fn output_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn description(&self) -> &str {
                "async dummy"
            }
            async fn execute(
                &self,
                _inputs: Value,
                _ctx: &apcore::context::Context<Value>,
            ) -> Result<Value, apcore::errors::ModuleError> {
                Ok(json!({}))
            }
        }

        let registry = Arc::new(apcore::registry::registry::Registry::default());
        let config = Arc::new(apcore::config::Config::default());

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("async".to_string(), json!(true));
        let descriptor = apcore::registry::ModuleDescriptor {
            module_id: module_id.to_string(),
            name: None,
            description: "async dummy".to_string(),
            documentation: None,
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(apcore::module::ModuleAnnotations::default()),
            examples: vec![],
            metadata,
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };

        registry
            .register(module_id, Box::new(AsyncDummyModule), descriptor)
            .expect("register async module");

        Arc::new(apcore::executor::Executor::new(registry, config))
    }

    /// Build an executor whose single async-hinted module echoes the context
    /// identity, so a test can see what the bridge handed the task.
    fn make_apcore_executor_echoing_identity(module_id: &str) -> Arc<apcore::executor::Executor> {
        #[derive(Debug)]
        struct IdentityEchoModule;

        #[async_trait::async_trait]
        impl apcore::module::Module for IdentityEchoModule {
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            fn output_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn description(&self) -> &str {
                "identity echo"
            }
            async fn execute(
                &self,
                _inputs: Value,
                ctx: &apcore::context::Context<Value>,
            ) -> Result<Value, apcore::errors::ModuleError> {
                Ok(json!({
                    "identity_id": ctx.identity.as_ref().map(|i| i.id().to_string()),
                }))
            }
        }

        let registry = Arc::new(apcore::registry::registry::Registry::default());
        let config = Arc::new(apcore::config::Config::default());

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("async".to_string(), json!(true));
        let descriptor = apcore::registry::ModuleDescriptor {
            module_id: module_id.to_string(),
            name: None,
            description: "identity echo".to_string(),
            documentation: None,
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(apcore::module::ModuleAnnotations::default()),
            examples: vec![],
            metadata,
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry
            .register(module_id, Box::new(IdentityEchoModule), descriptor)
            .expect("register async module");
        Arc::new(apcore::executor::Executor::new(registry, config))
    }

    /// [A-D-FA-7] The async-bridge branch of `handle_call` resolved identity
    /// from `extra` alone, unlike `build_context_with_trace` which also falls
    /// back to the `AUTH_IDENTITY` task-local. A transport path that binds the
    /// task-local but passes no `extra` therefore submitted the task anonymous,
    /// where Python and TypeScript read their ContextVar / AsyncLocalStorage
    /// inside the handler and keep the identity.
    #[tokio::test]
    async fn async_bridge_submit_uses_the_ambient_identity() {
        use apcore::TaskStatus;

        let bridge = Arc::new(AsyncTaskBridge::new(make_apcore_executor_echoing_identity(
            "echo.module",
        )));
        let router = ExecutionRouter::new(Box::new(MockExecutor), false, None)
            .with_async_bridge(Arc::clone(&bridge));

        let identity = apcore::Identity::new(
            "ambient-user".to_string(),
            "user".to_string(),
            vec![],
            Default::default(),
        );

        let (content, is_error, _) = AUTH_IDENTITY
            .scope(Some(identity), async {
                router
                    .handle_call(
                        "__apcore_task_submit",
                        &json!({"module_id": "echo.module", "arguments": {}}),
                        None,
                    )
                    .await
            })
            .await;
        assert!(!is_error, "submit must succeed: {content:?}");

        let envelope: Value =
            serde_json::from_str(content[0].data.as_str().expect("string content")).unwrap();
        let task_id = envelope["task_id"].as_str().expect("task_id").to_string();

        // Poll until the task finishes; the module returns immediately.
        let mut info = None;
        for _ in 0..200 {
            match bridge.get_status(&task_id) {
                Some(i) if i.status == TaskStatus::Completed || i.status == TaskStatus::Failed => {
                    info = Some(i);
                    break;
                }
                _ => tokio::task::yield_now().await,
            }
        }
        let info = info.expect("task must reach a terminal state");
        assert_eq!(info.status, TaskStatus::Completed, "task failed: {info:?}");
        assert_eq!(
            info.result.as_ref().and_then(|r| r["identity_id"].as_str()),
            Some("ambient-user"),
            "the submitted task must carry the ambient identity; got: {:?}",
            info.result
        );
    }

    #[tokio::test]
    async fn meta_tool_submit_routes_via_async_bridge() {
        // Use an executor with `some.module` pre-registered as async-hinted
        // so the bridge's [A-D-008] guard (ASYNC_MODULE_NOT_ASYNC) does not
        // reject the submit. The bridge requires the target module to be
        // registered AND async-hinted to accept __apcore_task_submit.
        let bridge = Arc::new(AsyncTaskBridge::new(make_apcore_executor_with_async(
            "some.module",
        )));
        let router =
            ExecutionRouter::new(Box::new(MockExecutor), false, None).with_async_bridge(bridge);
        let (content, is_error, trace_id) = router
            .handle_call(
                "__apcore_task_submit",
                &json!({"module_id": "some.module", "arguments": {}}),
                None,
            )
            .await;
        assert!(!is_error);
        assert!(trace_id.is_some());
        // Content must be the spec envelope: {task_id, status: "pending"}.
        // [A-D-007] regression — pre-fix Rust serialised the full TaskInfo
        // which would have included module_id; the spec envelope omits it.
        let text = match &content[0].data {
            Value::String(s) => s.clone(),
            _ => panic!("expected string content"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert!(parsed.get("task_id").is_some(), "task_id required");
        assert_eq!(
            parsed.get("status").and_then(|v| v.as_str()),
            Some("pending"),
            "status must be 'pending' per spec; got: {parsed:?}"
        );
        assert!(
            parsed.get("module_id").is_none(),
            "envelope must NOT contain module_id (spec shape); got: {parsed:?}"
        );
    }

    #[tokio::test]
    async fn approval_check_meta_tool_routes_via_approval_bridge() {
        // [H-1] When an ApprovalBridge is installed, the router must
        // short-circuit `__apcore_approval_check` to the bridge instead of
        // dispatching to the executor.
        use crate::server::approval_bridge::ApprovalBridge;
        use apcore::approval::{ApprovalHandler, ApprovalRequest, ApprovalResult};
        use apcore::errors::ModuleError;

        #[derive(Debug)]
        struct StubHandler;
        #[async_trait]
        impl ApprovalHandler for StubHandler {
            async fn request_approval(
                &self,
                _request: &ApprovalRequest,
            ) -> Result<ApprovalResult, ModuleError> {
                let mut r = ApprovalResult::default();
                r.status = "approved".to_string();
                Ok(r)
            }
            async fn check_approval(
                &self,
                _approval_id: &str,
            ) -> Result<ApprovalResult, ModuleError> {
                let mut r = ApprovalResult::default();
                r.status = "approved".to_string();
                Ok(r)
            }
        }

        let bridge = Arc::new(ApprovalBridge::new(Arc::new(StubHandler)));
        let router =
            ExecutionRouter::new(Box::new(MockExecutor), false, None).with_approval_bridge(bridge);
        let (content, is_error, _trace_id) = router
            .handle_call(
                "__apcore_approval_check",
                &json!({"approval_id": "abc-123"}),
                None,
            )
            .await;
        assert!(!is_error);
        let text = match &content[0].data {
            Value::String(s) => s.clone(),
            _ => panic!("expected string content"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed.get("status").and_then(|v| v.as_str()),
            Some("approved")
        );
        assert_eq!(
            parsed.get("approval_id").and_then(|v| v.as_str()),
            Some("abc-123")
        );
    }

    #[tokio::test]
    async fn async_hinted_module_routes_via_bridge() {
        // [A-D-031] Static `async_module_ids` set has been dropped — the
        // bridge dispatch is now driven entirely by dynamic descriptor
        // lookup. Use an executor that has `heavy.module` registered as
        // async-hinted so the bridge classifies it correctly at call time.
        let bridge = Arc::new(AsyncTaskBridge::new(make_apcore_executor_with_async(
            "heavy.module",
        )));
        let router =
            ExecutionRouter::new(Box::new(MockExecutor), false, None).with_async_bridge(bridge);
        let (content, is_error, _) = router.handle_call("heavy.module", &json!({}), None).await;
        assert!(!is_error);
        let text = match &content[0].data {
            Value::String(s) => s.clone(),
            _ => panic!("expected string content"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert!(
            parsed.get("task_id").is_some(),
            "bridge submit must return task_id"
        );
    }

    #[tokio::test]
    async fn meta_tool_submit_reserved_id_is_error() {
        let bridge = Arc::new(AsyncTaskBridge::new(make_apcore_executor()));
        let router =
            ExecutionRouter::new(Box::new(MockExecutor), false, None).with_async_bridge(bridge);
        let (_content, is_error, _) = router
            .handle_call(
                "__apcore_task_submit",
                &json!({"module_id": "__apcore_something"}),
                None,
            )
            .await;
        assert!(is_error, "reserved module id must produce an error");
    }

    /// Regression test for [A-D-005].
    ///
    /// `validate_tool` must propagate `requires_approval` from the
    /// executor's preflight result. Pre-fix Rust hard-coded
    /// `requires_approval: false` in all branches; AI agents querying
    /// validate_tool to decide whether to surface the approval flow would
    /// always see `false` and skip the flow even for approval-gated
    /// modules.
    #[tokio::test]
    async fn validate_tool_propagates_requires_approval() {
        // Executor that reports requires_approval=true on preflight.
        struct ApprovalGatedExecutor;
        #[async_trait]
        impl Executor for ApprovalGatedExecutor {
            async fn call_async(
                &self,
                _module_id: &str,
                _inputs: &Value,
                _context: Option<&Value>,
                _version_hint: Option<&str>,
            ) -> Result<Value, ExecutorError> {
                Ok(json!({}))
            }
            fn validate(
                &self,
                _module_id: &str,
                _inputs: &Value,
                _context: Option<&Value>,
            ) -> Option<ValidationResult> {
                Some(ValidationResult {
                    valid: true,
                    errors: vec![],
                    requires_approval: true,
                })
            }
        }

        let router = ExecutionRouter::new(Box::new(ApprovalGatedExecutor), false, None);
        let result = router.validate_tool("gated.module", &json!({})).await;
        assert_eq!(
            result.get("requires_approval").and_then(|v| v.as_bool()),
            Some(true),
            "validate_tool must surface executor's requires_approval flag; got: {result:?}"
        );
    }

    /// [B-002] Regression: cancel_call sets a token, then a tombstone for
    /// unknown call_ids.
    #[test]
    fn cancel_call_returns_false_for_unknown_id_records_tombstone() {
        let router = ExecutionRouter::stub();
        let ok = router.cancel_call("call-unknown", Some("client abort"));
        assert!(!ok, "unknown call_id must return false");
        let guard = router.cancel_tokens.lock().unwrap();
        let tombstone = guard.get("call-unknown").expect("tombstone recorded");
        assert!(tombstone.is_cancelled(), "tombstone must be cancelled");
    }

    #[test]
    fn cancel_call_returns_true_for_active_token() {
        let router = ExecutionRouter::stub();
        let token = std::sync::Arc::new(CancelToken::new());
        router
            .cancel_tokens
            .lock()
            .unwrap()
            .insert("call-1".to_string(), std::sync::Arc::clone(&token));
        let ok = router.cancel_call("call-1", None);
        assert!(ok, "active token must return true");
        assert!(token.is_cancelled(), "token must be cancelled");
    }

    // ── panic safety on the typed-context dispatch ───────────────────────────

    /// An executor whose typed dispatch panics, standing in for a buggy
    /// third-party implementation.
    struct PanickingTypedExecutor;

    #[async_trait]
    impl Executor for PanickingTypedExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: Option<&Value>,
            _version_hint: Option<&str>,
        ) -> Result<Value, ExecutorError> {
            Ok(json!({"untyped": true}))
        }

        async fn call_typed(
            &self,
            _module_id: &str,
            _inputs: &Value,
            _context: &apcore::Context<Value>,
            _version_hint: Option<&str>,
            _want_trace: bool,
        ) -> Option<Result<(Value, Option<Value>), ExecutorError>> {
            panic!("buggy executor");
        }
    }

    #[tokio::test]
    async fn test_typed_dispatch_panic_becomes_an_error_result() {
        // `call_typed` is the primary production dispatch, so it carries the
        // same [A-D-029] guard as `call_async`: a panicking executor must come
        // back as an `is_error` result, not take the connection down. Without
        // the guard this test unwinds instead of asserting.
        let router = ExecutionRouter::new(Box::new(PanickingTypedExecutor), false, None);
        let ctx = apcore::Context::<Value>::anonymous();
        let (content, is_error, _) = router
            .handle_call_async_with_hint("mod", &json!({}), None, None, Some(&ctx))
            .await;

        assert!(is_error, "a panicking executor must yield an error result");
        let text = content[0].data.as_str().unwrap_or_default();
        assert!(
            text.contains("executor panicked"),
            "the caller must be told the executor panicked: {text}"
        );
    }

    #[tokio::test]
    async fn test_typed_dispatch_declining_falls_back_to_call_async() {
        // The default `call_typed` returns `None`, meaning "this executor
        // cannot take a typed context". That must fall through to the untyped
        // path rather than failing the call — it is what keeps every existing
        // Executor implementation working.
        struct DecliningExecutor;

        #[async_trait]
        impl Executor for DecliningExecutor {
            async fn call_async(
                &self,
                _module_id: &str,
                _inputs: &Value,
                _context: Option<&Value>,
                _version_hint: Option<&str>,
            ) -> Result<Value, ExecutorError> {
                Ok(json!({"untyped": true}))
            }
        }

        let router = ExecutionRouter::new(Box::new(DecliningExecutor), false, None);
        let ctx = apcore::Context::<Value>::anonymous();
        let (content, is_error, _) = router
            .handle_call_async_with_hint("mod", &json!({}), None, None, Some(&ctx))
            .await;

        assert!(
            !is_error,
            "declining a typed context must not fail the call"
        );
        let text = content[0].data.as_str().unwrap_or_default();
        assert!(
            text.contains("untyped"),
            "the untyped path must have produced the result: {text}"
        );
    }

    // -----------------------------------------------------------------------
    // Cross-language conformance: F-038 output redaction
    // -----------------------------------------------------------------------
    //
    // Lives here rather than under `tests/` because `redact_output` is
    // private, and widening it to `pub(crate)` purely so an integration test
    // could reach it would relax the production surface for a test's
    // convenience. The cost is that `tests/common`'s fixture locator cannot be
    // reused — a `#[cfg(test)]` item is not part of the compiled crate an
    // integration test links against — so the lookup is repeated in miniature
    // below.

    /// Locate the shared conformance fixtures, mirroring `tests/common`.
    ///
    /// Honours `APCORE_CONFORMANCE_FIXTURES`, otherwise walks up looking for
    /// `apcore-mcp/conformance/fixtures`, which covers both the sibling
    /// checkout developers use and the CI layout where the spec repo is
    /// checked out inside the workspace.
    fn conformance_fixture(name: &str) -> Option<Value> {
        let dir = match std::env::var("APCORE_CONFORMANCE_FIXTURES") {
            Ok(explicit) => {
                let candidate = std::path::PathBuf::from(explicit);
                candidate.is_dir().then_some(candidate)
            }
            Err(_) => {
                let mut dir: std::path::PathBuf = env!("CARGO_MANIFEST_DIR").into();
                let mut found = None;
                for _ in 0..=4 {
                    let candidate = dir.join("apcore-mcp").join("conformance").join("fixtures");
                    if candidate.is_dir() {
                        found = Some(candidate);
                        break;
                    }
                    if !dir.pop() {
                        break;
                    }
                }
                found
            }
        };

        if let Some(dir) = dir {
            let path = dir.join(name);
            if path.is_file() {
                let raw = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", path.display()));
                return Some(
                    serde_json::from_str(&raw)
                        .unwrap_or_else(|e| panic!("fixture {} is malformed: {e}", path.display())),
                );
            }
        }

        assert!(
            std::env::var_os("CI").is_none(),
            "conformance fixture {name:?} not found. In CI this is a failure \
             rather than a skip: the cross-language conformance suite exists to \
             catch divergence between the three bridges, and skipping it \
             silently reports success while proving nothing."
        );
        eprintln!("skipping: conformance fixture {name:?} not found");
        None
    }

    #[test]
    fn conformance_output_redaction() {
        // Pins the Rust half of a divergence the fixture records under
        // `known_gaps#upstream_key_name_heuristic`: the three upstream apcore
        // libraries disagree about masking a secret-sounding key the schema
        // never marked. Python and TypeScript were measured when that gap was
        // written; Rust's behaviour was only read off `redact_secret_prefix`.
        // This measures it, so the claim has a test behind it.
        let Some(fixture) = conformance_fixture("output_redaction.json") else {
            return;
        };
        let tool = "conformance.subject";

        for case in fixture["test_cases"].as_array().expect("test_cases") {
            let id = case["id"].as_str().expect("case id");
            let mut schemas = HashMap::new();
            schemas.insert(tool.to_string(), case["output_schema"].clone());

            let router = ExecutionRouter::stub().with_output_schemas(schemas);
            let redacted = router.redact_output(tool, &case["output"]);

            assert_eq!(
                redacted, case["expected_redacted_output"],
                "case {id}: redaction mismatch"
            );
        }
    }

    #[test]
    fn upstream_redaction_key_name_behaviour_is_measured_not_assumed() {
        // The fixture's `known_gaps#upstream_key_name_heuristic` claims the
        // three upstream apcore libraries disagree about masking a
        // secret-sounding key that the schema never marked. Python and
        // TypeScript were measured directly. This measures Rust, so no leg of
        // that claim rests on reading the upstream source.
        //
        // Deliberately not a shared-fixture case: pinning it there would
        // declare one language's behaviour to be the correct one, and the
        // divergence has to be settled in apcore before that can be said.
        let empty = serde_json::json!({});

        let by_name = apcore::redact_sensitive(&serde_json::json!({"token": "v"}), &empty);
        let by_prefix = apcore::redact_sensitive(&serde_json::json!({"_secret_x": "v"}), &empty);
        let untouched = apcore::redact_sensitive(&serde_json::json!({"harmless": "v"}), &empty);

        assert_eq!(
            by_prefix["_secret_x"],
            apcore::REDACTED_VALUE,
            "apcore-rust masks the _secret_ prefix with no schema"
        );
        assert_eq!(
            untouched["harmless"], "v",
            "an unmarked, unsuggestive key is never masked"
        );
        assert_eq!(
            by_name["token"], "v",
            "apcore-rust does NOT mask a secret-sounding key name without a \
             schema, where apcore (Python) does. If this assertion starts \
             failing, upstream has converged and \
             output_redaction.json#known_gaps#upstream_key_name_heuristic \
             should be revisited — the fixture case narrowed for this reason \
             can then widen again."
        );
    }
}
