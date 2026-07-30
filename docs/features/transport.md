# Feature: Transport Manager

## Module Purpose
Manages MCP server transport modes: stdio, streamable-http, and SSE. Provides health and metrics HTTP endpoints for HTTP transports.

## Public API Surface

### MetricsExporter (trait)
- `export_prometheus() -> String`

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
- [ ] Supports extra routes and middleware injection
