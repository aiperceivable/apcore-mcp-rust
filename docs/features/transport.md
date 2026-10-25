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

## Stdio stdout ownership

The real stdio entry points reserve process stdout exclusively for MCP. They
duplicate the original stdout for protocol writes and redirect ordinary stdout
to stderr until startup, request handling, EOF draining and shutdown callbacks
finish. This includes application `println!`, apcore's `StdoutSubscriber` and
tracing subscribers configured by an embedding application to use stdout. The
CLI also configures its own tracing subscriber to write to stderr on all
transports. CLI help still uses stdout before serving begins. The existing
`--version <VERSION>` option sets the advertised server version.

The CLI acquires ownership before backend bootstrap. The free `serve` function
acquires it before constructing the bridge; instance `serve*` methods acquire
it before constructing server components. Output emitted by the caller before
entering these functions, including building an instance itself, remains the
caller's responsibility. `TransportManager::run_stdio` protects its transport
lifetime. `run_stdio_with_io` and explicitly selected HTTP/SSE transports leave
process streams unchanged. When the convenience function initially defaults
to stdio, it also protects Config Bus discovery until the effective transport
is known, then releases that temporary scope before starting HTTP/SSE.

Nested serving layers reuse their scope. An unrelated concurrent stdio server
or a second transport within the same scope returns an ownership error instead
of interleaving protocol streams. Normal completion and errors restore stdout;
RAII also restores it when the serving future is cancelled or unwinds. A normal
restoration failure is returned as an I/O error. Drop cannot return errors, so
it reports a failure on stderr and keeps ownership reserved if stdout could
not be restored. An unrecoverable restoration failure retains the saved
capture instead of closing a Windows handle that is still installed as stdout.

Server-owned disconnect callbacks, async task cancellation and approval-store
sweep tasks finish before normal stdout restoration. Request tokens retain
late module execution after a timeout response; the response remains prompt,
while server shutdown waits for actual cooperative cleanup. Dropping a serving
future signals its owned request tokens and retains stdout ownership until
their cleanup finishes. A module that ignores cooperative cancellation may
therefore keep shutdown waiting; a reported timeout does not force termination.

The complete async execution guarantee requires the accompanying unreleased
apcore Rust execution-draining fix. Published apcore 0.32.2 does not expose
`CancelToken::drain` or track spawned module lifetimes. This bridge continues
to compile against that declared minimum using a private compatibility
fallback; synchronous output isolation and sweep/callback ownership work,
but late module execution cannot be fully drained on the old core. Validation
of the enhanced guarantee uses a Cargo `patch.crates-io.apcore.path` override
to the fixed core checkout, with no checkout-specific path in this manifest.

On Unix this redirects descriptor 1, including native writes and inherited
child stdout. On Windows it redirects the Win32 standard output handle used by
Rust stdio, retaining an owned stderr duplicate and the borrowed original
handle through restoration. It does not rewrite separate CRT descriptors or
handles cached before entry. The Windows CI job is configured to exercise this
path; it has not been run as part of local macOS validation.
On either platform, code that explicitly retained a separate duplicate of the
original protocol stream can bypass redirection. Detached tasks must finish
before the serving lifetime ends; output after restoration follows ordinary
stdout again. These are process ownership boundaries, not a sandbox for
arbitrary native code.

`cargo run --example stdio_noise` provides an adversarial fixture with
`stdio_noise`, `stdio_error`, `stdio_wait` and `stdio_cooperative` tools. It exercises application prints, a
stdout tracing subscriber, apcore's stdout subscriber, joined background work
and startup/shutdown callbacks. `APCORE_STDIO_FIXTURE_MODE=cancel`, `timeout`
or `sweep` exercises cancellation of the serving future, prompt timeout with
late cooperative cleanup, or an owned approval sweep. Regression tests run global descriptor changes
in subprocesses so concurrent Rust tests retain their own stdout behavior.

## Structured Protocol Negotiation (Unreleased)

The server supports `2024-11-05`, `2025-03-26` and `2025-06-18`. An initialize
request for one of these receives the same version. Newer or unknown requests
receive `2025-06-18`, allowing the client to decide whether it supports that version.
Stdio and SSE remember the negotiated version on each live session, with
state removed on disconnect. Stateless HTTP uses the request's
`MCP-Protocol-Version` header, defaults to `2025-03-26` when it is absent, and
returns HTTP 400 for present malformed or unsupported versions on POST, GET,
and DELETE. Python and TypeScript delegate version negotiation to their native
MCP SDKs, which support additional revisions; the Rust supported set remains
`2024-11-05`, `2025-03-26` and `2025-06-18`. A different supported fallback is valid when the
requested revision is not implemented.

Transport-injected protocol and session metadata cannot be supplied by the
client. HTTP never accepts a caller-supplied session identity or acquires
elicitation support from a forged metadata object.

- [ ] Preserve the legacy response format on old connections and headerless HTTP requests
- [ ] Negotiate structured results independently for each live stdio/SSE session
- [ ] Reject unsupported or malformed HTTP protocol headers on all MCP endpoint methods
- [ ] Reject malformed JSON-RPC request versions, identifiers and non-object params before dispatch
- [ ] Ignore caller-supplied protocol/session metadata in favor of transport state

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
