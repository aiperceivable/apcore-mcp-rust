# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **SSE transport delivered one client's responses to another client.** `build_sse_app` created a single process-global `mpsc` channel and shared its receiver across every connection behind an `Arc<Mutex<...>>`, so responses were handed out round-robin to whichever stream was next: with two clients connected, one client received the other's tool output. Each `GET /sse` now opens an independent session with its own inbound queue and its own event stream, keyed by a session id — as the TypeScript SDK's `runSse` (one `SSEServerTransport` per session id) and the Python SDK's `SseServerTransport.connect_sse` already did.
- **SSE dropped one message per past disconnect.** The shared-receiver consumer loop only exited after a failed send, so every disconnected client left a zombie consumer that silently swallowed exactly one later message while `POST /messages/` still answered `202 Accepted`. The per-connection consumer now also watches for the client going away and deregisters its session, and a message posted to a closed or unknown session is rejected rather than absorbed.
- **`__apcore_module_preview` disclosed the resolved binary and argv to a caller the ACL had denied.** Until apcore 0.26, `Executor::validate` reported the ACL verdict as a failed check and then still ran the module-level preflight and preview steps, so the envelope came back with `predicted_changes` naming the binary and the full argv for a module the caller was not allowed to reach. This bridge filtered the disclosure out by string. **apcore 0.27 fixes it at the source** (PROTOCOL_SPEC §12.8.5.1): `validate()` no longer invokes either hook, emits their checks, or populates `predicted_changes` when the `acl` check failed. The local filter is therefore **deleted** rather than kept as defence-in-depth — with the gate upstream it could never be observed to act, so no mutation of it could fail a test, and two guards where nobody can tell which is load-bearing is worse than one that is pinned. Two tests replace it: one asserts §12.8.5.1 against a real `Executor` (with an allow-control, so an apcore that stopped introspecting entirely cannot satisfy it for the wrong reason), and the end-to-end envelope test still asserts no argv reaches a denied caller. The three check names are now read from apcore's `pub const`s, so a rename upstream is a compile error rather than a filter that quietly stops matching.
- **The Explorer never showed a destructive-tool warning, for any tool.** `ToolInfo` carried only `name`, `description` and `inputSchema` and left `mcp_embedded_ui::Tool::annotations()` at its `None` default, so `GET /explorer/tools` and `GET /explorer/tools/{tool}` dropped the annotations that `tools/list` returns, and the UI's annotation render branch could never fire — on the one surface that also offers direct execution via `POST /explorer/tools/{tool}/call`. `ToolInfo` now carries `annotations` and `filter_explorer_tools` populates it. The Python bridge hands the MCP tool objects to the shared UI library verbatim and has always served them; this brings Rust in line.
- **SSE disconnect cancelled nothing.** `notify_cancel` fired with the server-minted session uuid, but `AsyncTaskBridge.session_tasks` is keyed from `params._meta.sessionId`, which only ever held what the client sent — so `cancel_session_tasks` matched zero tasks every time. A client could open `GET /sse`, POST `__apcore_task_submit`, close the stream, and the task still ran to completion holding an executor concurrency slot and performing its side effects. The transport now stamps its own session id onto every dispatched message (see Changed), so the ids match.
- **`GET /sse` emitted no bytes until the first message arrived.** It now emits the MCP `endpoint` event (`data: /messages/?sessionId=<id>`) immediately, so a spec-compliant SSE client can learn its POST URL, and holds the stream open with a 15-second keep-alive. This matches the streamable-HTTP `GET /mcp` handler.

### Added

- **Interactive approval works from a handler the router did not construct (apexe#29).** `--enable-approval` denied every gated call on a CLI-launched server: `ElicitationApprovalHandler` had no way to reach the connected client, so the human-in-the-loop path the docs describe did not exist. Two independent breaks stood in the way.

  **The callback was unreachable by type.** apcore hands an `ApprovalHandler` a `Context` whose `data` is `HashMap<String, serde_json::Value>`, and an `ElicitCallback` is a `Box<dyn Fn>` — not a `Value`. The router could only write the marker string `"available"`, which told the handler elicitation existed while giving it no way to perform one. The new crate-local `server::elicit_registry` turns that marker into a handle: the router registers the callback, receives an RAII guard holding an unguessable v4 uuid, and writes the id into `Context.data` under the new `MCP_ELICIT_CALL_ID_KEY`; the handler exchanges the id for the callback. This is the same per-request resolution apcore-python does at `adapters/approval.py`, one indirection wider — the limitation was Rust-only and needed no apcore change. The guard deregisters in `Drop`, which runs on the unwind path too, so a panicking tool call cannot leak an entry or leave a stale id resolvable.

  **The context was unreachable at all.** `ApcoreExecutorAdapter` passed `None` for the context to `apcore::Executor::call` / `call_with_trace`, so apcore fabricated an anonymous `@external` context and everything the router had written — the elicit id, and the resolved identity with it — was discarded before the pipeline ran. No amount of writing into `Context.data` could have worked while this held. The router's `Executor` trait gains `call_typed`, which takes the real `apcore::Context`; it is additive with a default returning `None`, so existing implementations keep compiling and a non-apcore executor keeps the JSON-only surface. Resolution order in the handler is live request, then constructor-injected callback, then rejection, so an embedding that injected its own callback is unchanged and a client that never declared elicitation support still gets today's clean fail-closed rejection.

  **Still missing: the transport cannot supply a session.** This closes the library half. `ExecutionRouter::handle_call_with_extra` — the only entry point that accepts a `CallExtra` carrying a session — has no production caller: the MCP `call_tool` handler goes through `handle_call`, whose `extract_extra` reads a plain JSON `extra` and returns `session: None` unconditionally. No production code implements `SessionHandle`. Serving a prompt over stdio or SSE needs a server-to-client JSON-RPC **request** with response correlation by id, which this crate's transports do not yet implement, plus capability detection from `initialize`. Until that lands, interactive approval is reachable by an embedder that supplies its own `SessionHandle` and not by `apcore-mcp serve`.

### Changed

- **Required `apcore` raised to `^0.27`, and bounded.** The dependency was `">=0.26"` — unbounded, so cargo was free to resolve any future release, including one whose breaking changes this crate had never been compiled against. Of the 0.27 breaking list, none of `ErrorCode::ConfigurationError`, `RedactionConfig`/`sensitive_keys`, `with_coerce_types`, `_config.strict`, `pipeline.configure`, `inject_checked`, `OtelTracing*` or `RefResolver` is referenced here, and the crate ships no apcore config file that could carry a newly-rejected key. What did break is **not in the 0.27 changelog**: `Registry::describe` changed from `-> String` to `-> Result<String, ModuleError>`, and its result changed shape with it — the generated markdown envelope instead of the module's one-line `description()`. Both call sites now read `descriptor.description`, the exact value 0.26 returned for a registered module. One of the two was a silent break the compiler could not catch: `build_registry_json` feeds the value to `json!`, and `ModuleError` derives `Serialize`, so `Result<String, ModuleError>` satisfies the macro and would have written `{"description": {"Ok": "..."}}` into the OpenAI converter's registry JSON.

- **BREAKING: `__apcore_module_preview` reports `requires_approval` verbatim on an ACL denial** instead of masking it to `false`. It describes the module's annotations, not the denied call, and apcore resolves it before the §12.8.5.1 gate. Masking it made this the only SDK of three answering `false` where apcore-python (`async_task_bridge.py`) and apcore-typescript (`async-task-bridge.ts`) both pass it through — an undocumented cross-SDK divergence, now retired.

- **BREAKING (source): `ExecutionRouter::build_context` and `build_context_with_trace` return a fourth element**, the elicit guard. It MUST be bound for the life of the dispatch (`let (.., _elicit_guard) = ...`); a bare `_` drops it immediately and deregisters the callback before the approval gate runs. A test pins that footgun.

- **BREAKING (wire): `POST /messages/` now requires a `sessionId` query parameter** naming the SSE stream that should receive the response; the value is advertised by that stream's `endpoint` event. A request without it, or naming an unknown session, returns `400`. Previously any POST was accepted and its response went to an arbitrary stream. **Every deployed SSE client that posts to the bare `/messages/` URL stops working** and must read the `endpoint` event first; a spec-compliant client already does. Matches the TypeScript SDK, which returns `400` in both cases. The two rejections carry distinct bodies, `missing sessionId query parameter...` and `unknown session` / `session closed`.
- **BREAKING (source): `explorer::ToolInfo` gained a public `annotations` field, is now `#[non_exhaustive]`, and must be built with `ToolInfo::new`.** The field addition alone already broke every downstream struct literal — all fields are `pub` and there was no constructor, so a literal was the only way to build one. Rather than break them again on the next field, the struct is sealed and gets `ToolInfo::new(name, description, input_schema)` plus `ToolInfo::with_annotations(value)`; future fields are additive. Migration: `ToolInfo { name, description, input_schema, annotations: Some(a) }` becomes `ToolInfo::new(name, description, input_schema).with_annotations(a)`. `serde` deserialisation of an older payload is unaffected — `annotations` defaults to `None`.
- **SSE messages now carry the transport's own session id.** Every message dispatched from an SSE connection has `params._meta.sessionId` overwritten with that connection's server-minted id before it reaches the handler, so `AsyncTaskBridge` binds submitted tasks to the connection and disconnect actually cancels them. The write is an overwrite, not a default: `notifications/cancelled` routes a caller-supplied key into `cancel_session_tasks`, so a client able to choose its own session id could cancel another connection's tasks. A client-supplied `_meta.sessionId` is now ignored; other `_meta` members such as `progressToken` are preserved. Matches the TypeScript SDK's `extra.sessionId` and the Python SDK's `transport_session_var`.
- **SSE messages within one session are now dispatched concurrently** (up to 16 in flight), where the consumer previously awaited each handler inline. A pipelined `ping` behind a 30-second `tools/call` waited the full 30 seconds, and so did disconnect detection, which delayed TM-4 cancellation by the same amount; a panicking handler also took the whole connection down with it. TypeScript already dispatched concurrently. **Responses may now arrive in an order other than the one the client posted in** — JSON-RPC correlates by `id`, and a client that assumed FIFO must not.
- **SSE client disconnect now invokes the transport's cancel handler** with the closing session id (TM-4), mirroring the TypeScript `transport.onclose` handler and the Python `_scoped_session` teardown. Teardown moved into a `Drop` guard so it also runs when the handler panics, where the previous straight-line cleanup was skipped by the unwind and leaked the registry entry for the lifetime of the process.

## [0.17.2] - 2026-07-14

Patch release. Fixes the MCP elicitation approval flow and bumps the required `apcore` floor to `0.26`.

### Fixed

- **`ElicitationApprovalHandler` now sends a non-empty elicitation `requested_schema`.** The approval elicitation was previously sent with an empty schema; clients that render an approval form (Cursor, Codex, ...) ignore or reject an empty schema, so the request returned no response and the gate failed closed. The handler now sends an object schema with a boolean `approve` field and honors an explicit `approve: false` from the form. Mirrors apcore-mcp (Python) 0.17.2.
- **Elicitation send failures now log at `warn`** (was `debug`) so a failing elicitation surfaces instead of silently denying.

### Changed

- **Required `apcore` floor raised to `0.26`** to align the ecosystem on the 0.26.0 governance layer.

## [0.17.1] - 2026-07-07
update package dependency version for apcore-toolkit (0.10.0) and increment project patch version

## [0.17.0] - 2026-06-23

Audit-driven hardening of the serve/embed entry points and the Phase B approval
chain, plus the apcore 0.25 / apcore-toolkit 0.9.1 dependency uplift.

### Fixed

- **Phase B approvals were end-to-end dead code**: the wrapped
  `StorageBackedApprovalHandler` was stored on the builder but never read by the
  serve path, the `__apcore_approval_check` meta-tool was never advertised, and
  `start_sweep()` was never started. `build_server_components` now instantiates
  `ApprovalBridge` from the configured handler, advertises the meta-tool, and the
  TTL sweep runs in both `serve_with_options` and `async_serve`. The stale
  "not yet wired" warning was removed (no silent-failure path remains).
- **`MCPServer` could not actually serve**: `RegistryOrExecutor` held an
  `Arc<dyn Any>` placeholder and `start()` only awaited shutdown. It now holds
  real `Arc<apcore::Registry>` / `Arc<apcore::Executor>`, registers tool/resource
  handlers, and drives the configured transport (stdio / streamable-HTTP / SSE)
  via `TransportManager`. The obsolete "until the apcore crate exposes the real
  traits" comment was removed.
- **`async_serve()` returned a Router with no `/mcp` route**: it now builds a
  `ServerHandler` and returns `build_streamable_http_app(...)` (which nests
  `/mcp`) instead of `health_metrics_router()`.
- **`strategy` was computed then dropped**: it is now resolved via
  `apcore::executor::resolve_strategy_by_name` and applied on the trace execution
  path (see Known limitations).

### Added

- `MCPServerConfig`, `ServeConfig`, and `AsyncServeConfig` gained `trace`,
  `strategy`, explorer branding (`explorer` / `explorer_prefix` /
  `explorer_title` / `explorer_project_name` / `explorer_project_url` /
  `allow_execute`), and `output_formatter`, aligning the framework-integration
  surface with the builder. `ServeConfig` / `AsyncServeConfig` also expose
  `approval_store` / `approval_notify` / `approval_handler`.
- `BackendSource::Registry` is now fully supported (shared into an `Executor`
  via `Arc`); `ApprovalStore::start_sweep` is now a trait method.
- Real per-connection SSE streaming on `GET /mcp` (endpoint event + keep-alive
  until disconnect), replacing the one-shot placeholder.

### Changed

- Raised apcore floor to `0.25` and apcore-toolkit to `0.9.1` (`Cargo.toml`).
- Documented the `redact_output: Option<bool>` tri-state (`None` = builder
  default enabled, `Some(false)` = explicitly disabled).

### Known limitations

- `strategy` overrides apply on the trace execution path; the non-trace `call()`
  path has no strategy-override parameter and still runs the executor's own
  strategy.
- `BackendSource::ExtensionsDir` builds an empty registry with a warning —
  runtime directory discovery cannot be honored through apcore's public API
  (`register_discovered` is private). Callers needing discovered modules should
  build a `Registry` / `Executor` with their own discoverer and pass it via
  `Registry` / `Executor`.

All 949 tests pass.


## [0.16.1] - 2026-06-18

### Changed
Remove the local duplicate CancelToken implementation and import the official type from apcore instead. Update all usage sites to use the imported token, add tests to verify the cancellation state propagates correctly into the apcore Context. This keeps the codebase in sync with apcore's official cancellation token and enables proper cross-component cancellation notifications.

## [0.16.0] - 2026-06-12

### Added

- **Approval Phase B: async polling via `__apcore_approval_check` meta-tool**.
  apcore-mcp now supports out-of-band human approvals that do not block the MCP connection.

  New public API:
  - `ApprovalStore` (trait) — pluggable persistence; three async methods:
    `save_pending`, `get_result`, `resolve`.
  - `InMemoryApprovalStore` — in-process implementation for testing/local dev.
    Bounded memory: per-record TTL via `tokio::spawn` + `time::sleep`, background
    sweep task via `start_sweep()`, and `max_records` hard cap with oldest-pending eviction.
    **Not suitable for production.**
  - `StorageBackedApprovalHandler` (implements `apcore::approval::ApprovalHandler`) —
    writes pending records on `request_approval()`, reads on `check_approval()`.
    Optional `notify_callback`.
  - `ApprovalBridge` — registers `__apcore_approval_check` as an MCP meta-tool,
    symmetric with `AsyncTaskBridge`.

Closes [issue #70](https://github.com/aiperceivable/apcore/issues/70): remove bridge-level `user_fixable` stamping now that apcore 0.24.0 resolves it at construction time via `user_fixable_for_code`.

### Changed

- **Raised apcore floor to `0.24` and apcore-toolkit to `0.8.1`** (`Cargo.toml`). apcore 0.24.0 auto-populates `user_fixable` on `ModuleError` at construction time; apcore-toolkit 0.8.1 declares the matching `apcore = "^0.24"` dependency (resolving the pre-1.0 semver conflict that blocked the upgrade from 0.8.0).
- **Removed bridge-level stamping** from `src/adapters/errors.rs`: deleted the `USER_FIXABLE_ERROR_CODES` constant, the `stamp_user_fixable` helper, and the two explicit match arms that applied it for `DependencyNotFound | DependencyVersionMismatch` and `BindingSchemaInferenceFailed | BindingSchemaModeConflict | BindingStrictSchemaIncompatible | VersionConstraintInvalid`. `user_fixable` now flows through the existing `attach_ai_guidance` path.
- **Adapted `Config::namespace()` call sites** (`src/config.rs`) to the apcore 0.24 API change: `namespace()` now returns `HashMap<String, serde_json::Value>` directly (no longer `Option`). Updated `get_pipeline_config`, `get_middleware_config`, `get_acl_config`, and the inner `read` closure in `get_scalar_config` accordingly. All 861 tests pass.

## [0.15.0] - 2026-05-29

Audit-driven consistency work from `/apcore-skills:audit --scope mcp`. Ten Rust-side fixes land here; the docs/spec repo (`apcore-mcp/`) remains at 0.15.0 because no spec contracts changed, so SDK versions also stay at 0.15.0 pending an explicit release decision. The entries below describe changes already committed on `main`.

### Changed

- **Upgraded required runtime to apcore 0.22.0 and apcore-toolkit 0.8.0** (`Cargo.toml`: `apcore = "0.22"`, `apcore-toolkit = "0.8"`). Aligned the bridge with two apcore 0.22.0 breaking API changes: `Registry::get_definition()` now returns `Result<Option<ModuleDescriptor>, ModuleError>` (all bridge call sites updated to treat a lookup error as "no descriptor"), and `Registry::list()` gained a third `visibility` filter argument (the bridge passes `None` to retain default public-only listing). Additionally, `Executor::call_with_trace()` gained `version_hint` (4th) and `strategy` (5th) parameters — the executor adapter now forwards the resolved `version_hint` (closing the long-standing "TODO(apcore>=0.19): forward version_hint") and leaves `strategy` at the executor default. No public API change; full suite green (861 lib + integration tests passed; clippy clean).

### Breaking Changes

- **[D9-002] Removed eight stub fields from `ServeConfig` and `AsyncServeConfig`.** The fields `schema_converter`, `annotation_mapper`, `error_mapper`, `on_startup`, `on_shutdown`, `metrics_collector`, `async_tasks`, and `async_max_concurrent` existed only for "Python parity" but were never read by `serve()` / `async_serve()`. Two of them (`on_startup`, `on_shutdown`) used `Option<serde_json::Value>` placeholders that could not accept callbacks, so users who wired them up silently got no effect. Lifecycle / observability callbacks remain wired via `ServeOptions` / `AsyncServeOptions`, which are the canonical surface.
- **[D10-002] `MCPServerFactory::create_server` now returns `Result<MCPServer, FactoryError>` and validates the server name.** Spec mandates non-empty, max 255 chars. Added `FactoryError::InvalidName(usize)` variant. Empty / oversized names now produce a typed error at the validation step instead of propagating into the MCP server constructor.

### Fixed

- **[D1-001] Exported `mcp_defaults` at crate root.** Previously `pub(crate) fn mcp_defaults` in `src/config.rs` was unreachable from external code, and the README cited an import path (`use apcore_mcp::mcp_defaults;`) that did not compile. Now exported alongside `MCP_NAMESPACE` / `MCP_ENV_PREFIX`. Closes cross-SDK parity gap with Python `MCP_DEFAULTS` and TypeScript `MCP_DEFAULTS`.
- **[D1-002] Exported `AsyncTaskBridge` and `META_TOOL_NAMES` at crate root.** Previously only `META_TOOL_PREFIX` was re-exported (as `APCORE_META_TOOL_PREFIX`); the struct and the 5-tuple of meta-tool name constants were reachable only via `apcore_mcp::server::async_task_bridge::*`. Cross-SDK parity with Python `__init__.py` and TypeScript `index.ts`.
- **[D10-005] `ElicitationApprovalHandler::request_approval` now unconditionally rejects when `request.context.is_none()`.** Previously Rust special-cased `is_none() && self.elicit.is_none()` and proceeded to invoke the constructor-injected callback when context was None but elicit was Some, contradicting Python and TypeScript which always reject on missing context. Security-gated `ApprovalHandler` surface — observable contract now matches peer SDKs.

### Refactored

- **[D9-006] Deleted `apcore_mcp::inspector` stub module.** TODO-only placeholder declared `pub(crate) mod inspector` from `lib.rs:21` with zero references in `src/`, `tests/`, or `examples/`. Will be re-created when F-039 implementation begins.
- **[D9-008] Moved `chrono` from `[dependencies]` to `[dev-dependencies]`.** Zero references in `src/`; only `examples/run/{main,modules}.rs` used it.
- **[D9-012] `json_entry_to_scanned_module` is now `pub(crate)` and removed from the crate root re-exports.** All non-test callers were inside `src/converters/openai.rs` itself; the symbol was accidentally surfaced in the public API.
- **[D2-001] Added `#![allow(clippy::upper_case_acronyms)]` at crate root** to silence the lint on intentionally-acronymed public types (`APCoreMCP`, `APCoreMCPError`, `APCoreMCPConfig`, `APCoreMCPBuilder`, `OpenAIToolsConfig`). Preserves cross-language brand consistency with Python/TypeScript while making clippy explicit about the policy.

### Tests

- **[D5-003] Added `tests/explorer_test.rs`** — five integration tests covering explorer mount registration and route wiring. Closes the per-SDK coverage gap relative to Python's `tests/explorer/...`.
- Total suite: **916 passed, 1 ignored**.

### Known Issues

- **[D9-011]** Audit flagged `OpenAIToolsConfig` as a potential empty parity shim; subsequent analysis showed the struct's four fields (`embed_annotations`, `strict`, `tags`, `prefix`) are actively read by `to_openai_tools`. Deferred pending audit re-evaluation.

Leverages **apcore 0.21.0 + apcore-toolkit 0.7.0**. Cross-SDK byte-
equivalent with `apcore-mcp-python` and `apcore-mcp-typescript` 0.15.0.

### Changed (BREAKING)

- **`apcore` minimum version bumped to `0.21`** (was `0.19`). Downstream
  consumers must pick up the async-ified `AsyncTaskManager.{submit,cancel,shutdown}`
  signatures (D10-003, D10-004), the `#[non_exhaustive]` annotation on
  `ApprovalRequest` / `ApprovalResult`, and the removal of
  `ApcoreErrorCode::BindingPolicyViolation` (the wire string
  `"BINDING_POLICY_VIOLATION"` is retained in
  `apcore_mcp::constants::ErrorCode` for backward-compat with legacy
  custom emitters).
- **`apcore-toolkit` minimum version bumped to `0.7`** (was `0.5`).
- **`AsyncTaskBridge::{submit, cancel, cancel_session_tasks, handle_meta_tool, handle_submit, handle_cancel, shutdown}` are now `async fn`** — propagates the upstream apcore 0.20+ async signatures. Sync transport-layer cancel handlers (the `Fn(&str)` closures installed via `transport_manager.set_cancel_handler` and `server_handler.with_cancel_handler`) now `tokio::spawn` the cancel call as fire-and-forget. The `progress_tokens` mutex guard is held in a tighter scope so the cancel future remains `Send` across the `.await` boundary.

### Added

- **Built-in output format support**: Added `--output-format` (`json`, `csv`, `jsonl`) to CLI and `output_format()` to `APCoreMCPBuilder`. Leverages `apcore-toolkit` 0.7 for standard tabular formatting.
- **`__apcore_module_preview` meta-tool** (apcore 0.21 PROTOCOL_SPEC §5.6 / §12.8) — fifth reserved meta-tool alongside the four `__apcore_task_*` ones. New `META_TOOL_PREVIEW` constant. The `handle_preview` async method drives `executor.validate(module_id, inputs, context)` and returns a `{valid, requires_approval, predicted_changes, checks}` JSON envelope WITHOUT executing the module. `arguments: null` and missing `arguments` are both preserved as `Value::Null` (the calling business decides whether null is acceptable); structurally-wrong shapes (arrays, scalars) error with `__apcore_module_preview requires 'arguments' to be a JSON object or null`. `MCPServerFactory::append_meta_tools` and `build_server_components` test counts updated to 5.
- **`MCPServerFactory::with_rich_description(bool)` constructor** + `rich_description()` accessor — when set, `build_tools` renders `Tool.description` as canonical apcore-toolkit Markdown (`format_module(ModuleStyle::Markdown)`) instead of `registry.describe()`. Display-overlay `mcp.description` overrides still win first. LLMs select tools primarily from this string; Markdown packs more decision signal per token.
- **`OpenAIConverter::convert_descriptor_with_options` / `convert_registry_with_options` / `convert_registry_apcore_with_options`** — accept a `ConvertOptions` struct (`embed_annotations`, `strict`, `rich_description`) so cross-cutting flags don't ratchet the positional signature of every public method. The original 5-positional-arg variants are retained as thin wrappers, no test breakage. The JSON path delegates to a new public `json_entry_to_scanned_module(module_id, entry)` adapter so duck-typed JSON registries can drive the same Markdown rendering path; the `&Registry` path uses `markdown::render_module_markdown(&descriptor, true)` directly to leverage the strictly-richer `ModuleDescriptor` (full `documentation`, `examples`, `display` overlay).
- **`apcore_mcp::markdown` module** — public helpers `descriptor_to_scanned_module(&ModuleDescriptor) -> ScannedModule` and `render_module_markdown(&ModuleDescriptor, display: bool) -> String` for crate users wanting to render Markdown directly.
- **`ApcoreErrorCode::CircuitBreakerOpen` mapping** (apcore 0.20 sync alignment A-001) — `ErrorMapper` now dispatches the breaker-open code to a retryable=true envelope with `ai_guidance` mirrored from the upstream error (or a generic recovery hint when absent). New `apcore_mcp::constants::ErrorCode::CircuitBreakerOpen` enum variant. Strum `EnumIter` count is now **36** (was 35); cross-language parity test (`all_python_error_codes_parse`) updated.
- **Public re-exports** in `lib.rs`: `markdown` module, `ConvertOptions`, `json_entry_to_scanned_module`.

### Fixed

- **`ApprovalResult` / `ApprovalRequest` construction** — adapted to apcore 0.21's `#[non_exhaustive]` annotations. Replaced struct-literal construction with `let mut x = X::default(); x.field = ...; x` pattern across `adapters/approval.rs` and the `apcore_mcp.rs` test stubs.
- **`adapters/errors.rs` match arm cleanup** — removed the now-deleted `ApcoreErrorCode::BindingPolicyViolation` variant from the binding-error match (apcore 0.21 dropped the variant; the wire code remains supported via the constants table).

### Tests

- +12 new tests covering `__apcore_module_preview` (registration, basic predict, missing module_id, `arguments: null` preserved, missing arguments preserved, array rejection), `CIRCUIT_BREAKER_OPEN` mapping (retryable + ai_guidance), `rich_description` on factory + JSON path + apcore-Registry path, and the `json_entry_to_scanned_module` adapter (overlapping fields, sparse defaults).
- Total suite: **843 passed** (was 828).


## [0.14.0] - 2026-05-01

Leverages apcore 0.19.0 + apcore-toolkit 0.5.0. Wires three apcore modules
that the bridge previously did not use: `trace_context`, `async_task`, and
`observability::{metrics,usage}`. Aligns the Rust bridge with the Python
and TypeScript 0.14.0 implementations.

### Fixed

- **Explorer UI hides `__apcore_*` meta-tools.** The four reserved F-043
  meta-tools (`__apcore_task_submit`, `__apcore_task_status`,
  `__apcore_task_cancel`, `__apcore_task_list`) are protocol-level
  operations meant for programmatic MCP clients; their multi-step
  submit/status flow does not fit the form-driven Explorer UX. Filter
  added in `src/apcore_mcp.rs::filter_explorer_tools`; reuses
  `META_TOOL_PREFIX` from `src/server/async_task_bridge.rs`. Aligns
  Rust with apcore-mcp-python (`__init__.py` builds a parallel
  `explorer_tools` list excluding meta-tools) and apcore-mcp-typescript
  (the `apcore-explorer-ui` package filters them client-side). The MCP
  `tools/list` response is unchanged — real MCP clients still discover
  and call meta-tools.

### Changed

- **`apcore` dependency bumped from `"0.17"` to `"0.19"`** (actual resolution jumps
  from 0.18.0 to 0.19.0).
- **New dependency: `apcore-toolkit = "0.5"`** — brings `BindingLoader` /
  `BindingLoadError` (pure-data reader for `.binding.yaml` with safety caps:
  16 MiB per file, 10 000 files per dir) and `ScannedModule.display` into the
  MCP bridge's dependency surface for downstream callers that need to hydrate
  modules from declarative bindings.
- **`ModuleDescriptor` struct literals** in `src/apcore_mcp.rs`,
  `src/server/factory.rs`, `src/server/listener.rs`, and `examples/run/main.rs`
  updated to supply the new `display: None` field (apcore 0.19.0 breaking
  change).
- **`APCoreMCPBuilder::build()` ACL install path** — apcore 0.19.0's
  `Executor::set_acl` takes `&mut self`. The builder now calls `Arc::get_mut`
  on the resolved executor; if the `Arc` is already shared (caller passed a
  clone), the builder returns a `Config` error pointing to the remediation
  (install ACL on the `Executor` before wrapping it in `Arc`). Affects the
  `BackendSource::Executor(Arc<Executor>) + .acl(...)` flow.
- **`ACL::check()` call site** in `tests/acl_conformance.rs` — returns `bool`
  directly in 0.19.0 (was `Result<bool, _>`).
- **Executor `acl()` accessor** — now a public field (`exec.acl`) rather than a
  method in apcore 0.19.0; updated in the two test assertions that inspected it.
- **`ExecutionRouter` state** carries `async_bridge: Option<Arc<_>>` and
  `async_module_ids: HashSet<String>`. Non-async paths are bit-for-bit
  unchanged when the bridge is not attached.

### Added

- **W3C Trace Context propagation** (P0). `src/server/router.rs` now imports
  `apcore::trace_context::{TraceContext, TraceParent}`, parses inbound
  `_meta.traceparent` on `tools/call` requests, and threads the resulting
  trace id through the `apcore::Context` so downstream module invocations
  inherit the W3C trace chain. Outbound tool responses carry
  `_meta.traceparent` built via `TraceContext::inject(context)`, so MCP
  clients can correlate spans across the bridge without bespoke plumbing.
  Malformed headers are rejected by apcore's strict validator (the bridge
  does not duplicate that logic).
- **Async Task Bridge** (`src/server/async_task_bridge.rs`, new, F-043 per
  `docs/features/async-task-bridge.md`). Exposes apcore's
  `AsyncTaskManager` through MCP so long-running modules can be submitted,
  polled, cancelled, and listed without blocking the transport.
  - `AsyncTaskBridge` struct with `is_async_module` (checks
    `metadata.async == true` OR `annotations.extra.mcp_async == "true"`),
    `submit`, `get_status`, `cancel`, `cancel_session_tasks`, `list_tasks`,
    `shutdown`, plus `is_reserved_id` and `is_async_registered` helpers.
  - Four reserved meta-tools registered under the `__apcore_task_` prefix:
    `__apcore_task_submit`, `__apcore_task_status`, `__apcore_task_cancel`,
    `__apcore_task_list`. `MCPServerFactory::build_tools` rejects any
    user-registered module id that collides with the reserved prefix.
  - `ExecutionRouter::with_async_bridge(bridge, async_ids)` installs the
    bridge; the router routes async-hinted module ids through
    `AsyncTaskManager::submit` instead of the synchronous executor path
    and returns a `{task_id, status: "pending"}` envelope immediately.
  - Progress fan-out: when the caller supplies `_meta.progressToken`,
    module-side `report_progress(context, ...)` calls flow through as
    MCP `notifications/progress` tied to the submitting session.
  - Status projection redacts sensitive fields via `redact_sensitive` using
    the router's `output_schemas` map so completed results respect the
    same schema-driven masking as the sync path.
  - `TaskLimitExceededError` (apcore 0.19.0) is routed through the
    existing error mapper with `retryable: true`.
- **TransportManager cancellation forwarding**
  (`src/server/transport.rs`). New `set_cancel_handler` /
  `notify_cancel(session_id)` hook. `APCoreMCPBuilder::async_serve` /
  `serve` wire the handler to `AsyncTaskBridge::cancel_session_tasks` so
  client disconnects cancel any tasks submitted from that session.
- **Observability auto-wiring** (P0). New `observability: bool` field on
  `APCoreMCPConfig` + `--observability` CLI flag + `.observability(true)`
  builder method.
  - When enabled, `APCoreMCPBuilder::build` auto-instantiates
    `apcore::observability::metrics::MetricsCollector` and
    `apcore::observability::usage::UsageCollector` and installs
    `MetricsMiddleware` + `UsageMiddleware` on the executor. The
    transport's `/metrics` endpoint (already exposed via the existing
    `MetricsExporter` Protocol) now has a real source out of the box.
  - Blanket `impl MetricsExporter for apcore::…::MetricsCollector` so the
    apcore collector plugs directly into the bridge's existing metrics
    surface without an adapter type.
  - New `UsageExporter` trait + blanket impl for apcore's `UsageCollector`.
    Adds `/usage` endpoint to `TransportManager` returning per-module
    summaries (call count, error count, latency, unique callers, trend)
    as JSON. Endpoint returns 404 when no usage exporter is configured.
  - A pre-instantiated custom `MetricsExporter` passed by the caller is
    preserved untouched — auto-wiring only kicks in for the
    `observability=true` / `metrics=true` zero-config path.
- **Type-safe error dispatch** — `src/adapters/errors.rs` now matches the
  new apcore 0.19.0 `ModuleError` variants (`TaskLimitExceeded`,
  `DependencyNotFound`, `DependencyVersionMismatch`,
  `BindingSchemaInferenceFailed`, `BindingSchemaModeConflict`,
  `BindingStrictSchemaIncompatible`, `BindingPolicyViolation`,
  `VersionConstraintInvalid`) with explicit arms instead of relying only
  on error-code string matches, tightening cross-language contracts.
- **8 new `ErrorCode` variants** surfacing apcore 0.19.0 protocol additions:
  `DependencyNotFound`, `DependencyVersionMismatch`, `TaskLimitExceeded`,
  `VersionConstraintInvalid`, `BindingSchemaInferenceFailed`,
  `BindingSchemaModeConflict`, `BindingStrictSchemaIncompatible`,
  `BindingPolicyViolation`. Total variants: 35 (was 27).
- **Dependency-error mapping in `ErrorMapper`** — `DependencyNotFound` and
  `DependencyVersionMismatch` now render a structured, agent-friendly message
  extracted from `details.module_id` / `dependency_id` / `required` / `actual`
  so MCP clients don't have to parse the detail bag.
- **Binding-configuration error routing** — `BindingSchema*` / `TaskLimitExceeded`
  / `VersionConstraintInvalid` are explicitly routed through `build_detail_response`
  (detail passthrough + AI guidance attachment) rather than hitting the default
  branch.
- **Expanded annotation surface in `AnnotationMapper::to_description_suffix`** —
  `cache_ttl`, `cache_key_fields`, `pagination_style` are now rendered into the
  `[Annotations: ...]` block when set to non-default values. `annotations.extra`
  keys prefixed with `mcp_` are passed through verbatim (F-041, previously
  blocked on apcore exposing `extra`).
- **Top-level `ModuleDescriptor.display` precedence** in `MCPServerFactory::build_tools`.
  The 0.19.0 descriptor adds a canonical `display: Option<Value>` field; it now
  takes precedence over the legacy `metadata["display"]` overlay (still honored
  for backwards compatibility).

### Tests

- **788 tests pass** (`cargo test --all-features`): 771 lib + 2 acl + 1
  adapters + 1 auth + 6 cli + 1 converters + 2 middleware + 1 server + 3
  doc. Up from 756 before this release.
- New unit coverage added inline under `#[cfg(test)]` in
  `src/server/async_task_bridge.rs` (hint detection, reserved-id
  rejection, submit/status/cancel/list, meta-tool schema, session
  cancellation), `src/server/router.rs` (traceparent parse + trace-id
  propagation + outbound `_meta.traceparent`), `src/server/transport.rs`
  (usage endpoint JSON shape, 404 without exporter, cancel handler
  invocation), and `src/apcore_mcp.rs` (observability flag auto-wires
  collectors; disabled path wires nothing; blanket `MetricsExporter` impl
  routes to `MetricsCollector::export_prometheus`).
- `error_code_count` guard updated: 27 → 35.
- `all_python_error_codes_parse` fixture extended with the 8 new canonical names.

### Cross-language sync (deferred-modules round, 2026-04-28)

- **Dependency bump**: `mcp-embedded-ui = "0.4"` (was `"0.3"`). The new release ships `POST /tools/{name}/validate` (F7) — read-only schema validation, ungated by `allow_execute`, `auth_hook`, or `Authenticator`. The route flows automatically through the existing `mcp_embedded_ui::create_mount` adapter in `src/explorer/mount.rs`. **Resolves EUI-1.** TC-011 integration tests added in `src/explorer/mount.rs::tests`.
- **OC-5 (BREAKING) — `OpenAIConverter::convert_registry` signature.** The canonical entrypoint now takes `&apcore::registry::Registry` directly (matching Python+TS duck-typed Registry input). The pre-fix `&serde_json::Value` snapshot variant is preserved as `convert_registry_json` for callers that hold a serialized snapshot:
  ```rust
  // Live registry path (preferred):
  converter.convert_registry(&registry, false, false, None, None)?;

  // Or keep using a JSON snapshot:
  converter.convert_registry_json(&value, false, false, None, None)?;
  ```
  `APCoreMCP::to_openai_tools` switched to the live-registry path, dropping the unused `build_registry_json` helper. 4 regression tests added.
- **AH-1 — per-request elicit callback via task-local.** Added `tokio::task_local! ELICIT_CALLBACK` in `apcore_mcp::helpers`. `ElicitationApprovalHandler::request_approval` now resolves the callback from the task-local first (matching Python+TS, which read it from `context.data`), with the constructor field as a fallback. apcore-rust's `Context::data` (`HashMap<String, serde_json::Value>`) cannot hold boxed `Fn`s, so a task-local is the closest cross-SDK equivalent without forcing an apcore-rust extension. 4 regression tests.
- **EM-3 — `userFixable=true` stamp** for `DependencyNotFound`, `DependencyVersionMismatch`, `VersionConstraintInvalid`, and the four `Binding*` codes (matches TS). Added `USER_FIXABLE_ERROR_CODES` const + stamp in `build_detail_response`. 5 regression tests.
- **EM-6 — generic-error fallback.** `ErrorMapper::internal_error_response()` and `ErrorMapper::to_mcp_error_any<E: std::error::Error>()` return the canonical `{is_error:true, error_type:"GENERAL_INTERNAL_ERROR", message:"Internal error occurred", details:null}` envelope for any non-`ModuleError` input — matches Python's `to_mcp_error(error: Exception)` and TypeScript's `toMcpError(error: unknown)`. 3 regression tests.
- **MID-5 — `ModuleIDNormalizer::denormalize_checked`.** Bijection-guarded variant validates the dash→dot-replaced result against the canonical module-id pattern, returning `Err(InvalidModuleId)` for inputs that aren't valid pre-images of `normalize`. Plain `denormalize` stays lenient. 5 regression tests.
- **SC-9 / SC-18** — strict-schema walker now stops descending into `enum` / `const` / `examples` / `default` and preserves `type: ["object", "null"]` (no longer downgrades to bare `"object"`). Output now matches Python+TS.
- **AM-L1 — F-041 annotation extras format aligned with Python+TS.** `mcp_*` extras are now emitted as separate `<stripped-key>: <value>` lines appended after the `[Annotations: ...]` block, separated by a single newline. Pre-fix Rust inlined them into the `[Annotations: ...]` block as `mcp_key=value`, which diverged from the other two SDKs on the wire. 1 regression test.

#### Deferred to a future release

- **A-D-012** — canonical strict-schema sourcing via `apcore::Registry::export_schema_strict` (committed locally as `62706be` but not yet on crates.io). 0.14.0 ships with the local-`SchemaConverter` fallback as the canonical path; behaviour is identical, the upgrade is purely about delegating to apcore upstream when the new release lands.
- **EB-2 (Rust)** — adapter-hook injection (`schema_converter` / `annotation_mapper` / `error_mapper` overrides on `serve()`). Blocked on `SchemaConverter` and `AnnotationMapper` being stateless unit structs with only static methods; needs a trait-based redesign first. Python+TS already ship the kwargs.

---

## [0.13.0] - 2026-04-06

### Added

- **Pipeline Strategy Selection** (F-036) — `--strategy` CLI flag and builder `.strategy()` with 5 presets (standard, internal, testing, performance, minimal).
- **Tool Output Redaction** (F-038) — `redact_output` config (default: true) applies `redact_sensitive()` before serialization.
- **Pipeline Observability** (F-037) — `.trace(true)` enables `call_with_trace()` for per-step timing.
- **Tool Preflight Validation** (F-039) — `ExecutionRouter::validate_tool()` for dry-run validation.
- **YAML Pipeline Configuration** (F-040) — Config Bus `mcp.pipeline` section via `build_strategy_from_config()`.
- **Annotation Metadata Passthrough** (F-041) — `mcp_` prefixed keys from annotations extra (behind feature flag).
- **4 new error mappings** — `CONFIG_ENV_MAP_CONFLICT`, `PIPELINE_ABORT`, `STEP_NOT_FOUND`, `VERSION_INCOMPATIBLE`.
- **RegistryListener wired to `dynamic` serve option**.

### Changed

- **Dependency bump**: `apcore = "0.17"` (was `"0.15"`).

---

## [0.12.0] - 2026-03-31

### Added

- **Config Bus namespace registration** (F-033) — Registers `mcp` namespace with apcore Config Bus (`APCORE_MCP` env prefix) during `APCoreMCPBuilder::build()`. MCP configuration (transport, host, port, auth, explorer) can be managed via unified `apcore.yaml`.
- **Error Formatter Registry integration** (F-034) — `McpErrorFormatter` registered with apcore's `ErrorFormatterRegistry`, formalizing MCP error formatting into the shared protocol.
- **Dot-namespaced event constants** (F-035) — `apcore_events` module with canonical event type constants from apcore 0.15.0 (§9.16).
- **6 new error code variants** — `ConfigNamespaceDuplicate`, `ConfigNamespaceReserved`, `ConfigEnvPrefixConflict`, `ConfigMountError`, `ConfigBindError`, `ErrorFormatterDuplicate`.

### Changed

- Dependency bump: requires `apcore 0.15.1` (was `0.14`) for Config Bus (§9.4), Error Formatter Registry (§8.8), and dot-namespaced event types (§9.16).

---

## [0.11.1] - 2026-03-29

### Added
- **Context.data callback injection** — `build_context()` now constructs a proper `apcore::Context<Value>` and injects MCP callback markers (`_mcp_progress`, `_mcp_elicit`) into `Context.data` (SharedData). Actual callbacks stored in a side-channel `HashMap<String, Box<dyn Any>>` since `serde_json::Value` cannot hold function pointers. Modules can detect callback availability via marker values.
- **Identity propagation** — `build_context()` resolves identity with a priority chain: `CallExtra.typed_identity` > deserialized JSON identity > `AUTH_IDENTITY` task-local from auth middleware. Resolved identity is used with `Context::new(identity)` or `Context::anonymous()`.
- **`redact_sensitive()` logging** — Added `tool_schemas` field and `with_tool_schemas()` builder method to `ExecutionRouter`. Tool inputs are redacted via `apcore::redact_sensitive()` before debug logging, replacing `x-sensitive: true` fields and `_secret_*` prefixed keys with `***REDACTED***`.
- **`CallExtra.typed_identity`** field for direct typed identity injection (bypasses JSON deserialization).
- 12 new tests: `build_context` identity resolution (4), callback marker injection (4), redact_sensitive (3), builder (1).

### Changed
- `build_context()` now returns a 3-tuple `(context_value, callback_data, apcore_context)` instead of a 2-tuple, providing the constructed `apcore::Context` for downstream use.
- JSON context `trace_id` is now taken from the `apcore::Context` for consistency.

- Bump apcore dependency from 0.13 to 0.14. All 694 tests pass without code changes — apcore 0.14 breaking changes (Context.identity optional, SharedData, middleware priority default 100) are backward-compatible for apcore-mcp.

## [0.11.0] - 2026-03-26

### Added
- **Display overlay in `build_tool()`** — MCP tool name, description, and guidance now sourced from `metadata["display"]["mcp"]` when present.
  - Tool name: `metadata["display"]["mcp"]["alias"]` (pre-sanitized by `DisplayResolver`, already `[a-zA-Z_][a-zA-Z0-9_-]*` and ≤ 64 chars).
  - Tool description: `metadata["display"]["mcp"]["description"]`, with `guidance` appended as `\n\nGuidance: <text>` when set.
  - Falls back to raw `descriptor.module_id` / `descriptor.description` when no display overlay is present.
- `build_tool()` now accepts `name_override` parameter for display overlay tool names.
- `build_tools_with_metadata()` method for resolving display overlay from module metadata.

### Changed
- Dependency recommendation: works best with `apcore-toolkit >= 0.4.0` for `DisplayResolver`.

### Fixed
- README: corrected `mcp.serve(Default::default())` to `mcp.serve()` (zero-argument method).
- README: updated apcore version requirement from `>= 0.13.0` to `>= 0.14.0`.
- docs/features: updated function signatures to use config structs (`ServeConfig`, `AsyncServeConfig`, `OpenAIToolsConfig`).

### Tests
- `TestBuildToolDisplayOverlay` (8 tests): MCP alias used as tool name, MCP description used, guidance appended to description, fallback when no overlay, fallback with empty metadata, name_override direct test, all fields combined.

## [0.10.1] - 2026-03-22

### Changed
- Rebrand: aipartnerup → aiperceivable

## [0.10.0] - 2026-03-17

### Added
- Initial project scaffolding: core modules, CLI, server, authentication, and comprehensive planning.
- **MCP server** with stdio, Streamable HTTP, and SSE transport support.
- **MCPServerFactory** for building tools, resources, and initialization options from an apcore registry.
- **ExecutionRouter** for dispatching tool calls with streaming, progress reporting, elicitation, and output formatting.
- **TransportManager** with health/metrics endpoints and Prometheus observability.
- **RegistryListener** for dynamic tool registration/unregistration via registry events.
- **JWTAuthenticator** with configurable claim mapping, algorithm selection, and key file support.
- **AuthMiddlewareLayer** (Tower layer) for HTTP request authentication with `AUTH_IDENTITY` task-local propagation.
- **Adapters**: AnnotationMapper, SchemaConverter (with `$ref`/`$defs` inlining), ErrorMapper, ModuleIDNormalizer, ElicitationApprovalHandler.
- **OpenAIConverter** for translating apcore registries to OpenAI function-calling format with strict mode support.
- **Explorer UI** powered by `mcp-embedded-ui` crate, with AuthBridge for identity propagation between apcore and the UI layer.
- **CLI** (`apcore-mcp`) with `--transport`, `--host`, `--port`, `--extensions-dir`, `--tags`, `--prefix`, and `--jwt-*` flags.
- **Helper functions**: `report_progress` and `elicit` for MCP progress notifications and user elicitation.
- **Constants**: `ErrorCode` and `RegistryEvent` enums with strum-based serialization matching the Python SDK wire format.
- **APCoreMCPBuilder** for fluent construction with backend, authenticator, metrics, output formatter, and approval handler.
- Convenience functions: `serve()`, `async_serve()`, `to_openai_tools()`.
- `Makefile` with `setup`, `check`, `test`, `lint`, `fmt`, and `clean` targets.
- Git pre-commit hook via `make setup` using `apdev-rs check-chars`.
- 671 tests across unit, integration, and doc-test suites.

### Changed
- `apcore` dependency switched from local path (`../apcore-rust`) to published crate (`apcore = "0.13"`).
- Explorer module refactored: hand-rolled `api.rs` and `templates.rs` replaced by `mcp-embedded-ui = "0.3"` crate with bridge adapters (`AuthBridge`, `wrap_call_fn`).
- `OutputFormatter` type alias uses `Box<dyn Fn>` (Send + Sync) for custom result formatting.
- `StreamResult` type alias introduced for `Pin<Box<dyn Stream<Item = Result<Value, ExecutorError>> + Send>>`.
- `ReadResourceHandler` type alias introduced for the read_resource handler closure.
- `ExecutionRouter::new_with_formatter()` constructor added for creating routers with pre-configured settings but no executor.

### Removed
- `src/explorer/api.rs` — ExplorerState, API handlers, and CallResponse (replaced by `mcp-embedded-ui`).
- `src/explorer/templates.rs` — HTML template rendering (replaced by `mcp-embedded-ui`).

[0.11.2]: https://github.com/aiperceivable/apcore-mcp-rust/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/aiperceivable/apcore-mcp-rust/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/aiperceivable/apcore-mcp-rust/compare/v0.10.1...v0.11.0
[0.10.1]: https://github.com/aiperceivable/apcore-mcp-rust/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/aiperceivable/apcore-mcp-rust/releases/tag/v0.10.0
