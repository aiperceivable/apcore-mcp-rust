//! AsyncTaskBridge — routes async-hinted modules through apcore's
//! [`AsyncTaskManager`] and exposes the `__apcore_task_*` meta-tools.
//!
//! See `docs/features/async-task-bridge.md` for the full feature spec.
//!
//! This bridge is a thin routing layer:
//! * Modules whose descriptor carries `metadata.async == true` OR
//!   `annotations.extra["mcp_async"] == "true"` are routed through
//!   [`AsyncTaskManager::submit`].
//! * Four reserved MCP tool names are registered:
//!   - `__apcore_task_submit`
//!   - `__apcore_task_status`
//!   - `__apcore_task_cancel`
//!   - `__apcore_task_list`
//! * Module ids starting with `__apcore_` are rejected by the bridge to
//!   keep the meta-tool namespace reserved.
//! * When the caller supplies `_meta.progressToken`, the progress token is
//!   recorded and (future) progress events are fanned out via
//!   `notifications/progress`.
//! * On `__apcore_task_status`, completed results are redacted via
//!   `apcore::redact_sensitive` using the module's output schema.

// apcore::ModuleError is a large enum; the bridge returns it through the
// normal apcore error mapper rather than boxing, matching the convention
// used elsewhere in this crate (see src/server/router.rs).
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use apcore::async_task::{AsyncTaskManager, TaskInfo, TaskStatus};
use apcore::executor::Executor as ApcoreExecutor;
use apcore::registry::registry::Registry;
use apcore::registry::ModuleDescriptor;
use apcore::{Context, Identity};
use serde_json::{json, Value};

use crate::server::router::SendNotificationFn;
use crate::server::types::Tool;

/// Minimal submit result envelope returned by [`AsyncTaskBridge::submit`].
///
/// Matches the Python+TS protocol shape `{task_id, status: "pending"}`.
/// [D10-003]
#[derive(Debug, Clone)]
pub struct SubmitResult {
    /// The allocated task ID.
    pub task_id: String,
    /// Always `"pending"` at submit time.
    pub status: String,
}

/// Reserved prefix for MCP meta-tools.
pub const META_TOOL_PREFIX: &str = "__apcore_";

/// Meta-tool names.
pub const META_TOOL_SUBMIT: &str = "__apcore_task_submit";
pub const META_TOOL_STATUS: &str = "__apcore_task_status";
pub const META_TOOL_CANCEL: &str = "__apcore_task_cancel";
pub const META_TOOL_LIST: &str = "__apcore_task_list";
/// `__apcore_module_preview` — apcore PROTOCOL_SPEC §5.6 / §12.8.
/// Surfaces `Module.preview()` and the rest of the dry-run preflight
/// checks (input validation, ACL, approval requirement) to MCP clients
/// without side effects, so AI orchestrators can ask "what would change
/// in the world if I called this module?" before executing.
pub const META_TOOL_PREVIEW: &str = "__apcore_module_preview";

/// Default `AsyncTaskManager` configuration.
pub const DEFAULT_MAX_CONCURRENT: usize = 10;
pub const DEFAULT_MAX_TASKS: usize = 1000;

/// Bridge for routing async-hinted module calls through
/// [`AsyncTaskManager`] and exposing the `__apcore_task_*` meta-tools.
pub struct AsyncTaskBridge {
    /// Shared apcore async task manager.
    manager: Arc<AsyncTaskManager>,
    /// Reference to the executor, retained so that `handle_submit` can
    /// look up the module descriptor via `executor.registry()` to enforce
    /// the spec's `ASYNC_MODULE_NOT_ASYNC` rejection rule. [A-D-008]
    executor: Arc<ApcoreExecutor>,
    /// Cached output schemas (per module id) used for result redaction.
    output_schemas: HashMap<String, Value>,
    /// Map of task_id → progressToken recorded at submit time for
    /// progress fan-out on terminal transitions.
    progress_tokens: Arc<Mutex<HashMap<String, Value>>>,
    /// Map of task_id → MCP notification sender, recorded at submit time
    /// when the caller supplied both a progress token and a notification
    /// channel. [`AsyncTaskBridge::emit_progress`] fans out
    /// `notifications/progress` messages to the recorded sender. [A-D-220]
    ///
    /// **Per-language idiom note.** Python and TypeScript install a
    /// `ProgressCallback` directly under `context.data[MCP_PROGRESS_KEY]`
    /// so module-side `report_progress(context)` looks the callback up.
    /// Rust's `report_progress(ctx, callback, ...)` takes the callback
    /// explicitly because `apcore::Context::data` holds JSON `Value`s
    /// (cannot store closures). The bridge therefore owns the sender map
    /// itself and exposes [`AsyncTaskBridge::emit_progress`] as the
    /// fan-out point — module-side ergonomics differ from Py/TS by design.
    progress_senders: Arc<Mutex<HashMap<String, SendNotificationFn>>>,
    /// Map of session/connection key → set of task ids launched from that
    /// session, so [`AsyncTaskBridge::cancel_session_tasks`] can cancel
    /// them cooperatively when the transport detects disconnect.
    session_tasks: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl AsyncTaskBridge {
    /// Create a new bridge backed by the given apcore executor.
    pub fn new(executor: Arc<ApcoreExecutor>) -> Self {
        Self::with_limits(executor, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_TASKS)
    }

    /// Create a new bridge with explicit concurrency/task limits.
    pub fn with_limits(
        executor: Arc<ApcoreExecutor>,
        max_concurrent: usize,
        max_tasks: usize,
    ) -> Self {
        Self {
            manager: Arc::new(AsyncTaskManager::new(
                Arc::clone(&executor),
                max_concurrent,
                max_tasks,
            )),
            executor,
            output_schemas: HashMap::new(),
            progress_tokens: Arc::new(Mutex::new(HashMap::new())),
            progress_senders: Arc::new(Mutex::new(HashMap::new())),
            session_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set the output schemas used for result redaction on
    /// `__apcore_task_status` responses.
    pub fn with_output_schemas(mut self, schemas: HashMap<String, Value>) -> Self {
        self.output_schemas = schemas;
        self
    }

    /// Direct access to the underlying manager (used for shutdown).
    pub fn manager(&self) -> &Arc<AsyncTaskManager> {
        &self.manager
    }

    /// Check whether a module descriptor is async-hinted.
    ///
    /// Primary public API matching Python `is_async_module(descriptor)` and
    /// TS `isAsyncModule(descriptor)` — takes the full descriptor. [D10-005]
    pub fn is_async_module_descriptor(descriptor: &ModuleDescriptor) -> bool {
        let extra = descriptor.annotations.as_ref().map(|a| &a.extra);
        Self::is_async_module(&descriptor.metadata, extra)
    }

    /// Field-level async detection (internal). Prefer [`Self::is_async_module_descriptor`].
    ///
    /// Async modules are those with:
    /// - `metadata.async == true`, or
    /// - `annotations.extra["mcp_async"] == "true"` (string or bool).
    pub fn is_async_module(
        metadata: &HashMap<String, Value>,
        annotations_extra: Option<&HashMap<String, Value>>,
    ) -> bool {
        // Match Python's case-insensitive comparison so YAML-round-tripped
        // hint values ("True", "TRUE", "TrUe") dispatch identically across
        // SDKs. [A-003 cross-lang fix]
        let is_truthy_async_string =
            |v: &Value| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true"));
        if let Some(v) = metadata.get("async") {
            if v.as_bool() == Some(true) || is_truthy_async_string(v) {
                return true;
            }
        }
        if let Some(extra) = annotations_extra {
            if let Some(v) = extra.get("mcp_async") {
                if v.as_bool() == Some(true) || is_truthy_async_string(v) {
                    return true;
                }
            }
        }
        false
    }

    /// Look up `module_id` against this bridge's executor's registry and
    /// return whether the descriptor is async-hinted.
    ///
    /// Dynamic counterpart to the static `async_module_ids` set the
    /// router previously used: callers that mutate the registry at
    /// runtime (dynamic-tool-registration via `RegistryListener`) will
    /// have their newly-registered async modules routed through the
    /// bridge without the router needing a refresh. [A-D-031]
    pub fn is_async_module_registered_self(&self, module_id: &str) -> bool {
        Self::is_async_registered(self.executor.registry(), module_id)
    }

    /// Check whether an async hint applies to a module registered in the
    /// given apcore registry.
    pub fn is_async_registered(registry: &Registry, module_id: &str) -> bool {
        let Ok(Some(desc)) = registry.get_definition(module_id) else {
            return false;
        };
        let extra = desc.annotations.as_ref().map(|a| &a.extra);
        Self::is_async_module(&desc.metadata, extra)
    }

    /// Return `true` if a module id is reserved by the bridge.
    pub fn is_reserved_id(module_id: &str) -> bool {
        module_id.starts_with(META_TOOL_PREFIX)
    }

    /// Submit a module for asynchronous execution.
    ///
    /// Returns a `SubmitResult { task_id, status: "pending" }` envelope on
    /// success, matching Python+TS's `{task_id, status: "pending"}` shape.
    /// [D10-003]
    ///
    /// The `__apcore_*` reserved-id guard is NOT applied here; it belongs
    /// exclusively in the `__apcore_task_submit` meta-tool handler so that
    /// callers can call `submit()` directly without being blocked by the
    /// meta-tool namespace enforcement. [D10-004]
    ///
    /// When both `progress_token` AND `send_notification` are provided,
    /// the sender is stored on the bridge keyed by task_id so subsequent
    /// calls to [`AsyncTaskBridge::emit_progress`] fan out
    /// `notifications/progress` messages to the original MCP client.
    /// Closes cross-language progress-emission gap. [A-D-220]
    pub async fn submit(
        &self,
        module_id: &str,
        inputs: Value,
        identity: Option<Identity>,
        progress_token: Option<Value>,
        send_notification: Option<SendNotificationFn>,
        session_key: Option<&str>,
    ) -> Result<SubmitResult, apcore::errors::ModuleError> {
        let ctx: Option<Context<Value>> = identity.map(Context::new);
        let task_id = self.manager.submit(module_id, inputs, ctx).await?;

        // Store progress sink: token + sender pair, mirroring the Python
        // `_install_progress_sink` and TS inline-closure patterns. [A-D-220]
        // We require BOTH token and sender to install — a token without a
        // sender has nowhere to fan out to, and a sender without a token
        // has no way to identify the request to the MCP client.
        if let (Some(token), Some(sender)) = (progress_token, send_notification) {
            {
                let mut guard = self
                    .progress_tokens
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                guard.insert(task_id.clone(), token);
            }
            {
                let mut guard = self
                    .progress_senders
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                guard.insert(task_id.clone(), sender);
            }
        }

        if let Some(session) = session_key {
            let mut guard = self.session_tasks.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .entry(session.to_string())
                .or_default()
                .push(task_id.clone());
        }

        Ok(SubmitResult {
            task_id,
            status: "pending".to_string(),
        })
    }

    /// Emit a `notifications/progress` message for the given task.
    ///
    /// Looks up the task's stored progress token and notification sender
    /// (recorded by [`AsyncTaskBridge::submit`] when both were supplied)
    /// and fans out a notification to the MCP client. No-op if the task
    /// has no recorded sink. [A-D-220]
    ///
    /// The notification shape matches Python+TS:
    /// ```json
    /// {
    ///   "method": "notifications/progress",
    ///   "params": {
    ///     "progressToken": <token>,
    ///     "progress": <f64>,
    ///     "total": <f64 | null>,
    ///     "message": <string?>  // omitted when None
    ///   }
    /// }
    /// ```
    pub async fn emit_progress(
        &self,
        task_id: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
    ) {
        let (token, sender) = {
            let token_guard = self
                .progress_tokens
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let sender_guard = self
                .progress_senders
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let token = match token_guard.get(task_id) {
                Some(t) => t.clone(),
                None => return,
            };
            let sender = match sender_guard.get(task_id) {
                Some(s) => Arc::clone(s),
                None => return,
            };
            (token, sender)
        };

        let mut params = serde_json::Map::new();
        params.insert("progressToken".to_string(), token);
        params.insert("progress".to_string(), json!(progress));
        params.insert(
            "total".to_string(),
            total.map(|t| json!(t)).unwrap_or(Value::Null),
        );
        if let Some(msg) = message {
            params.insert("message".to_string(), Value::String(msg.to_string()));
        }
        let notification = json!({
            "method": "notifications/progress",
            "params": Value::Object(params),
        });
        if let Err(e) = sender(notification).await {
            tracing::debug!(task_id = %task_id, "failed to send progress notification: {e}");
        }
    }

    /// Retrieve the current `TaskInfo` for a task id.
    ///
    /// When the task is in `Completed` state, the embedded `result` is
    /// redacted via the module's registered output schema (if any). If the
    /// redactor panics, the unredacted result is returned and the panic is
    /// logged at debug level — matches apcore-mcp-python and
    /// apcore-mcp-typescript try/except behaviour.
    pub fn get_status(&self, task_id: &str) -> Option<TaskInfo> {
        let mut info = self.manager.get_status(task_id)?;
        if info.status == TaskStatus::Completed {
            if let Some(result) = &info.result {
                if let Some(schema) = self.output_schemas.get(&info.module_id) {
                    // The apcore redactor is best-effort; if it panics we degrade
                    // to the unredacted result rather than poisoning the response.
                    let result_for_redact = result.clone();
                    let schema_for_redact = schema.clone();
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                        apcore::redact_sensitive(&result_for_redact, &schema_for_redact)
                    })) {
                        Ok(redacted) => {
                            info.result = Some(redacted);
                        }
                        Err(_) => {
                            tracing::debug!(
                                task_id = %task_id,
                                module_id = %info.module_id,
                                "task-result redactor panicked; falling back to unredacted result"
                            );
                        }
                    }
                }
            }
        }
        Some(info)
    }

    /// Cancel a running or pending task.
    ///
    /// [D11-103] Order matters here: `manager.cancel` is awaited FIRST so
    /// any progress notification emitted during the cancellation handshake
    /// can still observe the sender/token in our maps. Only after the
    /// manager has finalised the task do we drop the progress bindings.
    /// Matches Python (async_task_bridge.py:458-460) and TypeScript
    /// (async_task_bridge.ts:590-596).
    pub async fn cancel(&self, task_id: &str) -> bool {
        let cancelled = self.manager.cancel(task_id).await;
        {
            let mut guard = self
                .progress_tokens
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            guard.remove(task_id);
        }
        {
            let mut guard = self
                .progress_senders
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            guard.remove(task_id);
        }
        cancelled
    }

    /// Cancel every task recorded under the given session key. Used by the
    /// transport layer when a client disconnects or cancels a request.
    ///
    /// Returns the number of tasks cancelled.
    pub async fn cancel_session_tasks(&self, session_key: &str) -> usize {
        let ids: Vec<String> = {
            let mut map = self.session_tasks.lock().unwrap_or_else(|p| p.into_inner());
            map.remove(session_key).unwrap_or_default()
        };
        let mut n = 0;
        for id in &ids {
            if self.cancel(id).await {
                n += 1;
            }
        }
        n
    }

    /// List all tracked tasks, optionally filtered by status.
    pub fn list_tasks(&self, status: Option<TaskStatus>) -> Vec<TaskInfo> {
        self.manager.list_tasks(status)
    }

    /// Cancel all pending/running tasks at server shutdown.
    pub async fn shutdown(&self) {
        self.manager.shutdown().await;
    }

    /// Build the four reserved meta-tool definitions.
    pub fn build_meta_tools() -> Vec<Tool> {
        vec![
            Tool {
                name: META_TOOL_SUBMIT.to_string(),
                description:
                    "Submit a module for asynchronous execution via apcore's AsyncTaskManager."
                        .to_string(),
                // Schema parity with Python+TS: additionalProperties:false
                // closes the meta-tool input shape against unknown keys.
                // [A-D-023]
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "module_id": { "type": "string" },
                        "arguments": { "type": "object" },
                        "version_hint": { "type": "string" }
                    },
                    "required": ["module_id"],
                    "additionalProperties": false
                }),
                annotations: None,
                meta: Some(json!({ "reserved": true })),
            },
            Tool {
                name: META_TOOL_STATUS.to_string(),
                description:
                    "Query the TaskInfo projection for a previously submitted task."
                        .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }),
                annotations: None,
                meta: Some(json!({ "reserved": true })),
            },
            Tool {
                name: META_TOOL_CANCEL.to_string(),
                description: "Cancel a pending or running async task.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }),
                annotations: None,
                meta: Some(json!({ "reserved": true })),
            },
            Tool {
                name: META_TOOL_LIST.to_string(),
                description:
                    "List all tracked tasks, optionally filtered by status (pending/running/completed/failed/cancelled)."
                        .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["pending", "running", "completed", "failed", "cancelled"]
                        }
                    },
                    "additionalProperties": false
                }),
                annotations: None,
                meta: Some(json!({ "reserved": true })),
            },
            Tool {
                name: META_TOOL_PREVIEW.to_string(),
                description:
                    "Preview a module call: predict state changes, validate inputs, and \
                     check approval requirements WITHOUT executing the module. Returns \
                     {valid, requires_approval, predicted_changes, checks}. Use this \
                     before invoking destructive or stateful modules to let the AI \
                     orchestrator answer 'what would change in the world if I called \
                     this?' (apcore PROTOCOL_SPEC §5.6)."
                        .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "module_id": { "type": "string" },
                        "arguments": { "type": "object" }
                    },
                    "required": ["module_id"],
                    "additionalProperties": false
                }),
                annotations: None,
                meta: Some(json!({ "reserved": true })),
            },
        ]
    }

    /// Handle a meta-tool invocation.
    ///
    /// Returns `Some(result_json)` when the call matched a meta-tool name,
    /// or `None` when the caller should fall through to the normal
    /// execution path.
    pub async fn handle_meta_tool(
        &self,
        tool_name: &str,
        arguments: &Value,
        identity: Option<Identity>,
        progress_token: Option<Value>,
        send_notification: Option<SendNotificationFn>,
        session_key: Option<&str>,
    ) -> Option<Result<Value, apcore::errors::ModuleError>> {
        match tool_name {
            META_TOOL_SUBMIT => Some(
                self.handle_submit(
                    arguments,
                    identity,
                    progress_token,
                    send_notification,
                    session_key,
                )
                .await,
            ),
            META_TOOL_STATUS => Some(self.handle_status(arguments)),
            META_TOOL_CANCEL => Some(self.handle_cancel(arguments).await),
            META_TOOL_LIST => Some(self.handle_list(arguments)),
            META_TOOL_PREVIEW => Some(self.handle_preview(arguments, identity).await),
            _ => None,
        }
    }

    async fn handle_submit(
        &self,
        arguments: &Value,
        identity: Option<Identity>,
        progress_token: Option<Value>,
        send_notification: Option<SendNotificationFn>,
        session_key: Option<&str>,
    ) -> Result<Value, apcore::errors::ModuleError> {
        // [A-D-AT-3] `as_str()` yields Some("") for an empty string, so the
        // emptiness check has to be explicit. Python
        // (`not isinstance(x, str) or not x`) and TypeScript
        // (`typeof x !== "string" || x.length === 0`) both reject, and the
        // spec Contracts pin `validates[non-empty string]`.
        let module_id = arguments
            .get("module_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_task_submit requires a non-empty 'module_id'",
                )
            })?;

        // [D10-004] Reserved-id guard belongs here in the meta-tool handler,
        // not in submit(). Python+TS only enforce this in the meta-tool handler
        // so that submit() can be called directly without restriction.
        if Self::is_reserved_id(module_id) {
            return Err(apcore::errors::ModuleError::new(
                apcore::errors::ErrorCode::GeneralInvalidInput,
                format!(
                    "module id '{module_id}' is reserved by the async task bridge (__apcore_ prefix)"
                ),
            ));
        }

        // Spec rule: __apcore_task_submit against a non-async module returns
        // ASYNC_MODULE_NOT_ASYNC. Python enforces this; TS+Rust were
        // previously skipping the check and silently wrapping sync-only
        // modules as async tasks. [A-D-008]
        let registry = self.executor.registry();
        if !Self::is_async_registered(registry, module_id) {
            return Err(apcore::errors::ModuleError::new(
                apcore::errors::ErrorCode::GeneralInvalidInput,
                format!(
                    "ASYNC_MODULE_NOT_ASYNC: module '{module_id}' is not async-hinted; \
                     use regular tools/call instead of __apcore_task_submit"
                ),
            ));
        }
        // [D11-101] Validate that `arguments`, when present and non-null,
        // is a JSON object. Mirrors the shape-check pattern already used in
        // `handle_preview` (above) so a stray array/scalar never silently
        // submits with empty inputs.
        let inputs = match arguments.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(Value::Object(_)) => arguments.get("arguments").cloned().unwrap_or(json!({})),
            Some(_) => {
                return Err(apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_task_submit requires `arguments` to be a JSON object",
                ));
            }
        };
        // submit() now returns SubmitResult{task_id, status} — the exact
        // envelope shape Python+TS return. [D10-003]
        let result = self
            .submit(
                module_id,
                inputs,
                identity,
                progress_token,
                send_notification,
                session_key,
            )
            .await?;
        Ok(json!({
            "task_id": result.task_id,
            "status": result.status,
        }))
    }

    fn handle_status(&self, arguments: &Value) -> Result<Value, apcore::errors::ModuleError> {
        // [A-D-AT-3] Reject an empty task_id up front; without this it
        // reached the registry and came back as ASYNC_TASK_NOT_FOUND instead of
        // an invalid-argument error, unlike Python and TypeScript.
        let task_id = arguments
            .get("task_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_task_status requires a non-empty 'task_id'",
                )
            })?;
        match self.get_status(task_id) {
            Some(info) => Ok(serde_json::to_value(info).unwrap_or(Value::Null)),
            // [D11-014] Align with Python's `_text_response({"error":
            // "ASYNC_TASK_NOT_FOUND", "task_id": ...}, is_error=True)` — return
            // Ok(json_envelope) with an error payload rather than Err(...).
            // This keeps the MCP response layer consistent: the router receives
            // Ok and can set `is_error: true` on the content item.
            None => Ok(json!({
                "error": "ASYNC_TASK_NOT_FOUND",
                "task_id": task_id,
                "is_error": true,
            })),
        }
    }

    async fn handle_cancel(&self, arguments: &Value) -> Result<Value, apcore::errors::ModuleError> {
        // [A-D-AT-3] Same non-empty requirement as handle_status.
        let task_id = arguments
            .get("task_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_task_cancel requires a non-empty 'task_id'",
                )
            })?;
        // [D10-004] Pre-check task existence (matching Python `_handle_cancel_tool`).
        // Without this, unknown task_ids silently return `{cancelled: false}` —
        // callers can't distinguish "task existed and is uncancellable" from
        // "task never existed". Mirrors handle_status's existence-check path.
        if self.get_status(task_id).is_none() {
            return Ok(json!({
                "error": "ASYNC_TASK_NOT_FOUND",
                "task_id": task_id,
                "is_error": true,
            }));
        }
        let cancelled = self.cancel(task_id).await;
        Ok(json!({ "task_id": task_id, "cancelled": cancelled }))
    }

    fn handle_list(&self, arguments: &Value) -> Result<Value, apcore::errors::ModuleError> {
        // [D11-102] Inspect the raw `status` value so non-string variants
        // (numbers, bools, arrays, objects) are rejected instead of silently
        // ignored by `as_str() → None`.
        let status_filter = match arguments.get("status") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(match s.as_str() {
                "pending" => TaskStatus::Pending,
                "running" => TaskStatus::Running,
                "completed" => TaskStatus::Completed,
                "failed" => TaskStatus::Failed,
                "cancelled" => TaskStatus::Cancelled,
                other => {
                    return Err(apcore::errors::ModuleError::new(
                        apcore::errors::ErrorCode::GeneralInvalidInput,
                        format!("unknown task status filter: {other}"),
                    ));
                }
            }),
            Some(_) => {
                return Err(apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_task_list `status` filter must be a string",
                ));
            }
        };
        let tasks = self.list_tasks(status_filter);
        let tasks_json: Vec<Value> = tasks
            .into_iter()
            .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
            .collect();
        Ok(json!({ "tasks": tasks_json }))
    }

    /// Run `executor.validate(module_id, inputs, context)` and return a
    /// JSON envelope with `valid`, `requires_approval`, `predicted_changes`,
    /// and `checks`. apcore PROTOCOL_SPEC §5.6 / §12.8.
    async fn handle_preview(
        &self,
        arguments: &Value,
        identity: Option<Identity>,
    ) -> Result<Value, apcore::errors::ModuleError> {
        // [A-D-AT-3] An empty module_id used to reach
        // `executor.validate("", ..)` — an actual executor call the peers never
        // make.
        let module_id = arguments
            .get("module_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_module_preview requires a non-empty 'module_id'",
                )
            })?;
        // Reject reserved-prefix module ids the same way the submit branch
        // does — the preview meta-tool must not introspect the bridge's own
        // meta-tools (apcore PROTOCOL_SPEC §5.6 contract). Cross-SDK parity:
        // Python and TypeScript apply the same guard. [A-D-001]
        if Self::is_reserved_id(module_id) {
            return Err(apcore::errors::ModuleError::new(
                apcore::errors::ErrorCode::GeneralInvalidInput,
                format!(
                    "Reserved module id: '{}'. Names starting with '{}' \
                     are reserved for apcore-mcp meta-tools and cannot be \
                     previewed via this handler.",
                    module_id, META_TOOL_PREFIX,
                ),
            ));
        }
        // Preserve `arguments: null` verbatim — let the calling
        // business decide whether null is a valid input. Missing
        // ``arguments`` collapses to JSON null too (no caller intent
        // to pass inputs); structurally-wrong shapes (arrays, scalars)
        // are rejected because they can never represent a JSON object.
        let inputs = match arguments.get("arguments") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::Object(_)) => arguments.get("arguments").cloned().unwrap_or(Value::Null),
            Some(_) => {
                return Err(apcore::errors::ModuleError::new(
                    apcore::errors::ErrorCode::GeneralInvalidInput,
                    "__apcore_module_preview requires 'arguments' to be a JSON object or null",
                ))
            }
        };
        let ctx_owned: Option<Context<Value>> = identity.map(Context::new);
        let ctx_ref = ctx_owned.as_ref();
        // executor.validate is non-throwing on input-shape errors; it
        // returns a structured PreflightResult with valid=false instead.
        let preflight = self.executor.validate(module_id, &inputs, ctx_ref).await?;

        // PROTOCOL_SPEC §12.8.5.1, implemented upstream in apcore 0.27:
        // `Executor::validate` MUST NOT invoke the module preflight/preview
        // hooks when the `acl` check failed, MUST NOT emit their checks, and
        // MUST leave `predicted_changes` empty. Before 0.27 it ran both hooks
        // on the strength of module lookup alone, so this bridge filtered the
        // two introspection checks out of the envelope by string to keep a
        // denied caller from reading the resolved binary and argv back out of
        // a preview.
        //
        // That filter is deleted rather than kept as a second line of defence.
        // With the gate upstream it can never be observed to act: no mutation
        // of it fails a test, which by this branch's own standard makes it
        // indistinguishable from dead code, and two guards where nobody can
        // tell which is load-bearing is worse than one that is pinned. What it
        // protected is asserted directly instead —
        // `upstream_withholds_module_introspection_from_a_denied_caller` pins
        // §12.8.5.1 against a real `Executor` (with
        // `upstream_still_introspects_for_an_allowed_caller` as its control, so
        // an apcore that stopped introspecting entirely cannot satisfy it for
        // the wrong reason), and
        // `preview_meta_tool_withholds_predicted_changes_when_acl_denies` pins
        // the same property end to end on this envelope.
        if preflight
            .checks
            .iter()
            .any(|c| c.check == apcore::module::ACL_CHECK_NAME && !c.passed)
        {
            // Operator signal, not a guard: apcore has already emptied the
            // envelope by this point. Bound to apcore's own constant so a
            // rename upstream is a compile error rather than a log line that
            // quietly stops firing.
            tracing::warn!(
                module_id = %module_id,
                "module preview denied by ACL; apcore withheld module introspection"
            );
        }

        let checks: Vec<Value> = preflight
            .checks
            .iter()
            .map(preflight_check_to_json)
            .collect();
        let predicted: Vec<Value> = preflight
            .predicted_changes
            .iter()
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            .collect();
        Ok(json!({
            "valid": preflight.valid,
            // Reported verbatim, as apcore-python's and apcore-typescript's
            // bridges both do.
            //
            // Since apcore 0.28.0 (spec §6.9 row 3) this bit is no longer a
            // property of the module's annotations alone: it is the *union* of
            // the annotation, any `ExecutionPolicy` override, and the ACL's
            // verdict for this caller with these arguments (§6.1.6). What makes
            // returning it verbatim safe on a denial is narrower than the
            // earlier "it is unrelated to authorization" reasoning, which no
            // longer holds: the ACL component is constant `false` on the deny
            // path. `deny` + `approval: required` is rejected at load
            // (§6.1.6 rule 2) and a denial clears any pending requirement
            // (§6.1.4 rule 5), so `AccessDecision::approval_required` is always
            // `false` when `access == "deny"`. A denied caller therefore reads
            // only the policy/annotation half and learns nothing about the ACL
            // policy. Masking it anyway would make this the one SDK of three
            // answering `false` where the others answer `true`.
            "requires_approval": preflight.requires_approval,
            "predicted_changes": predicted,
            "checks": checks,
        }))
    }
}

/// Render a single [`apcore::module::PreflightCheckResult`] as the wire
/// object shared by the Python and TypeScript bridges.
fn preflight_check_to_json(check: &apcore::module::PreflightCheckResult) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("check".to_string(), Value::String(check.check.clone()));
    obj.insert("passed".to_string(), Value::Bool(check.passed));
    if let Some(err) = &check.error {
        obj.insert(
            "error".to_string(),
            serde_json::to_value(err).unwrap_or(Value::Null),
        );
    }
    if !check.warnings.is_empty() {
        obj.insert(
            "warnings".to_string(),
            Value::Array(check.warnings.iter().cloned().map(Value::String).collect()),
        );
    }
    Value::Object(obj)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::executor::Executor;
    use apcore::module::ModuleAnnotations;
    use apcore::registry::registry::Registry;
    use apcore::registry::ModuleDescriptor;
    use std::sync::Arc;

    fn make_executor() -> Arc<Executor> {
        let registry = Arc::new(Registry::default());
        let config = Arc::new(apcore::config::Config::default());
        Arc::new(Executor::new(registry, config))
    }

    /// Build an executor with an async-hinted module pre-registered under
    /// the given module_id. Used by tests that exercise the spec-compliant
    /// `handle_submit` path post-A-D-008 (which now rejects non-async
    /// module ids with ASYNC_MODULE_NOT_ASYNC).
    fn make_executor_with_async(module_id: &str) -> Arc<Executor> {
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
                _ctx: &Context<Value>,
            ) -> Result<Value, apcore::errors::ModuleError> {
                Ok(json!({}))
            }
        }

        let registry = Arc::new(Registry::default());
        let config = Arc::new(apcore::config::Config::default());

        let mut metadata = HashMap::new();
        metadata.insert("async".to_string(), json!(true));
        let descriptor = ModuleDescriptor {
            module_id: module_id.to_string(),
            name: None,
            description: "async dummy".to_string(),
            documentation: None,
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(ModuleAnnotations::default()),
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

        Arc::new(Executor::new(registry, config))
    }

    #[test]
    fn detects_async_metadata_bool() {
        let mut meta = HashMap::new();
        meta.insert("async".to_string(), json!(true));
        assert!(AsyncTaskBridge::is_async_module(&meta, None));
    }

    #[test]
    fn detects_async_metadata_string() {
        let mut meta = HashMap::new();
        meta.insert("async".to_string(), json!("true"));
        assert!(AsyncTaskBridge::is_async_module(&meta, None));
    }

    #[test]
    fn detects_mcp_async_in_extra() {
        let mut extra: HashMap<String, Value> = HashMap::new();
        extra.insert("mcp_async".to_string(), json!("true"));
        let meta = HashMap::new();
        assert!(AsyncTaskBridge::is_async_module(&meta, Some(&extra)));
    }

    #[test]
    fn non_async_module_returns_false() {
        let meta = HashMap::new();
        let ann = ModuleAnnotations::default();
        assert!(!AsyncTaskBridge::is_async_module(&meta, Some(&ann.extra)));
    }

    #[test]
    fn reserved_id_detection() {
        assert!(AsyncTaskBridge::is_reserved_id("__apcore_task_submit"));
        assert!(AsyncTaskBridge::is_reserved_id("__apcore_custom"));
        assert!(!AsyncTaskBridge::is_reserved_id("math.add"));
    }

    #[test]
    fn meta_tools_have_five_reserved_names() {
        let tools = AsyncTaskBridge::build_meta_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 5);
        assert!(names.contains(&META_TOOL_SUBMIT));
        assert!(names.contains(&META_TOOL_STATUS));
        assert!(names.contains(&META_TOOL_CANCEL));
        assert!(names.contains(&META_TOOL_LIST));
        assert!(names.contains(&META_TOOL_PREVIEW));
    }

    #[tokio::test]
    async fn submit_reserved_id_succeeds_direct() {
        // [D10-004] The reserved-id guard was moved from submit() to the
        // meta-tool handler. Calling submit() directly with a reserved id
        // must SUCCEED (no GeneralInvalidInput from submit itself).
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .submit("__apcore_task_submit", json!({}), None, None, None, None)
            .await;
        // The manager will reject it internally (unknown module), but that's
        // a different error path — NOT a GeneralInvalidInput from the bridge.
        // If it errors, it must NOT be the bridge's reserved-id check.
        if let Err(e) = &result {
            assert!(
                !e.to_string().contains("reserved by the async task bridge"),
                "submit() must not reject reserved ids; that belongs in handle_submit. Got: {e}"
            );
        }
    }

    #[tokio::test]
    async fn submit_returns_submit_result_with_pending_status() {
        // [D10-003] submit() must return SubmitResult{task_id, status:"pending"}
        // not the full TaskInfo.
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .submit("some.module", json!({}), None, None, None, None)
            .await
            .expect("submit should succeed");
        assert!(!result.task_id.is_empty(), "task_id must be non-empty");
        assert_eq!(result.status, "pending", "status must always be 'pending'");
    }

    #[tokio::test]
    async fn handle_meta_tool_submit_then_status() {
        let bridge = AsyncTaskBridge::new(make_executor_with_async("some.module"));
        let submit_result = bridge
            .handle_meta_tool(
                META_TOOL_SUBMIT,
                &json!({"module_id": "some.module", "arguments": {}}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool must be routed")
            .expect("submit should return Ok");
        let task_id = submit_result
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("task_id must be present");

        // Regression for [A-D-007]: submit envelope must be exactly
        // {task_id, status: "pending"} — not the full TaskInfo.
        assert_eq!(
            submit_result.get("status").and_then(|v| v.as_str()),
            Some("pending"),
            "submit envelope must report status=pending; got: {submit_result:?}"
        );
        // Pre-fix Rust serialised the full TaskInfo which leaks
        // module_id, submitted_at, etc. Confirm those are absent.
        assert!(
            submit_result.get("module_id").is_none(),
            "submit envelope must NOT contain module_id; got: {submit_result:?}"
        );
        assert!(
            submit_result.get("submitted_at").is_none(),
            "submit envelope must NOT contain submitted_at; got: {submit_result:?}"
        );

        let status_result = bridge
            .handle_meta_tool(
                META_TOOL_STATUS,
                &json!({"task_id": task_id}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool must be routed")
            .expect("status should return Ok");
        assert_eq!(
            status_result.get("task_id").and_then(|v| v.as_str()),
            Some(task_id)
        );
    }

    /// Regression test for [A-D-008].
    ///
    /// `__apcore_task_submit` against a module that is NOT async-hinted
    /// must return ASYNC_MODULE_NOT_ASYNC (per spec). Pre-fix Rust skipped
    /// this check and silently wrapped sync-only modules as async tasks.
    #[tokio::test]
    async fn handle_submit_rejects_non_async_module_with_async_module_not_async() {
        // Executor with NO async-hinted modules registered. The default
        // `make_executor()` already produces an empty registry, so we
        // expect the bridge to reject any module_id (since no module is
        // async-hinted, every module triggers the rule).
        let bridge = AsyncTaskBridge::new(make_executor());

        let result = bridge
            .handle_meta_tool(
                META_TOOL_SUBMIT,
                &json!({"module_id": "non_async.mod", "arguments": {}}),
                None,
                None,
                None,
                None,
            )
            .await;

        // handle_meta_tool returns Some(Err(...)) for known meta-tool with
        // an error.
        let inner = result.expect("meta-tool must be routed");
        let err = inner.expect_err("submit on non-async module must error");
        let msg = err.to_string();
        assert!(
            msg.contains("ASYNC_MODULE_NOT_ASYNC"),
            "error must surface ASYNC_MODULE_NOT_ASYNC code, got: {msg}"
        );
    }

    #[tokio::test]
    async fn handle_meta_tool_cancel_returns_flag() {
        let executor = make_executor();
        let bridge = AsyncTaskBridge::with_limits(executor, 0, 100); // 0 concurrency keeps tasks pending
        let submit = bridge
            .submit("m", json!({}), None, None, None, None)
            .await
            .expect("submit");
        let task_id = submit.task_id;
        let res = bridge
            .handle_meta_tool(
                META_TOOL_CANCEL,
                &json!({"task_id": task_id}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("routed")
            .expect("cancel ok");
        assert_eq!(res.get("cancelled").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn handle_meta_tool_list_filters_by_status() {
        let bridge = AsyncTaskBridge::with_limits(make_executor(), 0, 100);
        let _ = bridge
            .submit("m", json!({}), None, None, None, None)
            .await
            .unwrap();
        let res = bridge
            .handle_meta_tool(
                META_TOOL_LIST,
                &json!({"status": "pending"}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("routed")
            .expect("list ok");
        let tasks = res.get("tasks").and_then(|v| v.as_array()).unwrap();
        // The submitted task may still be Pending (0 concurrency) but
        // tokio scheduling may also race it to Running. Either way the
        // response must be an array.
        assert!(tasks.len() <= 1, "at most one task recorded");
    }

    #[tokio::test]
    async fn cancel_session_tasks_cancels_tracked_ids() {
        let bridge = AsyncTaskBridge::with_limits(make_executor(), 0, 100);
        let t1 = bridge
            .submit("m1", json!({}), None, None, None, Some("sess-a"))
            .await
            .unwrap();
        let t2 = bridge
            .submit("m2", json!({}), None, None, None, Some("sess-a"))
            .await
            .unwrap();
        // Task from another session should not be affected.
        let t3 = bridge
            .submit("m3", json!({}), None, None, None, Some("sess-b"))
            .await
            .unwrap();

        let cancelled = bridge.cancel_session_tasks("sess-a").await;
        assert_eq!(cancelled, 2);
        assert_eq!(
            bridge.get_status(&t1.task_id).map(|i| i.status),
            Some(TaskStatus::Cancelled)
        );
        assert_eq!(
            bridge.get_status(&t2.task_id).map(|i| i.status),
            Some(TaskStatus::Cancelled)
        );
        assert!(matches!(
            bridge.get_status(&t3.task_id).map(|i| i.status),
            Some(TaskStatus::Pending) | Some(TaskStatus::Running) | Some(TaskStatus::Failed)
        ));
    }

    #[tokio::test]
    async fn handle_meta_tool_unknown_returns_none() {
        let bridge = AsyncTaskBridge::new(make_executor());
        let res = bridge
            .handle_meta_tool("math.add", &json!({}), None, None, None, None)
            .await;
        assert!(res.is_none());
    }

    // -- [A-D-AT-3] empty-string ids are invalid input, not "not found" -----

    /// `as_str()` returns Some("") for an empty string, and no length check
    /// followed. Python and TypeScript both reject; on Rust an empty task_id
    /// returned ASYNC_TASK_NOT_FOUND and an empty module_id reached
    /// `executor.validate("", ..)` — an executor call the peers never make.
    #[tokio::test]
    async fn meta_tools_reject_empty_string_ids() {
        let bridge = AsyncTaskBridge::new(make_executor());
        let cases = [
            (META_TOOL_SUBMIT, json!({"module_id": "", "arguments": {}})),
            (META_TOOL_STATUS, json!({"task_id": ""})),
            (META_TOOL_CANCEL, json!({"task_id": ""})),
            (META_TOOL_PREVIEW, json!({"module_id": "", "arguments": {}})),
        ];
        for (tool, args) in cases {
            let result = bridge
                .handle_meta_tool(tool, &args, None, None, None, None)
                .await
                .expect("meta-tool must be routed");
            let err = result.expect_err(&format!("{tool} must reject an empty id"));
            assert_eq!(
                err.code,
                apcore::errors::ErrorCode::GeneralInvalidInput,
                "{tool} must report invalid input, not not-found"
            );
        }
    }

    // -- Issue D10-004: meta-tool handler still rejects __apcore_* ids -------

    #[tokio::test]
    async fn meta_tool_handler_rejects_reserved_id() {
        // [D10-004] The reserved-id check belongs in the meta-tool handler.
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .handle_meta_tool(
                META_TOOL_SUBMIT,
                &json!({"module_id": "__apcore_task_submit", "arguments": {}}),
                None,
                None,
                None,
                None,
            )
            .await;
        let inner = result.expect("meta-tool must be routed");
        let err = inner.expect_err("meta-tool submit must reject __apcore_ module_id");
        assert!(
            err.to_string().contains("reserved"),
            "error must mention reserved: {err}"
        );
    }

    // -- Issue D10-005: is_async_module_descriptor facade --------------------

    #[test]
    fn is_async_module_descriptor_detects_async_in_metadata() {
        // [D10-005] Primary public API takes the full descriptor.
        let mut metadata = HashMap::new();
        metadata.insert("async".to_string(), json!(true));
        let descriptor = apcore::registry::ModuleDescriptor {
            module_id: "test.module".to_string(),
            name: None,
            description: "test".to_string(),
            documentation: None,
            input_schema: json!({}),
            output_schema: json!({}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: None,
            examples: vec![],
            metadata,
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        assert!(AsyncTaskBridge::is_async_module_descriptor(&descriptor));
    }

    #[test]
    fn is_async_module_descriptor_returns_false_for_sync() {
        let descriptor = apcore::registry::ModuleDescriptor {
            module_id: "test.module".to_string(),
            name: None,
            description: "test".to_string(),
            documentation: None,
            input_schema: json!({}),
            output_schema: json!({}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: None,
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        assert!(!AsyncTaskBridge::is_async_module_descriptor(&descriptor));
    }

    // -- apcore 0.21 PROTOCOL_SPEC §5.6: __apcore_module_preview ------------

    #[tokio::test]
    async fn preview_meta_tool_returns_predicted_changes() {
        use apcore::module::{Change, PreviewResult};
        use async_trait::async_trait;

        #[derive(Debug)]
        struct PreviewableModule;

        #[async_trait]
        impl apcore::module::Module for PreviewableModule {
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn output_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn description(&self) -> &str {
                "previewable demo"
            }
            async fn execute(
                &self,
                _inputs: Value,
                _ctx: &Context<Value>,
            ) -> Result<Value, apcore::errors::ModuleError> {
                Ok(json!({}))
            }
            fn preview(
                &self,
                _inputs: &Value,
                _ctx: Option<&Context<Value>>,
            ) -> Option<PreviewResult> {
                let mut change = Change::default();
                change.action = "create".to_string();
                change.target = "row:42".to_string();
                change.summary = "insert row".to_string();
                let mut result = PreviewResult::default();
                result.changes = vec![change];
                Some(result)
            }
        }

        let registry = Arc::new(Registry::default());
        let config = Arc::new(apcore::config::Config::default());
        let descriptor = apcore::registry::ModuleDescriptor {
            module_id: "demo.preview".to_string(),
            name: None,
            description: "previewable demo".to_string(),
            documentation: None,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(apcore::module::ModuleAnnotations::default()),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry
            .register("demo.preview", Box::new(PreviewableModule), descriptor)
            .expect("register previewable module");

        let executor = Arc::new(apcore::executor::Executor::new(registry, config));
        let bridge = AsyncTaskBridge::new(executor);
        let result = bridge
            .handle_meta_tool(
                META_TOOL_PREVIEW,
                &json!({"module_id": "demo.preview", "arguments": {}}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool must be routed")
            .expect("preview should return Ok");
        assert_eq!(result.get("valid").and_then(|v| v.as_bool()), Some(true));
        let changes = result
            .get("predicted_changes")
            .and_then(|v| v.as_array())
            .expect("predicted_changes must be an array");
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].get("action").and_then(|v| v.as_str()),
            Some("create"),
        );
    }

    /// Stands in for a CLI-wrapping module: both its preflight warning
    /// and its predicted change name the resolved binary and the argv.
    #[derive(Debug)]
    struct DestructiveModule;

    #[async_trait::async_trait]
    impl apcore::module::Module for DestructiveModule {
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn output_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn description(&self) -> &str {
            "destructive demo"
        }
        async fn execute(
            &self,
            _inputs: Value,
            _ctx: &Context<Value>,
        ) -> Result<Value, apcore::errors::ModuleError> {
            Ok(json!({}))
        }
        fn preflight(&self, _inputs: &Value, _ctx: Option<&Context<Value>>) -> Vec<String> {
            vec!["This will run: /bin/cp src dst".to_string()]
        }
        fn preview(
            &self,
            _inputs: &Value,
            _ctx: Option<&Context<Value>>,
        ) -> Option<apcore::module::PreviewResult> {
            let mut change = apcore::module::Change::default();
            change.action = "overwrite".to_string();
            change.target = "/bin/cp src dst".to_string();
            change.summary = "This will run: /bin/cp src dst".to_string();
            let mut result = apcore::module::PreviewResult::default();
            result.changes = vec![change];
            Some(result)
        }
    }

    /// An executor holding `demo.destructive`, with an ACL that denies every
    /// caller.
    fn destructive_module_executor_denying_all() -> apcore::executor::Executor {
        destructive_module_executor("deny")
    }

    /// An executor holding `demo.destructive` behind a single ACL rule that
    /// applies `effect` to every caller. Shared by the denial tests and their
    /// allow-control so all of them observe the same real `PreflightResult`.
    fn destructive_module_executor(effect: &str) -> apcore::executor::Executor {
        let registry = Arc::new(Registry::default());
        let descriptor = apcore::registry::ModuleDescriptor {
            module_id: "demo.destructive".to_string(),
            name: None,
            description: "destructive demo".to_string(),
            documentation: None,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(apcore::module::ModuleAnnotations {
                requires_approval: true,
                ..Default::default()
            }),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry
            .register("demo.destructive", Box::new(DestructiveModule), descriptor)
            .expect("register destructive module");

        let acl = apcore::ACL::new(
            vec![apcore::ACLRule {
                callers: vec!["*".to_string()],
                targets: vec!["demo.destructive".to_string()],
                effect: effect.to_string(),
                // apcore 0.28.0 (apcore#108): `ACLRule` is not
                // `#[non_exhaustive]`, so every literal now names this field.
                approval: None,
                description: Some(format!("{effect} the destructive demo")),
                conditions: None,
            }],
            "allow",
            None,
        );
        let mut executor =
            apcore::executor::Executor::new(registry, Arc::new(apcore::config::Config::default()));
        executor.set_acl(acl);
        executor
    }

    #[tokio::test]
    async fn upstream_withholds_module_introspection_from_a_denied_caller() {
        // PROTOCOL_SPEC §12.8.5.1, implemented in apcore 0.27. This bridge no
        // longer filters its own envelope, so the security property is
        // upstream's to keep and this is the test that notices if it stops.
        // The check names are read from apcore's own `pub const`s, so a rename
        // upstream is a compile error here instead of a literal that quietly
        // stops matching — the failure mode the deleted runtime guard existed
        // to catch, now caught by the compiler.
        let executor = destructive_module_executor_denying_all();
        let preflight = executor
            .validate("demo.destructive", &json!({}), None)
            .await
            .expect("validate must return a structured result");

        let emitted: Vec<&str> = preflight.checks.iter().map(|c| c.check.as_str()).collect();

        // The denial itself still comes back, so a denied caller learns why.
        assert!(
            preflight
                .checks
                .iter()
                .any(|c| c.check == apcore::module::ACL_CHECK_NAME && !c.passed),
            "the failed acl check must survive the gate: {emitted:?}"
        );
        // Neither module-authored hook may have run, nor been reported.
        for withheld in [
            apcore::module::MODULE_PREFLIGHT_CHECK_NAME,
            apcore::module::MODULE_PREVIEW_CHECK_NAME,
        ] {
            assert!(
                !emitted.contains(&withheld),
                "§12.8.5.1: '{withheld}' must not be emitted for an ACL-denied \
                 caller — apcore ran module-authored introspection for a caller \
                 it had just denied, and this bridge no longer filters it out. \
                 Emitted: {emitted:?}"
            );
        }
        assert!(
            preflight.predicted_changes.is_empty(),
            "§12.8.5.1: predicted_changes must be empty for an ACL-denied caller"
        );
        // Control on the other half of the envelope decision: the gate does
        // NOT suppress `requires_approval`, because apcore resolves it from the
        // descriptor's annotations before the ACL check runs. `handle_preview`
        // reports it verbatim for exactly that reason.
        assert!(
            preflight.requires_approval,
            "requires_approval is a module property and survives a denial"
        );
    }

    #[tokio::test]
    async fn upstream_still_introspects_for_an_allowed_caller() {
        // Control for the test above. Without it, an apcore that stopped
        // running preflight()/preview() altogether would satisfy every denial
        // assertion for entirely the wrong reason, and this bridge would serve
        // an empty preview envelope to every caller with its suite still green.
        // Mirrors the control case in apcore's own
        // `conformance/fixtures/preflight_disclosure.json`, which exists for
        // this reason.
        let executor = destructive_module_executor("allow");
        let preflight = executor
            .validate("demo.destructive", &json!({}), None)
            .await
            .expect("validate must return a structured result");

        let emitted: Vec<&str> = preflight.checks.iter().map(|c| c.check.as_str()).collect();
        for expected in [
            apcore::module::MODULE_PREFLIGHT_CHECK_NAME,
            apcore::module::MODULE_PREVIEW_CHECK_NAME,
        ] {
            assert!(
                emitted.contains(&expected),
                "an allowed caller must still receive '{expected}': {emitted:?}"
            );
        }
        assert!(
            !preflight.predicted_changes.is_empty(),
            "an allowed caller must still receive predicted_changes"
        );
    }

    #[tokio::test]
    async fn preview_meta_tool_withholds_predicted_changes_when_acl_denies() {
        let executor = destructive_module_executor_denying_all();
        let bridge = AsyncTaskBridge::new(Arc::new(executor));
        let result = bridge
            .handle_meta_tool(
                META_TOOL_PREVIEW,
                &json!({"module_id": "demo.destructive", "arguments": {}}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool must be routed")
            .expect("preview should return Ok");

        // The denial itself is still reported, so the caller learns why.
        let checks = result
            .get("checks")
            .and_then(|v| v.as_array())
            .expect("checks must be an array");
        assert!(
            checks
                .iter()
                .any(|c| c.get("check").and_then(Value::as_str) == Some("acl")
                    && c.get("passed").and_then(Value::as_bool) == Some(false)),
            "the failed acl check must survive: {checks:?}"
        );

        // Nothing that names the resolved binary or the argv may survive it.
        assert_eq!(result.get("valid").and_then(Value::as_bool), Some(false));
        // `requires_approval` is deliberately NOT masked. It reports a property
        // of the module's annotations, not a fact about the denied call, and
        // apcore-python and apcore-typescript both pass it through — masking it
        // here made Rust the only SDK of three answering `false`.
        assert_eq!(
            result.get("requires_approval").and_then(Value::as_bool),
            Some(true),
            "requires_approval must pass through a denial, matching the peer SDKs"
        );
        assert_eq!(
            result
                .get("predicted_changes")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0),
            "predicted_changes must be withheld from a denied caller"
        );
        // Assert on the filter's actual effect, not only on the absence of a
        // string: `!contains("/bin/cp")` also holds when a check survives
        // with `passed: true, warnings: []`, so on its own it cannot tell a
        // working filter from one whose names have drifted out of match.
        let surviving: Vec<&str> = checks
            .iter()
            .filter_map(|c| c.get("check").and_then(Value::as_str))
            .collect();
        for withheld in [
            apcore::module::MODULE_PREFLIGHT_CHECK_NAME,
            apcore::module::MODULE_PREVIEW_CHECK_NAME,
        ] {
            assert!(
                !surviving.contains(&withheld),
                "'{withheld}' must be withheld from a denied preview: {surviving:?}"
            );
        }
        let rendered = serde_json::to_string(&result).expect("envelope serialises");
        assert!(
            !rendered.contains("/bin/cp"),
            "the resolved binary leaked into a denied preview: {rendered}"
        );
    }

    #[tokio::test]
    async fn preview_meta_tool_preserves_arguments_null() {
        // `arguments: null` and missing arguments must both reach
        // executor.validate as Value::Null — the calling business
        // decides whether null is acceptable. Pre-fix code coerced
        // missing → `{}` via `unwrap_or(json!({}))`.
        //
        // We assert by registering a module whose preflight chain
        // would emit a distinct check status when called with null
        // vs `{}`. The simplest proxy: register a module that
        // requires no inputs and verify validate returns valid=true
        // for both null and missing-arguments calls (i.e. neither
        // path errors with INVALID_INPUT).
        let bridge = AsyncTaskBridge::new(make_executor());

        // null arguments
        let result = bridge
            .handle_meta_tool(
                META_TOOL_PREVIEW,
                &json!({"module_id": "demo.preview", "arguments": null}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool routed")
            .expect("Ok envelope");
        // validate against an unregistered module returns valid=false
        // with a module_not_found check, NOT an arguments-shape error.
        // The important contract: Value::Null is accepted as valid
        // input shape by the bridge.
        assert!(result.get("valid").is_some());
        assert!(result.get("predicted_changes").is_some());

        // missing arguments — same path
        let result = bridge
            .handle_meta_tool(
                META_TOOL_PREVIEW,
                &json!({"module_id": "demo.preview"}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool routed")
            .expect("Ok envelope");
        assert!(result.get("valid").is_some());
    }

    #[tokio::test]
    async fn preview_meta_tool_rejects_array_arguments() {
        // Structurally-impossible shapes (array, scalar) must error.
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .handle_meta_tool(
                META_TOOL_PREVIEW,
                &json!({"module_id": "demo.preview", "arguments": [1, 2, 3]}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool routed");
        let err = result.expect_err("array arguments must error");
        assert!(err.to_string().contains("JSON object or null"));
    }

    #[tokio::test]
    async fn preview_meta_tool_rejects_missing_module_id() {
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .handle_meta_tool(
                META_TOOL_PREVIEW,
                &json!({"arguments": {}}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("meta-tool must be routed");
        let err = result.expect_err("missing module_id must error");
        assert!(err.to_string().contains("module_id"));
    }

    // -- Issue D11-014: handle_status not-found returns Ok(json) not Err -----

    #[tokio::test]
    async fn handle_status_unknown_task_returns_ok_json_error() {
        // [D11-014] Python returns _text_response({error:ASYNC_TASK_NOT_FOUND,task_id:...})
        // TS throws. Rust must return Ok(json) not Err, aligning with Python.
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .handle_meta_tool(
                META_TOOL_STATUS,
                &json!({"task_id": "nonexistent-task-id"}),
                None,
                None,
                None,
                None,
            )
            .await;
        let inner = result.expect("meta-tool must be routed");
        let val = inner.expect("handle_status must return Ok, not Err, for unknown task");
        assert_eq!(
            val.get("error").and_then(|v| v.as_str()),
            Some("ASYNC_TASK_NOT_FOUND"),
            "error field must be ASYNC_TASK_NOT_FOUND; got: {val:?}"
        );
        assert_eq!(
            val.get("task_id").and_then(|v| v.as_str()),
            Some("nonexistent-task-id"),
            "task_id must be echoed in the error envelope"
        );
    }

    // -- [D10-004] handle_cancel pre-checks task existence --------------------

    #[tokio::test]
    async fn handle_cancel_unknown_task_returns_not_found_envelope() {
        // [D10-004] Python `_handle_cancel_tool` checks `_manager.get_status(task_id) is None`
        // first and returns the ASYNC_TASK_NOT_FOUND envelope. Pre-fix Rust skipped the
        // existence check and silently returned `{cancelled: false}` for unknown ids —
        // callers couldn't distinguish "task existed but uncancellable" from "never existed".
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .handle_meta_tool(
                META_TOOL_CANCEL,
                &json!({"task_id": "ghost-task"}),
                None,
                None,
                None,
                None,
            )
            .await;
        let inner = result.expect("meta-tool must be routed");
        let val = inner.expect("handle_cancel must return Ok envelope, not Err, for unknown task");
        assert_eq!(
            val.get("error").and_then(|v| v.as_str()),
            Some("ASYNC_TASK_NOT_FOUND"),
            "error field must be ASYNC_TASK_NOT_FOUND; got: {val:?}"
        );
        assert_eq!(
            val.get("task_id").and_then(|v| v.as_str()),
            Some("ghost-task"),
            "task_id must be echoed in the error envelope"
        );
        // Must NOT silently report cancelled=false for unknown task
        assert!(
            val.get("cancelled").is_none(),
            "unknown-task envelope must not carry cancelled field; got: {val:?}"
        );
    }

    // -- [A-D-220] Progress sink fan-out -------------------------------------

    /// Build a SendNotificationFn that captures every notification into a
    /// shared Vec for assertion. Mirrors the test helper used by router.rs.
    fn make_capturing_sender() -> (
        crate::server::router::SendNotificationFn,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let sink = std::sync::Arc::clone(&captured);
        let sender: crate::server::router::SendNotificationFn =
            std::sync::Arc::new(move |val: Value| {
                let sink = std::sync::Arc::clone(&sink);
                Box::pin(async move {
                    sink.lock().unwrap().push(val);
                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                })
            });
        (sender, captured)
    }

    #[tokio::test]
    async fn submit_with_send_notification_records_progress_sink() {
        // [A-D-220] When submit() receives both progress_token and
        // send_notification, the bridge must store the sender so that
        // emit_progress(task_id, ...) can fan out a notifications/progress
        // message to the original MCP client. Pre-fix Rust ignored
        // send_notification entirely (the parameter did not exist).
        let bridge = AsyncTaskBridge::new(make_executor());
        let (sender, captured) = make_capturing_sender();

        let result = bridge
            .submit(
                "demo.module",
                json!({"k": "v"}),
                None,
                Some(json!("token-42")),
                Some(sender),
                None,
            )
            .await
            .expect("submit should succeed");

        // Now fan out a progress notification for this task.
        bridge
            .emit_progress(&result.task_id, 0.5, Some(1.0), Some("halfway"))
            .await;

        let notifications = captured.lock().unwrap();
        assert_eq!(
            notifications.len(),
            1,
            "exactly one notification must have been emitted; got {notifications:?}"
        );
        let notif = &notifications[0];
        assert_eq!(
            notif.get("method").and_then(|v| v.as_str()),
            Some("notifications/progress"),
            "method must be notifications/progress; got {notif:?}"
        );
        let params = notif
            .get("params")
            .and_then(|v| v.as_object())
            .expect("params must be an object");
        assert_eq!(
            params.get("progressToken").and_then(|v| v.as_str()),
            Some("token-42"),
            "progressToken must echo the submitted token"
        );
        assert_eq!(
            params.get("progress").and_then(|v| v.as_f64()),
            Some(0.5),
            "progress value must be passed through"
        );
        assert_eq!(
            params.get("total").and_then(|v| v.as_f64()),
            Some(1.0),
            "total value must be passed through"
        );
        assert_eq!(
            params.get("message").and_then(|v| v.as_str()),
            Some("halfway"),
            "message must be present when supplied"
        );
    }

    #[tokio::test]
    async fn emit_progress_no_op_when_no_sender_recorded() {
        // submit() without send_notification must NOT install a sink, and
        // emit_progress must be a silent no-op rather than panicking.
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .submit(
                "demo.module",
                json!({}),
                None,
                Some(json!("token-x")),
                None, // no sender
                None,
            )
            .await
            .expect("submit ok");

        // Should not panic; verifies the lookup-then-return-early branch.
        bridge.emit_progress(&result.task_id, 1.0, None, None).await;
    }

    #[tokio::test]
    async fn emit_progress_no_op_for_unknown_task_id() {
        let bridge = AsyncTaskBridge::new(make_executor());
        bridge
            .emit_progress("nonexistent-task", 0.5, Some(1.0), Some("noop"))
            .await;
        // success = no panic
    }

    #[tokio::test]
    async fn cancel_clears_progress_sender_to_avoid_leak() {
        // After cancel(), emit_progress must become a no-op for the
        // cancelled task — verifies progress_senders is cleaned up.
        let bridge = AsyncTaskBridge::with_limits(make_executor(), 0, 100);
        let (sender, captured) = make_capturing_sender();
        let result = bridge
            .submit(
                "m",
                json!({}),
                None,
                Some(json!("token-c")),
                Some(sender),
                None,
            )
            .await
            .expect("submit ok");

        let _cancelled = bridge.cancel(&result.task_id).await;

        bridge
            .emit_progress(&result.task_id, 0.9, Some(1.0), Some("after-cancel"))
            .await;

        let notifs = captured.lock().unwrap();
        assert!(
            notifs.is_empty(),
            "no notification should be emitted after cancel; got {notifs:?}"
        );
    }

    // -- [D11-101] handle_submit rejects non-object `arguments` ----------------

    #[tokio::test]
    async fn handle_submit_rejects_array_arguments() {
        // Pre-fix Rust accepted any shape for `arguments` and silently
        // collapsed to `{}` when it wasn't an object. Spec requires a JSON
        // object; arrays/scalars must produce GeneralInvalidInput.
        let bridge = AsyncTaskBridge::new(make_executor_with_async("some.module"));
        let result = bridge
            .handle_meta_tool(
                META_TOOL_SUBMIT,
                &json!({"module_id": "some.module", "arguments": [1, 2, 3]}),
                None,
                None,
                None,
                None,
            )
            .await;
        let inner = result.expect("meta-tool must be routed");
        let err = inner.expect_err("array arguments must produce an error");
        let msg = err.to_string();
        assert!(
            msg.contains("JSON object"),
            "error must mention JSON object requirement; got: {msg}"
        );
    }

    #[tokio::test]
    async fn handle_submit_accepts_null_arguments_as_empty_object() {
        // null / missing arguments collapse to `{}` per the existing
        // contract — only structurally-wrong shapes are rejected.
        let bridge = AsyncTaskBridge::new(make_executor_with_async("some.module"));
        let res = bridge
            .handle_meta_tool(
                META_TOOL_SUBMIT,
                &json!({"module_id": "some.module", "arguments": null}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("routed")
            .expect("submit must succeed for null arguments");
        assert!(res.get("task_id").and_then(|v| v.as_str()).is_some());
    }

    // -- [D11-102] handle_list rejects non-string `status` --------------------

    #[tokio::test]
    async fn handle_list_rejects_non_string_status_filter() {
        // Pre-fix Rust silently ignored `status: 5` (as_str() returned None
        // → no filter). Spec requires a string filter or absent/null.
        let bridge = AsyncTaskBridge::new(make_executor());
        let result = bridge
            .handle_meta_tool(
                META_TOOL_LIST,
                &json!({"status": 5}),
                None,
                None,
                None,
                None,
            )
            .await;
        let inner = result.expect("meta-tool must be routed");
        let err = inner.expect_err("numeric status filter must produce an error");
        assert!(
            err.to_string().contains("must be a string"),
            "error must mention string requirement; got: {err}"
        );
    }

    #[tokio::test]
    async fn handle_list_accepts_missing_or_null_status_filter() {
        let bridge = AsyncTaskBridge::new(make_executor());
        // missing status — must succeed
        let res = bridge
            .handle_meta_tool(META_TOOL_LIST, &json!({}), None, None, None, None)
            .await
            .expect("routed")
            .expect("missing status filter must succeed");
        assert!(res.get("tasks").is_some());
        // explicit null — must succeed
        let res = bridge
            .handle_meta_tool(
                META_TOOL_LIST,
                &json!({"status": null}),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("routed")
            .expect("null status filter must succeed");
        assert!(res.get("tasks").is_some());
    }

    // -- [D11-103] cancel awaits manager.cancel BEFORE dropping progress maps -

    #[tokio::test]
    async fn cancel_invokes_manager_cancel_before_dropping_progress_maps() {
        // [D11-103] Regression test for cancel() ordering. The bridge MUST
        // await `manager.cancel` first and only then drop the progress
        // token/sender maps so any terminal `notifications/progress`
        // emitted during the cancellation handshake can still reach the
        // client. Matches Python (async_task_bridge.py:458-460) and
        // TypeScript (async_task_bridge.ts:590-596).
        //
        // We verify the ordering by observing that emit_progress works
        // correctly up until cancel returns, after which the sender is
        // gone (cleanup happens last). A pre-fix ordering would clear
        // the sender BEFORE awaiting manager.cancel, so emit_progress
        // called inside the cancellation window would silently drop.
        let bridge = AsyncTaskBridge::with_limits(make_executor(), 0, 100);
        let (sender, captured) = make_capturing_sender();
        let result = bridge
            .submit(
                "m",
                json!({}),
                None,
                Some(json!("tok-1")),
                Some(sender),
                None,
            )
            .await
            .expect("submit ok");

        // Sanity: sender map populated before cancel.
        bridge
            .emit_progress(&result.task_id, 0.5, Some(1.0), Some("pre-cancel"))
            .await;
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "sender must be usable before cancel; got {:?}",
            captured.lock().unwrap()
        );

        let _ = bridge.cancel(&result.task_id).await;

        // After cancel, the sender map is dropped (cleanup completes).
        bridge
            .emit_progress(&result.task_id, 1.0, Some(1.0), Some("post-cancel"))
            .await;
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "post-cancel emit must be a no-op (sender already cleared); \
             got {:?}",
            captured.lock().unwrap()
        );
    }
}
