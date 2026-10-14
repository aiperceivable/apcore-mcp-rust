# Feature: Server Factory

## Module Purpose
Creates and configures MCP servers from apcore registries. Builds MCP tool definitions from apcore module descriptors, registers request handlers, and produces initialization options.

## Public API Surface

### MCPServerFactory
- `new() -> MCPServerFactory`
- `create_server(name, version) -> Server`
- `build_tool(descriptor) -> Tool`
- `build_tools(registry, tags, prefix) -> Vec<Tool>`
- `register_handlers(server, tools, router)`
- `register_resource_handlers(server, registry)`
- `build_init_options(server, name, version) -> InitializationOptions`

## Acceptance Criteria
- [ ] Creates an MCP Server with correct name and version
- [ ] Converts apcore module descriptors to MCP Tool definitions
- [ ] Tool name uses module_id directly (dot-notation)
- [ ] Tool description includes AI intent metadata (x-when-to-use, x-when-not-to-use, etc.)
- [ ] Tool annotations map from apcore annotations (readOnlyHint, destructiveHint, etc.)
- [ ] Registers call_tool handler that delegates to ExecutionRouter
- [ ] Registers list_tools handler that returns all tools
- [ ] Registers resource handlers for modules with documentation
- [ ] Filters tools by tags when tags are specified
- [ ] Applies prefix to tool names when prefix is specified

## Structured Results (Unreleased)

For clients negotiated to MCP `2025-06-18`, synchronous modules with an
object output schema advertise `outputSchema`. Successful object results
include `structuredContent` and the existing serialized JSON text, both from
the router's already-redacted output. Validation uses the advertised schema
after redaction using the raw schema and its declared draft, with formats as
annotations. Local definitions are supported; external HTTP/file references
cannot be retrieved. A single Draft 7, 2019-09 or 2020-12 dialect is supported per advertised schema
(default 2020-12); canonical URIs with or without a trailing `#` are equivalent.
Embedded schema resources declaring a different dialect fail closed. Opaque
default/example/constant values are not schema resources. A mismatch becomes a payload-free tool error with no
result metadata. An advertised
schema requires the primary text item to contain a JSON object; an empty result
or a custom-formatted non-JSON primary item fails this check. Additional text
items, including trace diagnostics, remain intact. Modules with
non-object or unspecified schemas keep their existing text responses, and
async task submissions do not advertise the final module's output schema.

- [ ] Advertise object output schemas only for synchronous modules
- [ ] Preserve text and result/tool metadata while adding object structured results
- [ ] Omit structured data on tool errors and reject post-redaction schema mismatches
- [ ] Omit both structured wire fields for legacy `2024-11-05`/`2025-03-26` clients, including custom handlers

`Tool` and `CallToolResult` gain optional Rust fields. Existing struct literals
must initialize the new field to `None`; this requires a minor SDK release.
