# Feature: Transport Manager

## Module Purpose
Manages MCP server transport modes: stdio, streamable-http, and SSE. Provides health and metrics HTTP endpoints for HTTP transports.

## Public API Surface

### MetricsExporter (trait)
- `export_prometheus() -> String`

### McpSession / SessionRegistry (server::session)
The server-to-client half of the JSON-RPC channel. Supplies the `SessionHandle`
the router needs for elicitation; see "Planned: migration to rmcp" below.
- `OutboundSink::send(message)` — per-transport write of a server-initiated message
- `McpSession::new(id, sink)` / `with_elicit_timeout(d)` / `supports_elicitation()`
- `McpSession::try_take_response(message) -> bool` — consume a client answer
- `SessionRegistry::{insert, get, remove}` — session id to live connection

### TransportManager
- `new(metrics_collector) -> TransportManager`
- `set_module_count(count)`
- `async build_streamable_http_app(server, init_options, extra_routes, middleware) -> Starlette-equivalent`
- `async run_stdio(server, init_options)`
- `async run_streamable_http(server, init_options, host, port, extra_routes, middleware)`
- `async run_sse(server, init_options, host, port, extra_routes, middleware)`

## Acceptance Criteria
- [ ] stdio transport reads/writes MCP JSON-RPC over stdin/stdout
- [ ] streamable-http mounts MCP endpoint at /mcp
- [ ] SSE transport mounts at /sse and /messages/ (deprecated)
- [ ] Each GET /sse is an independent session: it emits an `endpoint` event carrying `/messages/?sessionId=<id>`, receives only the responses to messages posted for that id, and is deregistered on disconnect
- [ ] POST /messages/ requires the `sessionId` query parameter and returns 400 for a missing or unknown session
- [ ] HTTP transports auto-register GET /health endpoint
- [ ] HTTP transports auto-register GET /metrics endpoint (when MetricsExporter provided)
- [ ] Metrics endpoint returns Prometheus-format text
- [ ] Health endpoint returns 200 with module count
- [ ] stdio dispatches concurrently — a call awaiting an elicitation must not block the read loop
- [ ] stdio and SSE register each connection in the `SessionRegistry`; streamable HTTP deliberately does not
- [ ] A client that did not advertise `elicitation` in `initialize` is refused without a request being written
- [ ] A client answer to a server-initiated request is consumed before the handler sees it
- [ ] Disconnect deregisters the session and wakes anything awaiting an answer
- [ ] Supports extra routes and middleware injection

## Planned: migration to rmcp

**Decision (2026-08-17): this crate will move its transport layer onto the
official Rust MCP SDK, but not in the 0.18 cycle.**

`factory.rs` used to state that no Rust MCP SDK existed. That was true when the
project was scaffolded (2026-03-16); it is not true now. `rmcp`
(<https://github.com/modelcontextprotocol/rust-sdk>) is the official SDK from
the same organisation that publishes the Python and TypeScript SDKs this
project's sibling bridges already use. Rust is the only one of the three that
hand-rolls its transport.

rmcp 3.1.2 supplies everything `server::session` was written to provide, and
more: `RequestContext.peer` for server-initiated requests,
`Peer::elicit_with_timeout`, a structured `ElicitationError`
(`UserDeclined` / `UserCancelled` / `ParseError`), a `SessionManager` trait with
genuine per-connection sessions, `RequestContext.ct` for cancellation, and
`ServerHandler::{list_tools, call_tool}` that can be implemented by hand — so
the dynamic, registry-driven tool list this bridge needs is supported without
the `#[tool]` macro. `StreamableHttpService` is a `tower::Service` and mounts
into the existing axum app.

### What migrates

- `server/session.rs` — retired wholesale, replaced by `Peer`.
- `server/transport.rs` — stdio and streamable-HTTP dispatch retired.
- `server/server.rs` — the JSON-RPC dispatch in `ServerHandler` retired.
- `server/factory.rs` — `register_handlers` reimplemented as rmcp's `ServerHandler`.
- `server/router.rs` — `extract_extra` reads `RequestContext`; `SessionHandle` deleted.

### What stays hand-rolled

These are not MCP protocol concerns and rmcp does not replace them. They are
axum routes and layers that will keep sitting beside the rmcp service:

- `/health`, `/metrics`, `/usage` endpoints.
- Explorer mounting (`mcp-embedded-ui`).
- The JWT auth middleware — but identity moves from the `AUTH_IDENTITY`
  task-local to `RequestContext.extensions`, which is a behaviour change that
  feeds the apcore ACL and must be re-verified, not merely recompiled.
- Everything in the apcore integration layer: `ExecutionRouter`, ACL,
  redaction, trace propagation, `AsyncTaskBridge`, `ApprovalBridge`, and the
  `elicit_registry` + `Executor::call_typed` indirection. None of it is
  transport-coupled.

### Open questions to settle before starting

1. **SSE has no replacement.** rmcp ships `client-side-sse` only; there is no
   SSE server transport. `TransportKind::Sse`, `--transport sse` and
   `build_sse_app()` are public API. Either SSE is dropped (it is already
   `#[deprecated]` here) or its current hand-rolled implementation is kept
   alongside rmcp. This is a product decision, not a migration detail.
2. **apexe's API surface is unverified.** If it only uses `serve()` or the CLI
   the break is small; if it drives `TransportManager` directly it must change
   in step.

### Why not now

1. The elicitation support that landed in this cycle works; migrating would
   reimplement it, not improve it.
2. 0.18.0 already carries three breaking changes. Adding a transport rewrite
   would make the release impossible to assess.
3. **Cross-SDK conformance has no CI protection.** The 23 conformance tests in
   the Python and TypeScript bridges skip silently on every CI run. Rewriting
   this transport without that safety net means behavioural drift is caught by
   eye or not at all. Fixing conformance CI is a prerequisite, not a parallel
   task.

Rough estimate for someone who knows this codebase: 15-23 days if SSE is
dropped, 19-28 days if it is kept. Target 0.19 or 1.0.
