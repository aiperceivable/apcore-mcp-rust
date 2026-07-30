# Feature: Explorer

## Module Purpose
Mounts the MCP Tool Explorer web UI for introspection and optional tool execution.

## Public API Surface

### Functions
- `create_explorer_mount(tools, router, allow_execute, explorer_prefix, authenticator, title, project_name, project_url) -> Mount`

## Acceptance Criteria
- [ ] Creates a web UI mount at the specified prefix
- [ ] Lists all available MCP tools with descriptions and schemas
- [ ] Carries each tool's MCP annotations through to the UI, so the hint badges — the destructive-tool warning in particular — render on the surface that also offers direct execution
- [ ] Supports optional tool execution when allow_execute=true
- [ ] Bridges auth via AUTH_IDENTITY context when authenticator is provided
- [ ] Configurable title, project name, and project URL
- [ ] Default prefix is /explorer
