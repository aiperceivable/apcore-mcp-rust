<div align="center">
  <img src="https://raw.githubusercontent.com/aiperceivable/apcore-mcp/main/apcore-mcp-logo.svg" alt="apcore-mcp logo" width="200"/>
</div>

# apcore-mcp-rust

Automatic MCP Server & OpenAI Tools Bridge for apcore (Rust edition).

**apcore-mcp** turns any [apcore](https://github.com/aiperceivable/apcore)-based project into an MCP Server and OpenAI tool provider — with **zero code changes** to your existing project.

```
┌──────────────────┐
│  axum-apcore     │  ← your existing apcore project (unchanged)
│  project         │
└────────┬─────────┘
         │  extensions directory
         ▼
┌──────────────────┐
│  apcore-mcp-rust │  ← just install & point to extensions dir
└───┬──────────┬───┘
    │          │
    ▼          ▼
  MCP       OpenAI
 Server      Tools
```

## Design Philosophy

- **Zero intrusion** — your apcore project needs no code changes, no imports, no dependencies on apcore-mcp
- **Zero configuration** — point to an extensions directory, everything is auto-discovered
- **Pure adapter** — apcore-mcp reads from the apcore Registry; it never modifies your modules
- **Works with any apcore project** — if it uses the apcore Module Registry, apcore-mcp can serve it

## Documentation

For full documentation, including Quick Start guides, visit:
**[https://aiperceivable.github.io/apcore-mcp/](https://aiperceivable.github.io/apcore-mcp/)**

## Installation

### As a library

```sh
cargo add apcore-mcp
```

### As a CLI tool

```sh
cargo install apcore-mcp
```

Requires Rust 1.75+ and `apcore >= 0.21.0` + `apcore-toolkit >= 0.6.0`.

## Quick Start

### Zero-code approach (CLI)

If you already have an apcore-based project with an extensions directory, just run:

```sh
apcore-mcp --extensions-dir /path/to/your/extensions
```

All modules are auto-discovered and exposed as MCP tools. No code needed.

### Programmatic approach (Rust API)

The `APCoreMCP` builder is the recommended entry point — one object, all capabilities:

```rust
use std::sync::Arc;
use apcore::config::Config;
use apcore::executor::Executor;
use apcore::registry::registry::Registry;
use apcore_mcp::APCoreMCP;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create a registry and register your modules.
    let registry = Registry::new();
    // registry.register("my.tool", Box::new(MyModule), descriptor)?;

    // 2. Wrap it in an Executor — APCoreMCP requires an executor backend.
    let executor = Arc::new(Executor::new(registry, Config::default()));

    // 3. Build and serve.
    let mcp = APCoreMCP::builder()
        .backend(executor)
        .name("my-server")
        .transport("streamable-http")
        .port(8000)
        .build()?;

    // serve() is synchronous and blocks. It spawns its own Tokio runtime;
    // do NOT call it from inside an active runtime (use async_serve() for
    // embedded use, see API Overview below).
    mcp.serve()?;

    Ok(())
}
```

> **Backend note (v0.15.0):** The Rust SDK's `BackendSource::Executor` is the
> functional path. `BackendSource::ExtensionsDir` (string path) and
> `BackendSource::Registry` are reserved variants that currently return a
> `BackendResolution` error from `build()` — wrap your registry in an
> `Executor` first, as shown above.

<details>
<summary>Function-based API (still supported)</summary>

```rust
use apcore_mcp::{serve, to_openai_tools, ServeConfig, OpenAIToolsConfig};

// Pass an Arc<Executor> as the backend (same constraint as the builder API).
serve(executor.clone(), ServeConfig::default())?;

let tools = to_openai_tools(executor, OpenAIToolsConfig::default())?;
```
</details>

## Integration with Existing Projects

### Typical apcore project structure

```
your-project/
├── extensions/          ← modules live here
│   ├── image_resize/
│   ├── text_translate/
│   └── ...
├── src/main.rs          ← your existing code (untouched)
└── ...
```

### Adding MCP support

No changes to your project. Just run apcore-mcp alongside it:

```sh
# Install (one time)
cargo install apcore-mcp

# Run
apcore-mcp --extensions-dir ./extensions
```

Your existing application continues to work exactly as before. apcore-mcp operates as a separate process that reads from the same extensions directory.

### Adding OpenAI tools support

For OpenAI integration, a thin script is needed — but still **no changes to your existing modules**:

```rust
use apcore_mcp::{to_openai_tools, OpenAIToolsConfig};

let tools = to_openai_tools("./extensions", OpenAIToolsConfig {
    strict: true,
    ..Default::default()
})?;
// Use with the OpenAI API
```

## MCP Client Configuration

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "apcore": {
      "command": "apcore-mcp",
      "args": ["--extensions-dir", "/path/to/your/extensions"]
    }
  }
}
```

### Claude Code

Add to `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "apcore": {
      "command": "apcore-mcp",
      "args": ["--extensions-dir", "./extensions"]
    }
  }
}
```

### Cursor

Add to `.cursor/mcp.json` in your project root:

```json
{
  "mcpServers": {
    "apcore": {
      "command": "apcore-mcp",
      "args": ["--extensions-dir", "./extensions"]
    }
  }
}
```

### Remote HTTP access

```sh
apcore-mcp --extensions-dir ./extensions \
    --transport streamable-http \
    --host 0.0.0.0 \
    --port 9000
```

Connect any MCP client to `http://your-host:9000/mcp`.

### SSE transport (deprecated)

`--transport sse` mounts `GET /sse` and `POST /messages/`. Prefer
streamable HTTP; SSE remains for older clients.

Each `GET /sse` opens an **independent session**. The stream's first frame is
the MCP `endpoint` event, which carries the URL to post to:

```
event: endpoint
data: /messages/?sessionId=1f8c...  <- server-minted, per connection
```

`POST /messages/` **requires** that `sessionId` query parameter. It names the
stream that receives the response, so a client must read the `endpoint` event
before posting; posting to the bare `/messages/` URL returns `400`, as does
naming a session that is not open. A spec-compliant SSE client already does
this, and the TypeScript SDK behaves the same way.

Two consequences worth knowing:

- `params._meta.sessionId` is **overwritten** with the connection's own session
  id on every message. Any value a client sends there is ignored. This is what
  binds async tasks to the connection, so closing the stream cancels the tasks
  it launched — and what stops a client naming someone else's session.
- Closing the stream deregisters the session immediately. A message posted
  afterwards is rejected rather than silently absorbed.

## CLI Reference

```
apcore-mcp --extensions-dir PATH [OPTIONS]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--extensions-dir` | *(required)* | Path to apcore extensions directory |
| `--transport` | `stdio` | Transport: `stdio`, `streamable-http`, or `sse` |
| `--host` | `127.0.0.1` | Host for HTTP-based transports |
| `--port` | `8000` | Port for HTTP-based transports (1-65535) |
| `--name` | `apcore-mcp` | MCP server name (max 255 chars) |
| `--version` | package version | MCP server version string |
| `--log-level` | `INFO` | Logging: `DEBUG`, `INFO`, `WARNING`, `ERROR` |
| `--explorer` | off | Enable the browser-based Tool Explorer UI (HTTP only) |
| `--explorer-prefix` | `/explorer` | URL prefix for the explorer UI |
| `--allow-execute` | off | Allow tool execution from the explorer UI |
| `--jwt-secret` | — | JWT secret key for Bearer token auth (HTTP only) |
| `--jwt-key-file` | — | Path to PEM key file for JWT verification (e.g. RS256 public key) |
| `--jwt-algorithm` | `HS256` | JWT signing algorithm |
| `--jwt-audience` | — | Expected JWT audience claim |
| `--jwt-issuer` | — | Expected JWT issuer claim |
| `--jwt-require-auth` | on | Require valid token; use `--no-jwt-require-auth` for permissive mode |
| `--exempt-paths` | — | Comma-separated paths exempt from auth (e.g. `/health,/metrics`) |
| `--approval` | `off` | Approval handler: `elicit`, `auto-approve`, `always-deny`, or `off` |
| `--output-format` | `json` | Built-in output format: `json`, `csv`, or `jsonl` |

JWT key resolution priority: `--jwt-key-file` > `--jwt-secret` > `APCORE_JWT_SECRET` environment variable.

Exit codes: `0` normal, `1` invalid arguments, `2` startup failure.

## Rust API Reference

### `APCoreMCP` (recommended)

The unified entry point — configure once, use everywhere:

```rust
use apcore_mcp::APCoreMCP;

let mcp = APCoreMCP::builder()
    .backend(executor)                   // Arc<Executor> (the functional backend in v0.15.0)
    .name("apcore-mcp")                  // server name
    .version("1.0.0")                    // defaults to crate version
    .tags(vec!["public".into()])         // filter modules by tags
    .prefix("image")                     // filter modules by ID prefix
    .transport("streamable-http")        // "stdio" | "streamable-http" | "sse"
    .host("127.0.0.1")                  // host for HTTP transports
    .port(8000)                          // port for HTTP transports
    .validate_inputs(true)               // validate inputs against schemas
    .authenticator(auth)                 // Authenticator for JWT/token auth (HTTP only)
    .metrics_collector(collector)         // MetricsExporter for /metrics endpoint
    .output_formatter(formatter)         // custom result formatting
    .approval_handler(handler)           // approval handler for runtime approval
    .build()?;

// Launch as MCP server (synchronous, blocking; spawns its own Tokio runtime).
// Use `serve_with_options(ServeOptions { ... })` to pass on_startup/on_shutdown
// hooks, the explorer config, or `dynamic = true`.
mcp.serve()?;

// Export as OpenAI tools.
// The method takes (embed_annotations, strict) as positional booleans;
// use named-arg-style locals so the call reads consistently with the
// OpenAIToolsConfig form shown later in this guide.
let embed_annotations = false;
let strict = true;
let tools = mcp.to_openai_tools(embed_annotations, strict)?;

// Inspect
let tool_names = mcp.tools();     // list of module IDs
let registry = mcp.registry();    // underlying Registry
let executor = mcp.executor();    // underlying Executor
```

### `serve()` (function-based)

```rust
use apcore_mcp::{serve, ServeConfig};
use std::sync::Arc;

// As noted in the builder caveat above, BackendSource::ExtensionsDir
// (string path) currently returns a BackendResolution error. Build an
// Executor first and pass it as the backend.
let registry: Arc<apcore::Registry> = /* load extensions, e.g. via apcore::load_extensions("./extensions")? */;
let executor: Arc<apcore::Executor> = Arc::new(apcore::Executor::new(registry, apcore::Config::default()));

serve(executor, ServeConfig {
    transport: "streamable-http".into(),
    host: "127.0.0.1".into(),
    port: 8000,
    name: "apcore-mcp".into(),
    explorer: true,
    allow_execute: true,
    ..Default::default()
})?;
```

### `async_serve()`

Embed the MCP server into a larger application (e.g. co-host with other services):

```rust
use apcore_mcp::{AsyncServeOptions, ExplorerOptions};

let app = mcp.async_serve(AsyncServeOptions {
    // explorer is an ExplorerOptions struct, not a bool — set the
    // inner `explorer: true` flag to mount the Tool Explorer UI.
    explorer: ExplorerOptions {
        explorer: true,
        ..Default::default()
    },
    ..Default::default()
}).await?;
// Mount `app` (an axum::Router) into your own axum Router
```

### Tool Explorer

When `explorer.explorer = true` is passed via `ServeOptions`, a browser-based Tool Explorer UI is mounted on HTTP transports. It provides an interactive page for browsing tool schemas and testing tool execution.

```rust
use apcore_mcp::{ExplorerOptions, ServeOptions};

mcp.serve_with_options(ServeOptions {
    explorer: ExplorerOptions {
        explorer: true,
        allow_execute: true,
        ..Default::default()
    },
    ..Default::default()
})?;
// Open http://127.0.0.1:8000/explorer/ in a browser
```

**Endpoints:**

| Endpoint | Description |
|----------|-------------|
| `GET /explorer/` | Interactive HTML page (self-contained, no external dependencies) |
| `GET /explorer/tools` | JSON array of all tools with name, description, annotations |
| `GET /explorer/tools/<name>` | Full tool detail with inputSchema |
| `POST /explorer/tools/<name>/call` | Execute a tool (requires `allow_execute=true`) |

- **HTTP transports only** (`streamable-http`, `sse`). Silently ignored for `stdio`.
- **Execution disabled by default** — set `allow_execute=true` to enable Try-it.
- **Custom prefix** — use `explorer_prefix="/browse"` to mount at a different path.

### JWT Authentication

Optional Bearer token authentication for HTTP transports. Supports symmetric (HS256) and asymmetric (RS256) algorithms.

```rust
use apcore_mcp::JWTAuthenticator;

let auth = JWTAuthenticator::new("my-secret", None, None, None, None, None, None);

let mcp = APCoreMCP::builder()
    .backend(executor)  // Arc<Executor> — see Quick Start for setup
    .transport("streamable-http")
    .authenticator(auth)
    .build()?;
```

**Permissive mode** — allow unauthenticated access (identity is `None` when no token is provided):

```rust
let auth = JWTAuthenticator::new("my-secret", None, None, None, None, None, Some(false));
```

**Path exemption** — bypass auth for specific paths via CLI:

```sh
apcore-mcp --extensions-dir ./extensions --jwt-secret my-secret --exempt-paths /health,/metrics
```

### Approval Mechanism

Optional runtime approval for tool execution. Bridges MCP elicitation to apcore's approval system.

```rust
use apcore_mcp::ElicitationApprovalHandler;

let handler = ElicitationApprovalHandler::new(None);

let mcp = APCoreMCP::builder()
    .backend(executor)  // Arc<Executor> — see Quick Start for setup
    .approval_handler(Arc::new(handler))
    .build()?;
```

**Built-in handlers:**

| Handler | Description |
|---------|-------------|
| `ElicitationApprovalHandler` | Prompts the MCP client for user confirmation via elicitation |
| `AutoApproveHandler` | Auto-approves all requests (dev/testing only) |
| `AlwaysDenyHandler` | Rejects all requests (enforcement) |

CLI usage:

```sh
apcore-mcp --extensions-dir ./extensions --approval elicit
```

### Output Formatting

By default, tool execution results are serialized as JSON. You can customize this by passing an `output_format` name or a custom `output_formatter` closure.

**Built-in formats** (requires `apcore-toolkit` 0.7.0+):

```rust
// Via CLI
// apcore-mcp --extensions-dir ./extensions --output-format csv

// Via API
let mcp = APCoreMCP::builder()
    .backend(executor)
    .output_format(OutputFormat::Csv)
    .build()?;
```

Supports `json`, `csv`, and `jsonl`. Non-tabular data gracefully falls back to JSON.

**Custom formatter**:
Pass a closure that converts a `serde_json::Value` into a string.

### Extension Helpers

Modules can report progress and request user input during execution via MCP protocol callbacks. Both helpers no-op gracefully when called outside an MCP context.

```rust
use apcore_mcp::{report_progress, elicit};

// Inside a module's execute():
report_progress(&context, progress_cb.as_ref(), 50.0, Some(100.0), Some("Halfway done")).await;

let result = elicit(&context, elicit_cb.as_ref(), "Confirm deletion?", Some(&schema)).await;
if let Some(r) = result {
    if r.action == ElicitAction::Accept {
        // proceed
    }
}
```

### `/metrics` Prometheus Endpoint

When `metrics_collector` is provided, a `/metrics` HTTP endpoint is exposed that returns metrics in Prometheus text exposition format.

- **Available on HTTP-based transports only** (`streamable-http`, `sse`). Not available with `stdio` transport.
- **Returns Prometheus text format** with Content-Type `text/plain; version=0.0.4; charset=utf-8`.
- **Returns 404** when no `metrics_collector` is configured.

### `to_openai_tools()`

```rust
use apcore_mcp::{to_openai_tools, OpenAIToolsConfig};
use std::sync::Arc;

// Same backend constraint as serve(): pass an Arc<Executor>, not a
// path string (BackendSource::ExtensionsDir currently errors out).
let registry: Arc<apcore::Registry> = /* load extensions, e.g. via apcore::load_extensions("./extensions")? */;
let executor: Arc<apcore::Executor> = Arc::new(apcore::Executor::new(registry, apcore::Config::default()));

let tools = to_openai_tools(executor, OpenAIToolsConfig {
    embed_annotations: false,   // append annotation hints to descriptions
    strict: true,               // OpenAI Structured Outputs strict mode
    tags: Some(vec!["image".into()]),  // filter by tags
    prefix: None,               // filter by module ID prefix
})?;
```

**Strict mode** (`strict: true`): sets `additionalProperties: false`, makes all properties required (optional ones become nullable), removes defaults.

**Annotation embedding** (`embed_annotations: true`): appends `[Annotations: read_only, idempotent]` to descriptions.

**Filtering**: `tags` or `prefix` to expose a subset of modules.

## Features

- **Auto-discovery** — all modules in the extensions directory are found and exposed automatically
- **Display overlay** — `metadata["display"]["mcp"]` controls MCP tool names, descriptions, and guidance per module (§5.13)
- **Markdown tool descriptions** (`MCPServerFactory::with_rich_description(true)`, v0.15+) — render `Tool.description` as canonical apcore-toolkit Markdown so LLMs get more decision-relevant signal per token; backed by `apcore_toolkit::format_module(ModuleStyle::Markdown)`.
- **Module preview meta-tool** (`__apcore_module_preview`, v0.15+) — drives `executor.validate()` to predict state changes WITHOUT executing the module (apcore PROTOCOL_SPEC §5.6). Returns `{valid, requires_approval, predicted_changes, checks}` so AI orchestrators can ask "what would change?" before invoking.
- **Three transports** — stdio (default, for desktop clients), Streamable HTTP, and SSE
- **JWT authentication** — optional Bearer token auth for HTTP transports with `JWTAuthenticator`, permissive mode, PEM key file support, and env var fallback
- **Approval mechanism** — runtime approval via MCP elicitation, auto-approve, or always-deny handlers
- **AI guidance** — error responses include `retryable`, `ai_guidance`, `user_fixable`, and `suggestion` fields for agent consumption
- **AI intent metadata** — tool descriptions enriched with `x-when-to-use`, `x-when-not-to-use`, `x-common-mistakes`, `x-workflow-hints` from module metadata
- **Extension helpers** — modules can call `report_progress()` and `elicit()` during execution for MCP progress reporting and user input
- **Annotation mapping** — apcore annotations (readonly, destructive, idempotent) map to MCP ToolAnnotations
- **Schema conversion** — JSON Schema `$ref`/`$defs` inlining, strict mode for OpenAI Structured Outputs
- **Error sanitization** — ACL errors and internal errors are sanitized; stack traces are never leaked
- **Dynamic registration** — modules registered/unregistered at runtime are reflected immediately
- **Dual output** — same registry powers both MCP Server and OpenAI tool definitions
- **Tool Explorer** — browser-based UI for browsing schemas and testing tools interactively
- **Config Bus integration** — registers an `mcp` namespace with the apcore Config Bus; configure transport, host, port, and more via unified `apcore.yaml` or `APCORE_MCP_*` env vars
- **Error Formatter Registry** — registers an MCP-specific error formatter for ecosystem-wide consistent error handling

## Config Bus Integration

apcore-mcp registers an `mcp` namespace with the apcore Config Bus during `APCoreMCPBuilder::build()`. MCP settings can live alongside other apcore configuration in a single `apcore.yaml`:

```yaml
apcore:
  version: "1.0.0"
mcp:
  transport: streamable-http
  host: 0.0.0.0
  port: 9000
  explorer: true
  require_auth: false
```

Environment variable overrides use the `APCORE_MCP_` prefix:

```bash
APCORE_MCP_TRANSPORT=streamable-http
APCORE_MCP_PORT=9000
APCORE_MCP_EXPLORER=true
```

**Defaults:** `transport=stdio`, `host=127.0.0.1`, `port=8000`, `explorer=false`, `require_auth=true`.

The namespace, prefix, and defaults are also available as importable constants:

```rust
use apcore_mcp::{MCP_NAMESPACE, MCP_ENV_PREFIX, mcp_defaults, register_mcp_namespace};
```

## How It Works

### Mapping: apcore to MCP

| apcore | MCP |
|--------|-----|
| `module_id` | Tool name |
| `description` | Tool description |
| `input_schema` | `inputSchema` |
| `annotations.readonly` | `ToolAnnotations.readOnlyHint` |
| `annotations.destructive` | `ToolAnnotations.destructiveHint` |
| `annotations.idempotent` | `ToolAnnotations.idempotentHint` |
| `annotations.open_world` | `ToolAnnotations.openWorldHint` |

### Mapping: apcore to OpenAI Tools

| apcore | OpenAI |
|--------|--------|
| `module_id` (`image.resize`) | `name` (`image-resize`) |
| `description` | `description` |
| `input_schema` | `parameters` |

Module IDs with dots are normalized to dashes for OpenAI compatibility (bijective mapping).

### Architecture

```
Your apcore project (unchanged)
    │
    │  extensions directory
    ▼
apcore-mcp-rust (separate process / library call)
    │
    ├── MCP Server path
    │     SchemaConverter + AnnotationMapper
    │       → MCPServerFactory → ExecutionRouter → TransportManager
    │
    └── OpenAI Tools path
          SchemaConverter + AnnotationMapper + IDNormalizer
            → OpenAIConverter → Vec<Value>
```

## Development

```sh
git clone https://github.com/aiperceivable/apcore-mcp-rust.git
cd apcore-mcp-rust
make setup                       # install toolchain + pre-commit hook
make check                       # run all checks
cargo test                       # comprehensive test suite spanning unit + integration tests across server, auth, adapters, converters, helpers, async-task, explorer layers (~821 tests)
```

### Common Commands

| Command | Description |
|---------|-------------|
| `make check` | Run all checks (format, lint, chars, tests) |
| `make test` | Run all tests |
| `make lint` | Run Clippy with warnings-as-errors |
| `make fmt` | Auto-format code |
| `make clean` | Clean build artifacts |

## License

Apache-2.0
