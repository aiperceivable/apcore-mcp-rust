# Feature: Async Task Bridge

## Module Purpose
Routes async-hinted module calls through apcore's `AsyncTaskManager` and serves
the reserved `__apcore_*` meta-tools. Owns the submit/status/cancel/list
lifecycle, per-task progress fan-out, session-bound cancellation, and the
`__apcore_module_preview` dry-run envelope.

## Public API Surface

### AsyncTaskBridge
- `new(executor) -> AsyncTaskBridge`
- `with_limits(executor, max_concurrent, max_tasks) -> AsyncTaskBridge`
- `with_output_schemas(schemas) -> AsyncTaskBridge`
- `is_async_module_descriptor(descriptor) -> bool`
- `async submit(module_id, arguments, identity, progress_token, send_notification, session_key) -> SubmitResult`
- `async handle_meta_tool(name, arguments, identity, progress_token, send_notification, session_key) -> Option<Result<Value>>`
- `async cancel(task_id) -> bool`
- `async cancel_session_tasks(session_key) -> usize`
- `async emit_progress(task_id, progress, total, message)`
- `list_tasks(status) -> Vec<TaskInfo>`
- `async shutdown()`
- `build_meta_tools() -> Vec<Tool>`

### Meta-tool names
`__apcore_task_submit`, `__apcore_task_status`, `__apcore_task_cancel`,
`__apcore_task_list`, `__apcore_module_preview` — all under the reserved
`__apcore_` prefix.

## Acceptance Criteria
- [ ] Async-hinted modules (`metadata.async` or `annotations.extra.mcp_async`,
      case-insensitive) dispatch through the bridge; non-async modules submitted
      via `__apcore_task_submit` are rejected
- [ ] `__apcore_task_submit` returns `{task_id, status: "pending"}`
- [ ] `__apcore_task_status` redacts results against the module's output schema
- [ ] A submission carrying a `progressToken` and a notification sender fans out
      `notifications/progress` on progress and on terminal transitions
- [ ] A task submitted over a transport session is recorded under that session,
      and a client disconnect cancels every task recorded under it
- [ ] The session key is the transport's own id, never the caller's: a client
      cannot bind its tasks to, or cancel, another connection's session
- [ ] `__apcore_module_preview` refuses to introspect the `__apcore_` meta-tools
      themselves
- [ ] `__apcore_module_preview` preserves `arguments: null` verbatim and rejects
      array or scalar `arguments`
- [ ] **An ACL denial withholds `module_preflight`, `module_preview`,
      `predicted_changes` and `requires_approval`.** `Executor::validate` runs
      the module-level preflight and preview steps whenever module lookup
      succeeded, regardless of whether an earlier check failed, so for a
      CLI-wrapping module a denied caller would otherwise receive the resolved
      binary and the full argv. The failed `acl` check itself is kept, so the
      caller still learns why — a denial that discloses what was denied is not a
      denial
- [ ] The check names the disclosure filter matches on are verified against a
      real `PreflightResult`, so an upstream rename in apcore fails loudly
      instead of silently disabling the filter
